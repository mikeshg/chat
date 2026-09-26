// ACP 客户端测试：跑 tests/fake-acp-agent.mjs（node）。

use agent_host::acp::run_invocation;
use agent_host::runtime::{AgentSpec, RunEvent, RunOutput, RunRequest};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

fn script() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fake-acp-agent.mjs")
        .to_string_lossy()
        .to_string()
}

fn fake() -> AgentSpec {
    AgentSpec {
        id: Uuid::new_v4(),
        name: "fake".into(),
        command: "node".into(),
        args: vec![script()],
        cwd: String::new(),
        env: Vec::new(),
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
        registry: None,
    }
}

fn req(prompt: &str) -> RunRequest {
    runtime_req(fake(), prompt)
}

fn runtime_req(spec: AgentSpec, prompt: &str) -> RunRequest {
    RunRequest::for_invocation(spec, prompt.into(), std::env::temp_dir())
}

/// 带取消信号的请求（A04 的取消用例）
fn cancellable(spec: AgentSpec, prompt: &str, cancel: watch::Receiver<bool>) -> RunRequest {
    let mut r = runtime_req(spec, prompt);
    r.cancel = cancel;
    r
}

async fn run(prompt: &str) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    run_invocation(req(prompt), events).await
}

#[tokio::test]
async fn acp_echo() {
    let out = run("echo:hello world").await.expect("echo 应成功");
    assert_eq!(out.text, "hello world");
}

#[tokio::test]
async fn acp_fail() {
    let e = run("fail").await.expect_err("fail 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_crash() {
    let e = run("crash").await.expect_err("crash 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_long_chunks_concat() {
    let out = run("long:5000").await.expect("long 应成功");
    assert_eq!(out.text.len(), 5000);
}

#[tokio::test]
async fn acp_emits_text_events_in_order() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let out = run_invocation(req("echo:hello world"), events)
        .await
        .expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
}

#[tokio::test]
async fn acp_whoami_applies_config() {
    let mut spec = fake();
    spec.env = vec![("FAKE_ECHO".into(), "sekrit".into())];
    spec.instructions = "INST-a06".into();
    spec.model = "smart".into();
    spec.mode = "code".into();
    spec.extra_dirs = vec!["D:/extra".into()];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(runtime_req(spec, "whoami"), events)
        .await
        .expect("whoami 应成功");
    assert!(out.text.contains("mode=code"), "{}", out.text);
    assert!(out.text.contains("model=smart"), "{}", out.text);
    assert!(out.text.contains("FAKE_ECHO=sekrit"), "{}", out.text);
    assert!(out.text.contains("first=INST-a06"), "{}", out.text);
    assert!(out.text.contains("dirs=D:/extra"), "{}", out.text);
    assert_eq!(out.modes.as_ref().map(|m| m.len()), Some(2));
    assert_eq!(out.models.as_ref().map(|m| m.len()), Some(2));
}

#[tokio::test]
async fn acp_probe_reports_options_without_prompt() {
    let (events, _) = mpsc::unbounded_channel();
    let mut r = req("");
    r.probe = true;
    let out = run_invocation(r, events).await.expect("probe 应成功");
    assert!(
        out.text.is_empty(),
        "probe 不应发 prompt，正文应为空: {}",
        out.text
    );
    assert_eq!(out.modes.unwrap().len(), 2);
    assert_eq!(out.models.unwrap().len(), 2);
}

#[tokio::test]
async fn acp_ignores_mode_outside_options() {
    let mut spec = fake();
    spec.mode = "nope".into();
    spec.model = "nope".into();
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(runtime_req(spec, "whoami"), events)
        .await
        .expect("越界模式应忽略并继续");
    assert!(out.text.contains("mode=ask"), "{}", out.text);
    assert!(out.text.contains("model=fast"), "{}", out.text);
}

fn fake_caps(caps: &str) -> AgentSpec {
    let mut spec = fake();
    spec.args.push(format!("--caps={caps}"));
    spec
}

async fn invoke(
    spec: AgentSpec,
    prompt: &str,
    resume: Option<&str>,
) -> Result<(RunOutput, Vec<String>), String> {
    let (events, mut rx) = mpsc::unbounded_channel();
    let mut req = runtime_req(spec, prompt);
    req.resume_session = resume.map(str::to_string);
    let out = run_invocation(req, events).await?;
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    Ok((out, chunks))
}

#[tokio::test]
async fn acp_resume_recalls_prior_prompt() {
    let spec = fake_caps("resume");
    let (first, _) = invoke(spec.clone(), "echo:one", None)
        .await
        .expect("第一次应成功");
    let sid = first.session_id.clone().expect("新会话应有 session_id");
    assert_eq!(first.text, "one");
    let (second, chunks) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("恢复后 recall 应成功");
    assert_eq!(second.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(second.text, "echo:one");
    assert_eq!(chunks.concat(), second.text);
}

#[tokio::test]
async fn acp_load_filters_replay_and_keeps_session() {
    let spec = fake_caps("load");
    let (first, _) = invoke(spec.clone(), "echo:REPLAYME", None)
        .await
        .expect("第一次应成功");
    let sid = first.session_id.clone().expect("session_id");
    assert_eq!(first.text, "REPLAYME");
    let (second, chunks) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("load 后续接应成功");
    assert_eq!(second.session_id.as_deref(), Some(sid.as_str()));
    // 回放的旧回复是整段 "REPLAYME"；混进来会变成 "REPLAYMEecho:REPLAYME"
    assert_eq!(second.text, "echo:REPLAYME");
    assert_eq!(chunks.concat(), second.text);
    assert!(
        chunks.iter().all(|c| c != "REPLAYME"),
        "回放块不应进入事件: {chunks:?}"
    );
}

#[tokio::test]
async fn acp_without_session_caps_starts_fresh() {
    let spec = fake_caps("none");
    let (first, _) = invoke(spec.clone(), "echo:one", None)
        .await
        .expect("无能力时新会话应成功");
    let sid = first.session_id.clone().expect("session_id");
    let (second, _) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("退回新会话后调用仍应成功");
    assert_eq!(second.text, "(empty)");
    assert_ne!(second.session_id.as_deref(), Some(sid.as_str()));
    assert!(second.session_id.is_some());
}

#[tokio::test]
async fn acp_resume_error_falls_back_to_new_session() {
    let spec = fake_caps("resume");
    let (out, _) = invoke(spec, "recall", Some("missing-session"))
        .await
        .expect("恢复失败应退回新会话而不是报错");
    assert_eq!(out.text, "(empty)");
    assert!(out.session_id.is_some());
    assert_ne!(out.session_id.as_deref(), Some("missing-session"));
}

/// session/cancel 让 sleep 立刻以 cancelled 结束，不等满 sleep。
#[tokio::test]
async fn acp_cancel_stops_sleep() {
    let (tx, rx) = watch::channel(false);
    let (events, _) = mpsc::unbounded_channel();
    let handle = tokio::spawn(run_invocation(
        cancellable(fake(), "sleep:10000", rx),
        events,
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t0 = std::time::Instant::now();
    tx.send(true).unwrap();
    let out = tokio::time::timeout(Duration::from_secs(4), handle)
        .await
        .expect("合作取消应在 4s 内返回")
        .unwrap()
        .expect("取消路径不报错");
    assert!(t0.elapsed() < Duration::from_secs(4));
    assert_ne!(out.text, "done");
}

/// --ignore-cancel：通知无效，约 5 秒后进程被杀掉，调用返回。
#[tokio::test]
async fn acp_ignore_cancel_kills_after_grace() {
    let (tx, rx) = watch::channel(false);
    let (events, _) = mpsc::unbounded_channel();
    let mut spec = fake();
    spec.args.push("--ignore-cancel".into());
    let handle = tokio::spawn(run_invocation(cancellable(spec, "sleep:30000", rx), events));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t0 = std::time::Instant::now();
    tx.send(true).unwrap();
    let out = tokio::time::timeout(Duration::from_secs(8), handle)
        .await
        .expect("5s 宽限后应返回");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(8),
        "强制结束应约 5s，实际 {elapsed:?}"
    );
    match out.unwrap() {
        Ok(o) => assert_ne!(o.text, "done"),
        Err(_) => {}
    }
}
