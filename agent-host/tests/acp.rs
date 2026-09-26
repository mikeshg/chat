// ACP 客户端测试：跑 tests/fake-acp-agent.mjs（node）。

use agent_host::acp::run_invocation;
use agent_host::runtime::{AgentSpec, RunEvent, RunOutput, RunRequest};
use std::path::PathBuf;
use tokio::sync::mpsc;
use uuid::Uuid;

fn fake() -> AgentSpec {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-acp-agent.mjs");
    AgentSpec {
        id: Uuid::new_v4(),
        name: "fake".into(),
        command: "node".into(),
        args: vec![script.to_string_lossy().to_string()],
        cwd: String::new(),
        env: Vec::new(),
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
    }
}

fn req(prompt: &str) -> RunRequest {
    runtime_req(fake(), prompt)
}

fn runtime_req(spec: AgentSpec, prompt: &str) -> RunRequest {
    RunRequest {
        env: spec.env.clone(),
        instructions: spec.instructions.clone(),
        model: spec.model.clone(),
        mode: spec.mode.clone(),
        extra_dirs: spec.extra_dirs.clone(),
        spec,
        prompt: prompt.into(),
        data_dir: std::env::temp_dir(),
        probe: false,
    }
}

async fn run(prompt: &str) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    run_invocation(&req(prompt), events).await
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
    let out = run_invocation(&req("echo:hello world"), events)
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
    let out = run_invocation(&runtime_req(spec, "whoami"), events)
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
    let out = run_invocation(&r, events).await.expect("probe 应成功");
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
    let out = run_invocation(&runtime_req(spec, "whoami"), events)
        .await
        .expect("越界模式应忽略并继续");
    assert!(out.text.contains("mode=ask"), "{}", out.text);
    assert!(out.text.contains("model=fast"), "{}", out.text);
}
