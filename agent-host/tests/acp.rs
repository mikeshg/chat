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
    }
}

async fn run(prompt: &str) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    let req = RunRequest {
        spec: fake(),
        prompt: prompt.into(),
        data_dir: std::env::temp_dir(),
        mcp_servers: Vec::new(),
    };
    run_invocation(&req, events).await
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
    let req = RunRequest {
        spec: fake(),
        prompt: "echo:hello world".into(),
        data_dir: std::env::temp_dir(),
        mcp_servers: Vec::new(),
    };
    let out = run_invocation(&req, events).await.expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
}
