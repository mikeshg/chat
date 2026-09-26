// ACP 客户端测试：跑 tests/fake-acp-agent.mjs（node）。

use agent_host::acp::{run_invocation, AgentSpec};
use std::path::PathBuf;
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

fn dir() -> PathBuf {
    std::env::temp_dir()
}

#[tokio::test]
async fn acp_echo() {
    let out = run_invocation(&fake(), "echo:hello world", &dir())
        .await
        .expect("echo 应成功");
    assert_eq!(out, "hello world");
}

#[tokio::test]
async fn acp_fail() {
    let e = run_invocation(&fake(), "fail", &dir())
        .await
        .expect_err("fail 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_crash() {
    let e = run_invocation(&fake(), "crash", &dir())
        .await
        .expect_err("crash 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_long_chunks_concat() {
    let out = run_invocation(&fake(), "long:5000", &dir())
        .await
        .expect("long 应成功");
    assert_eq!(out.len(), 5000);
}
