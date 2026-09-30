// Agent A29：内置 agent（chat-builtin-agent 二进制）经 AcpRuntime 端到端测试。
//
// 假模型端点：本地 axum 起的 OpenAI Chat Completions SSE 服务，按脚本逐请求
// 出数据（文本 / tool_calls / 慢响应），并记录每次请求体供断言。
// 注入 MCP 走宿主真实的 McpServer + 一个假后端（同 mcp.rs 的形状）。

use agent_host::acp::run_invocation;
use agent_host::runtime::{
    AgentSpec, ApprovalLink, ApprovalRegistry, InjectedMcp, RunEvent, RunOutput, RunRequest,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

// ===== 假 OpenAI Chat Completions 端点 =====

/// 一次模型请求该回什么。
enum Step {
    /// 流一段文本，finish_reason=stop
    Text(String),
    /// 一轮工具调用，finish_reason=tool_calls
    Calls(Vec<(String, Value)>),
    /// 先睡 secs 秒再回文本（取消测试用）
    Slow(u64, String),
}

struct FakeModel {
    scripts: Mutex<VecDeque<Step>>,
    /// 收到的每个请求体（完整 JSON，含 messages/tools）
    seen: Mutex<Vec<Value>>,
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    )
}

fn sse_calls(calls: &[(String, Value)]) -> String {
    let deltas: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({
                "index": i, "id": format!("call_{i}"), "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            })
        })
        .collect();
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"tool_calls":deltas}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    )
}

async fn chat_completions(
    axum::extract::State(st): axum::extract::State<Arc<FakeModel>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    st.seen.lock().unwrap().push(req);
    let step = st
        .scripts
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Step::Text("(无脚本)".into()));
    let body = match step {
        Step::Text(t) => sse_text(&t),
        Step::Calls(calls) => sse_calls(&calls),
        Step::Slow(secs, t) => {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            sse_text(&t)
        }
    };
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

/// 起假端点，返回 (base_url, 记录器)。脚本按请求顺序消费。
async fn fake_model(scripts: Vec<Step>) -> (String, Arc<FakeModel>) {
    use axum::{routing::post, Router};
    let st = Arc::new(FakeModel {
        scripts: Mutex::new(scripts.into()),
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), st)
}

fn endpoints_json(base: &str) -> String {
    json!([{
        "id": "ep1", "name": "测试端点", "protocol": "chat",
        "base_url": base, "api_key": "k", "models": ["m-a", "m-b"],
    }])
    .to_string()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("a29-{tag}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 直接喂环境变量的 spec（不经 conn::materialize——那条路在 conn.rs 单测里覆盖）。
fn spec(work: &Path, state: &Path, endpoints: &str, tier: &str) -> AgentSpec {
    AgentSpec {
        id: Uuid::new_v4(),
        name: "内置".into(),
        command: env!("CARGO_BIN_EXE_chat-builtin-agent").to_string(),
        args: Vec::new(),
        cwd: work.to_string_lossy().to_string(),
        env: vec![
            ("CHAT_BUILTIN_ENDPOINTS".into(), endpoints.to_string()),
            (
                "CHAT_BUILTIN_STATE_DIR".into(),
                state.to_string_lossy().to_string(),
            ),
        ],
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
        registry: None,
        builtin: Some(json!({"endpoints": []})),
        permission_tier: tier.into(),
    }
}

async fn invoke(req: RunRequest) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    run_invocation(req, events).await
}

/// 记录 RunEvent 的调用（步骤断言用）。
fn invoke_events(req: RunRequest) -> (mpsc::UnboundedReceiver<RunEvent>, RunFuture_) {
    let (events, rx) = mpsc::unbounded_channel();
    (rx, tokio::spawn(run_invocation(req, events)))
}
type RunFuture_ = tokio::task::JoinHandle<Result<RunOutput, String>>;

// ===== 1. 纯文本回复 =====

#[tokio::test]
async fn builtin_plain_text_response() {
    let (base, _fake) = fake_model(vec![Step::Text("你好，内置 agent".into())]).await;
    let (work, state) = (tmp("w"), tmp("s"));
    let out = invoke(RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "full"),
        "打个招呼".into(),
        work.clone(),
    ))
    .await
    .expect("纯文本应成功");
    assert_eq!(out.text, "你好，内置 agent");
    assert!(out.session_id.is_some());
}

// ===== 2. session/new 模型清单 =====

#[tokio::test]
async fn builtin_reports_available_models() {
    let (base, _fake) = fake_model(vec![Step::Text("ok".into())]).await;
    let endpoints = json!([
        {"id":"ep1","name":"主站","protocol":"chat","base_url":base,
         "api_key":"k","models":["m-a","m-b"]},
        {"id":"ep2","name":"备用","protocol":"chat","base_url":base,
         "api_key":"k","models":["x"]},
    ])
    .to_string();
    let (work, state) = (tmp("w"), tmp("s"));
    let out = invoke(RunRequest::for_invocation(
        spec(&work, &state, &endpoints, "read_only"),
        "hi".into(),
        work.clone(),
    ))
    .await
    .expect("应成功");
    let models = out.models.expect("应报告可用模型");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["ep1/m-a", "ep1/m-b", "ep2/x"]);
    let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["主站 · m-a", "主站 · m-b", "备用 · x"]);
    // 内置 agent 不提供模式
    assert!(out.modes.is_none() || out.modes.as_deref() == Some(&[]));
}

// ===== 3. 工具链：write_file → read_file，workspace 档目录内自动放行 =====

#[tokio::test]
async fn builtin_workspace_tool_chain() {
    let (base, fake) = fake_model(vec![
        Step::Calls(vec![(
            "write_file".into(),
            json!({"path":"note.txt","content":"hi-a29"}),
        )]),
        Step::Calls(vec![("read_file".into(), json!({"path":"note.txt"}))]),
        Step::Text("读写完成".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    let (mut rx, task) = invoke_events(RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "workspace"),
        "写文件再读回来".into(),
        work.clone(),
    ));
    let out = task.await.unwrap().expect("工具链应成功");
    assert_eq!(out.text, "读写完成");
    // 文件真的写了
    assert_eq!(
        std::fs::read_to_string(work.join("note.txt")).unwrap(),
        "hi-a29"
    );
    // workspace 档 + 目录内路径：自动放行，无审批
    assert_eq!(out.permissions_rejected, 0);
    // 两步都记成功
    let steps: Vec<(&str, &str, bool)> = out
        .steps
        .iter()
        .map(|s| (s.title.as_str(), s.kind.as_str(), s.ok))
        .collect();
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert!(steps[0].0.contains("note.txt") && steps[0].1 == "edit" && steps[0].2);
    assert!(steps[1].0.contains("note.txt") && steps[1].1 == "read" && steps[1].2);
    // 第三次请求的最后一条 tool 消息带着读到的文件内容
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let tool_msg = seen[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .last()
        .expect("应有 tool 消息");
    assert!(
        tool_msg["content"].as_str().unwrap().contains("hi-a29"),
        "read_file 结果应回给模型: {tool_msg}"
    );
    // 事件流里 StepStart/StepEnd 配对
    let mut starts = 0;
    let mut ends = 0;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            RunEvent::StepStart { .. } => starts += 1,
            RunEvent::StepEnd { ok, .. } => {
                assert!(ok);
                ends += 1;
            }
            _ => {}
        }
    }
    assert_eq!((starts, ends), (2, 2));
}

// ===== 4. read_only 档：写入走审批，拒绝后不写文件、模型看到拒绝 =====

#[tokio::test]
async fn builtin_read_only_write_rejected() {
    let (base, fake) = fake_model(vec![
        Step::Calls(vec![(
            "write_file".into(),
            json!({"path":"nope.txt","content":"x"}),
        )]),
        Step::Text("被拒绝了".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    let mut req = RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "read_only"),
        "写个文件".into(),
        work.clone(),
    );
    // 审批通道：收到审批就拒绝
    let (tx, mut rx) = mpsc::unbounded_channel();
    let registry = ApprovalRegistry::new();
    req.approvals = Some(ApprovalLink {
        tx,
        registry: registry.clone(),
    });
    let rejecter = tokio::spawn(async move {
        let notice = rx.recv().await.expect("应发出审批");
        // 审批卡片：kind=edit、locations 带目录内绝对路径
        assert_eq!(notice.tool_call["kind"], "edit");
        assert!(notice.tool_call["locations"][0]
            .as_str()
            .unwrap()
            .contains("nope.txt"));
        assert!(registry.resolve(&notice.request_id, "reject_once".into()));
    });
    let out = invoke(req).await.expect("拒绝后调用仍应正常结束");
    rejecter.await.unwrap();
    assert_eq!(out.text, "被拒绝了");
    assert_eq!(out.permissions_rejected, 1);
    assert!(!work.join("nope.txt").exists(), "拒绝后文件不应存在");
    // 模型收到拒绝原因
    let seen = fake.seen.lock().unwrap();
    let tool_msg = seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(
        tool_msg["content"]
            .as_str()
            .unwrap()
            .contains("用户拒绝了这次操作"),
        "{tool_msg}"
    );
}

// ===== 5. shell：read_only 要审批；full 自动跑且带回退出码 =====

#[tokio::test]
async fn builtin_shell_needs_approval_in_read_only() {
    let marker = "shell_ran.txt";
    let cmd = format!("echo ran > {marker}");
    let (base, _fake) = fake_model(vec![
        Step::Calls(vec![("shell".into(), json!({"command": cmd}))]),
        Step::Text("over".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    // 无审批通道（read_only 默认拒绝）
    let out = invoke(RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "read_only"),
        "跑命令".into(),
        work.clone(),
    ))
    .await
    .expect("调用应正常结束");
    assert_eq!(out.permissions_rejected, 1);
    assert!(!work.join(marker).exists(), "shell 未放行不应执行");
}

#[tokio::test]
async fn builtin_shell_runs_in_full_and_reports_exit_code() {
    let cmd = if cfg!(windows) {
        "echo hi-a29& exit /b 0"
    } else {
        "echo hi-a29; exit 0"
    };
    let (base, fake) = fake_model(vec![
        Step::Calls(vec![("shell".into(), json!({"command": cmd}))]),
        Step::Text("跑完了".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    let out = invoke(RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "full"),
        "跑命令".into(),
        work.clone(),
    ))
    .await
    .expect("full 档应自动放行 shell");
    assert_eq!(out.permissions_rejected, 0);
    // 输出含退出码：第二次请求的 tool 消息里有
    let seen = fake.seen.lock().unwrap();
    let tool_msg = seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    let content = tool_msg["content"].as_str().unwrap();
    assert!(content.contains("hi-a29"), "{content}");
    assert!(content.contains("退出码 0"), "{content}");
}

// ===== 6. 会话续接：第二次调用走 session/load，模型看到历史 =====

#[tokio::test]
async fn builtin_session_continues_with_history() {
    // 共用一个 state 目录：第一次调用存档，第二次拉起新进程 load 回来
    let (base, fake) = fake_model(vec![
        Step::Text("记住了 ALPHA-9".into()),
        Step::Text("接着回答".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    let make_spec = || spec(&work, &state, &endpoints_json(&base), "full");

    let first = invoke(RunRequest::for_invocation(
        make_spec(),
        "记住 ALPHA-9".into(),
        work.clone(),
    ))
    .await
    .expect("第一次应成功");
    let sid = first.session_id.clone().expect("应回 session_id");

    let mut req = RunRequest::for_invocation(make_spec(), "继续".into(), work.clone());
    req.resume_session = Some(sid.clone());
    let second = invoke(req).await.expect("续接应成功");
    assert_eq!(second.session_id.as_deref(), Some(sid.as_str()));

    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let messages = seen[1]["messages"].as_array().unwrap().clone();
    let user = messages.iter().find(|m| m["role"] == "user");
    let assistant = messages.iter().find(|m| m["role"] == "assistant");
    assert!(
        user.and_then(|m| m["content"].as_str())
            .is_some_and(|c| c.contains("记住 ALPHA-9")),
        "续接请求应带第一条用户消息: {messages:?}"
    );
    assert!(
        assistant
            .and_then(|m| m["content"].as_str())
            .is_some_and(|c| c.contains("记住了 ALPHA-9")),
        "续接请求应带第一条助手回复: {messages:?}"
    );
}

// ===== 7. 取消：慢端点 + session/cancel → 5 秒内收场 =====

#[tokio::test]
async fn builtin_cancel_finishes_promptly() {
    let (base, _fake) = fake_model(vec![Step::Slow(60, "慢".into())]).await;
    let (work, state) = (tmp("w"), tmp("s"));
    let (tx, rx) = watch::channel(false);
    let mut req = RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "full"),
        "慢慢想".into(),
        work.clone(),
    );
    req.cancel = rx;
    let handle = tokio::spawn(async move {
        let (events, _) = mpsc::unbounded_channel();
        run_invocation(req, events).await
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t0 = std::time::Instant::now();
    tx.send(true).unwrap();
    let out = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("取消应在 5s 内返回")
        .unwrap()
        .expect("取消路径不报错");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_ne!(out.text, "慢");
}

// ===== 8. 注入 MCP：模型调 chat__read_conversation_history 拿到结果 =====

/// 假后端：只实现 GET /v1/agent-tools/history（同 tests/mcp.rs）。
async fn fake_backend() -> String {
    use axum::{routing::get, Json, Router};
    let app = Router::new().route(
        "/v1/agent-tools/history",
        get(|| async {
            Json(json!({"messages":[
                {"seq":11,"sender_name":"张三","is_agent":false,
                 "created_at":"2026-09-26T10:00:00Z","content":"早会结论-A29"},
            ],"has_more":false}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn builtin_injected_mcp_tool() {
    let backend = fake_backend().await;
    let mcp = agent_host::mcp::McpServer::start().unwrap();
    let h = mcp.register(backend, "TOKA29".into());
    let (base, fake) = fake_model(vec![
        Step::Calls(vec![(
            "chat__read_conversation_history".into(),
            json!({"limit": 5}),
        )]),
        Step::Text("拿到历史".into()),
    ])
    .await;
    let (work, state) = (tmp("w"), tmp("s"));
    let mut req = RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "full"),
        "读会话历史".into(),
        work.clone(),
    );
    req.mcp_servers = vec![InjectedMcp {
        name: "chat".into(),
        http_url: h.url.clone(),
        bridge: None,
    }];
    let out = invoke(req).await.expect("调用应成功");
    drop(h);
    assert_eq!(out.text, "拿到历史");
    // 第二个请求的 tool 消息带着 MCP 工具的返回
    let seen = fake.seen.lock().unwrap();
    let tool_msg = seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("应有 tool 消息");
    assert!(
        tool_msg["content"]
            .as_str()
            .unwrap()
            .contains("早会结论-A29"),
        "{tool_msg}"
    );
    // 步骤记录里这一跳是 other
    assert!(out
        .steps
        .iter()
        .any(|s| s.title == "chat__read_conversation_history" && s.kind == "other" && s.ok));
}

// ===== 附件：resource_link 块把路径带给模型 =====

#[tokio::test]
async fn builtin_attachment_reaches_model_as_path() {
    let (base, fake) = fake_model(vec![Step::Text("收到附件".into())]).await;
    let (work, state) = (tmp("w"), tmp("s"));
    // 附件文件落在 in/ 目录（宿主下载后就地放）
    let in_dir = tmp("in");
    let f = in_dir.join("说明.txt");
    std::fs::write(&f, "附件正文").unwrap();
    let mut req = RunRequest::for_invocation(
        spec(&work, &state, &endpoints_json(&base), "full"),
        "看下这个附件".into(),
        work.clone(),
    );
    req.attachments = vec![agent_host::runtime::LocalFile {
        path: f.clone(),
        name: "说明.txt".into(),
        mime: "text/plain".into(),
    }];
    let out = invoke(req).await.expect("应成功");
    assert_eq!(out.text, "收到附件");
    let seen = fake.seen.lock().unwrap();
    let user = seen[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .expect("应有 user 消息");
    let content = user["content"].as_str().unwrap();
    assert!(content.contains("说明.txt"), "{content}");
    assert!(content.contains("file:///"), "{content}");
}

// ===== 边界：空端点 → 管理员还没有配置模型 =====

#[tokio::test]
async fn builtin_no_endpoints_is_clean_error() {
    let (work, state) = (tmp("w"), tmp("s"));
    let e = invoke(RunRequest::for_invocation(
        spec(&work, &state, "[]", "full"),
        "hi".into(),
        work.clone(),
    ))
    .await
    .expect_err("空端点应失败");
    assert!(e.contains("管理员还没有配置模型"), "{e}");
}

// ===== probe：内置 agent 不起子进程 =====

#[tokio::test]
async fn builtin_probe_skips_process() {
    let spec = spec(&tmp("w"), &tmp("s"), "[]", "read_only");
    let out = agent_host::probe(spec, std::env::temp_dir(), "")
        .await
        .expect("内置 agent 的 probe 不应拉起进程");
    assert!(out.models.as_deref().is_some_and(|m| m.is_empty()));
    assert!(out.modes.as_deref().is_some_and(|m| m.is_empty()));
}
