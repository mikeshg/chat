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
    }
}

fn req(spec: AgentSpec, prompt: &str, cancel: watch::Receiver<bool>) -> RunRequest {
    RunRequest {
        spec,
        prompt: prompt.into(),
        data_dir: std::env::temp_dir(),
        cancel,
    }
}

async fn run(prompt: &str) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    // 发送端留着：生产里取消通道在调用结束前一直开着
    let (_tx, rx) = watch::channel(false);
    run_invocation(req(fake(), prompt, rx), events).await
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
    let (_tx, cancel) = watch::channel(false);
    let out = run_invocation(req(fake(), "echo:hello world", cancel), events)
        .await
        .expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
}

/// session/cancel 让 sleep 立刻以 cancelled 结束，不等满 sleep。
#[tokio::test]
async fn acp_cancel_stops_sleep() {
    let (tx, rx) = watch::channel(false);
    let (events, _) = mpsc::unbounded_channel();
    let handle = tokio::spawn(run_invocation(req(fake(), "sleep:10000", rx), events));
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
    let handle = tokio::spawn(run_invocation(req(spec, "sleep:30000", rx), events));
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
