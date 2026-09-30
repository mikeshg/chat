// Agent A07：真 agent 冒烟。默认跳过。
//
// 运行前提：本机已安装并登录 Claude Code 与 Codex，且 PATH 上有 Node.js（登记表
// 条目若是 npx 分发，测试会按登记表安装到临时数据目录）。
//
//   CHAT_REAL_AGENT_SMOKE=1 cargo test -p agent-host --test smoke -- --nocapture --test-threads=1
//
// 只跑其中一个：CHAT_REAL_AGENT_SMOKE=codex（或 claude）。
//
// 各发一次「只回复 pong」和一次往工作目录外写文件的指令（一定触发权限请求，冒烟里拒绝）。

use agent_host::acp::run_invocation;
use agent_host::registry::{self, Launch};
use agent_host::runtime::{AgentSpec, RunRequest};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

fn wanted(needle: &str) -> bool {
    match std::env::var("CHAT_REAL_AGENT_SMOKE").ok().as_deref() {
        Some("1") => true,
        Some(v) => v == needle,
        None => false,
    }
}

fn score(agent: &Value, needle: &str) -> i32 {
    let id = agent["id"].as_str().unwrap_or("").to_lowercase();
    let name = agent["name"].as_str().unwrap_or("").to_lowercase();
    if !id.contains(needle) && !name.contains(needle) {
        return -1;
    }
    let mut s = 1;
    if id == needle || id == format!("{needle}-code") || id == format!("{needle}-acp") {
        s += 10;
    }
    if name.contains(needle) {
        s += 3;
    }
    if agent.pointer("/distribution/npx").is_some()
        || agent.pointer("/distribution/binary").is_some()
    {
        s += 1;
    }
    s
}

fn pick<'a>(doc: &'a Value, needle: &str) -> Result<&'a Value, String> {
    let agents = doc["agents"]
        .as_array()
        .ok_or_else(|| "登记表没有 agents".to_string())?;
    agents
        .iter()
        .filter(|a| score(a, needle) >= 0)
        .max_by_key(|a| score(a, needle))
        .ok_or_else(|| format!("登记表里找不到 {needle}"))
}

fn spec_from(name: &str, launch: Launch, cwd: &str) -> AgentSpec {
    AgentSpec {
        id: Uuid::new_v4(),
        name: name.into(),
        command: launch.command,
        args: launch.args,
        cwd: cwd.into(),
        env: launch.env,
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
        registry: None,
        builtin: None,
        // 没有审批通道：超档请求会被取消，冒烟据此断言权限请求确实到了宿主
        permission_tier: "read_only".into(),
    }
}

async fn one(data: &PathBuf, url: &str, agent: &Value, work: &PathBuf) {
    let id = agent["id"].as_str().unwrap();
    let version = agent["version"].as_str().unwrap_or("");
    let name = agent["name"].as_str().unwrap_or(id);
    eprintln!("smoke: {name} id={id} version={version}");
    registry::install(data, url, id, version, |stage, err| {
        eprintln!("  install {stage} {}", err.unwrap_or(""));
    })
    .await;
    let launch = registry::resolve_launch(data, url, id, version)
        .await
        .unwrap_or_else(|e| panic!("{id} 未能解析成启动命令: {e}"));
    let cwd = work.display().to_string();

    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let pong = run_invocation(
        RunRequest::for_invocation(
            spec_from(name, launch.clone(), &cwd),
            "只回复 pong".into(),
            work.clone(),
        ),
        events,
    )
    .await
    .unwrap_or_else(|e| panic!("{id} pong 失败: {e}"));
    assert!(!pong.text.trim().is_empty(), "{id} pong 正文为空");

    // 工作目录外：Codex 的沙箱总允许写工作目录，只有目录外的写入一定会来问
    let outside = data.join(format!("outside-{id}"));
    std::fs::create_dir_all(&outside).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let perm = run_invocation(
        RunRequest::for_invocation(
            spec_from(name, launch, &cwd),
            format!("在目录 {} 里创建文件 smoke.txt", outside.display()),
            work.clone(),
        ),
        events,
    )
    .await
    .unwrap_or_else(|e| panic!("{id} 权限指令失败: {e}"));
    assert!(!perm.text.trim().is_empty(), "{id} 权限指令正文为空");
    assert!(
        perm.permissions_rejected >= 1,
        "{id} 应拒绝权限请求，实际 {}",
        perm.permissions_rejected
    );
    assert!(
        !outside.join("smoke.txt").exists(),
        "{id} 被拒后仍写出了文件"
    );
}

#[tokio::test]
async fn real_registry_agents_smoke() {
    if !wanted("claude") && !wanted("codex") {
        eprintln!("skip: 设置 CHAT_REAL_AGENT_SMOKE=1 才跑真 agent 冒烟");
        return;
    }
    let data = std::env::temp_dir().join(format!("chat-real-smoke-{}", std::process::id()));
    let work = data.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let url = agent_host::DEFAULT_REGISTRY_URL;
    let doc = registry::load_document_at(
        &registry::cache_path(&data),
        url,
        std::time::SystemTime::now(),
    )
    .await
    .expect("拉取登记表");
    for needle in ["claude", "codex"] {
        if wanted(needle) {
            let agent = pick(&doc, needle).expect(needle);
            one(&data, url, agent, &work).await;
        }
    }
}
