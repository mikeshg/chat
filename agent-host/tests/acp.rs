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
    };
    let out = run_invocation(&req, events).await.expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
}

// ===== Agent A03：tool_call / tool_call_update → RunEvent::Step* =====

/// steps:2 应按序产生 StepStart×2 + StepEnd(ok=true)×2，文本事件拼接等于完整输出。
#[tokio::test]
async fn acp_steps_emit_start_end_events() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let req = RunRequest {
        spec: fake(),
        prompt: "steps:2".into(),
        data_dir: std::env::temp_dir(),
    };
    let out = run_invocation(&req, events).await.expect("steps:2 应成功");
    let mut evs = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        evs.push(ev);
    }
    let starts: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::StepStart { id, title, kind } => {
                Some((id.clone(), title.clone(), kind.clone()))
            }
            _ => None,
        })
        .collect();
    let ends: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::StepEnd { id, ok } => Some((id.clone(), *ok)),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            (
                "step-1".to_string(),
                "步骤 1".to_string(),
                "other".to_string()
            ),
            (
                "step-2".to_string(),
                "步骤 2".to_string(),
                "other".to_string()
            ),
        ]
    );
    assert_eq!(
        ends,
        vec![("step-1".to_string(), true), ("step-2".to_string(), true)]
    );
    // 每个 StepStart 必须先于对应 StepEnd（按事件序）
    for id in ["step-1", "step-2"] {
        let si = evs
            .iter()
            .position(|e| matches!(e, RunEvent::StepStart { id: x, .. } if x == id));
        let ei = evs
            .iter()
            .position(|e| matches!(e, RunEvent::StepEnd { id: x, .. } if x == id));
        assert!(si < ei, "{id} 的 start 必须先于 end: {evs:?}");
    }
    let text: String = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, out.text);
    assert!(out.text.ends_with("done"));
}

/// stepfail：该步 StepEnd(ok=false)，调用本身仍成功（文本含 done）。
#[tokio::test]
async fn acp_stepfail_emits_failed_end() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let req = RunRequest {
        spec: fake(),
        prompt: "stepfail".into(),
        data_dir: std::env::temp_dir(),
    };
    let out = run_invocation(&req, events).await.expect("stepfail 应成功");
    let mut saw_fail = false;
    while let Ok(ev) = rx.try_recv() {
        if let RunEvent::StepEnd { id, ok } = ev {
            assert_eq!(id, "step-x");
            assert!(!ok, "step-x 应以失败结束");
            saw_fail = true;
        }
    }
    assert!(saw_fail, "应收到 StepEnd(ok=false)");
    assert!(out.text.contains("done"));
}
