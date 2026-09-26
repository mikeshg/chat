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
        resume_session: None,
    };
    run_invocation(&req, events).await
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
    let req = RunRequest {
        spec,
        prompt: prompt.into(),
        data_dir: std::env::temp_dir(),
        resume_session: resume.map(str::to_string),
    };
    let out = run_invocation(&req, events).await?;
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    Ok((out, chunks))
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
        resume_session: None,
    };
    let out = run_invocation(&req, events).await.expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
    assert!(out.session_id.is_some());
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
