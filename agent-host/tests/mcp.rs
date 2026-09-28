// Agent A13：宿主本地 MCP 服务与 stdio 桥接的测试。

use agent_host::acp::run_invocation;
use agent_host::mcp::McpServer;
use agent_host::runtime::{AgentSpec, InjectedMcp, RunRequest};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// 假服务端：只实现 GET /v1/agent-tools/history，记录 Authorization 头供断言。
async fn fake_backend() -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    use axum::{http::HeaderMap, routing::get, Json, Router};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/v1/agent-tools/history",
        get(move |h: HeaderMap| {
            let seen = seen2.clone();
            async move {
                seen.lock().unwrap().push(
                    h.get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string(),
                );
                Json(json!({"messages":[
                    {"seq":11,"sender_name":"张三","is_agent":false,
                     "created_at":"2026-09-26T10:00:00Z","content":"早会结论"},
                    {"seq":12,"sender_name":"research","is_agent":true,
                     "created_at":"2026-09-26T10:01:00Z","content":"已整理完毕"}
                ],"has_more":false}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let t = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), t, seen)
}

async fn rpc(url: &str, body: Value) -> (u16, Option<Value>) {
    let r = http().post(url).json(&body).send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    let v = if text.trim().is_empty() {
        None
    } else {
        Some(serde_json::from_str(&text).unwrap())
    };
    (status, v)
}

/// tools/list / tools/call：请求转发到服务端（带 Invocation 凭据），结果格式化成文本。
#[tokio::test]
async fn mcp_list_and_call_against_fake_backend() {
    let (backend, _bt, seen) = fake_backend().await;
    let mcp = McpServer::start().expect("mcp start");
    let h = mcp.register(backend, "TOKEN123".into());
    let url = h.url.clone();

    // initialize
    let (st, v) = rpc(
        &url,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert_eq!(st, 200);
    let v = v.unwrap();
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    assert!(v["result"]["serverInfo"]["name"].as_str().is_some());

    // 通知：202 且无响应体
    let (st, v) = rpc(
        &url,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(st, 202);
    assert!(v.is_none());

    // tools/list → 含 read_conversation_history
    let (st, v) = rpc(
        &url,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(st, 200);
    let tools = v.unwrap()["result"]["tools"].as_array().unwrap().clone();
    let t = tools
        .iter()
        .find(|t| t["name"] == "read_conversation_history")
        .expect("应有 read_conversation_history");
    assert!(t["inputSchema"].is_object());
    for name in [
        "list_conversations",
        "search_messages",
        "read_conversation",
        "send_message",
    ] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("应有 {name}"));
        let desc = tool["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("主人"),
            "{name} 的描述应写明只能访问主人的会话：{desc}"
        );
    }
    assert!(
        tools.iter().find(|t| t["name"] == "send_message").unwrap()["description"]
            .as_str()
            .unwrap()
            .contains("Agent")
    );

    // tools/call → 带调用凭据转发 → 格式化文本
    let (st, v) = rpc(
        &url,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
               "params":{"name":"read_conversation_history",
                         "arguments":{"before_seq":15,"limit":10}}}),
    )
    .await;
    assert_eq!(st, 200);
    let v = v.unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("[seq 11] 张三: 早会结论"), "{text}");
    assert!(
        text.contains("[seq 12] research (Agent): 已整理完毕"),
        "{text}"
    );
    assert_eq!(seen.lock().unwrap().as_slice(), ["Invocation TOKEN123"]);

    // 未知工具 → JSON-RPC error
    let (_st, v) = rpc(
        &url,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
               "params":{"name":"nope","arguments":{}}}),
    )
    .await;
    assert!(v.unwrap()["error"].is_object());
}

/// 映射摘除后立即 404（调用结束 = McpHandle drop）；不存在的路径也 404。
#[tokio::test]
async fn mcp_path_removed_on_handle_drop() {
    let mcp = McpServer::start().unwrap();
    let url = {
        let h = mcp.register("http://127.0.0.1:9".into(), "t".into());
        let url = h.url.clone();
        // drop 前可用
        let (st, _) = rpc(
            &url,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        assert_eq!(st, 200);
        url
    };
    // handle 已 drop → 404
    let (st, _) = rpc(
        &url,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert_eq!(st, 404);
    // 从未存在过的路径 → 404
    let (st, _) = rpc(
        &format!("http://127.0.0.1:{}/mcp/nope", mcp.port()),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert_eq!(st, 404);
}

/// agent-mcp-bridge bin：stdin 上的 JSON-RPC 行被 POST 到 MCP 服务，响应写回 stdout。
#[tokio::test]
async fn bridge_bin_forwards_stdio_roundtrip() {
    let mcp = McpServer::start().unwrap();
    let h = mcp.register("http://127.0.0.1:9".into(), "t".into());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-mcp-bridge"))
        .arg(&h.url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 1);
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");

    // 通知只发不回
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n")
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 2);
    assert!(v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "read_conversation_history"));

    drop(stdin);
    let _ = child.kill().await;
}

/// ACP 端到端：假 agent 经注入的 MCP 服务跑 tool:read_history。
/// mode=http → agent 直连本地 MCP；mode=stdio → agent 拉起 agent-mcp-bridge 子进程。
async fn acp_tool_e2e(mode: &str) -> String {
    let (backend, _bt, _seen) = fake_backend().await;
    let mcp = McpServer::start().unwrap();
    let h = mcp.register(backend, "TOK".into());
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake-acp-agent.mjs");
    let spec = AgentSpec {
        id: Uuid::new_v4(),
        name: "fake".into(),
        command: "node".into(),
        args: vec![
            script.to_string_lossy().to_string(),
            format!("--mcp={mode}"),
        ],
        cwd: String::new(),
        env: Vec::new(),
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
        registry: None,
        permission_tier: "read_only".into(),
    };
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut req =
        RunRequest::for_invocation(spec, "tool:read_history".into(), std::env::temp_dir());
    req.mcp_servers = vec![InjectedMcp {
        name: "chat".into(),
        http_url: h.url.clone(),
        bridge: Some((
            env!("CARGO_BIN_EXE_agent-mcp-bridge").to_string(),
            Vec::new(),
        )),
    }];
    let out = run_invocation(req, events).await.expect("调用应成功");
    drop(h);
    out.text
}

#[tokio::test]
async fn acp_http_mcp_end_to_end() {
    let text = acp_tool_e2e("http").await;
    assert!(text.contains("[seq 11] 张三: 早会结论"), "{text}");
    assert!(
        text.contains("[seq 12] research (Agent): 已整理完毕"),
        "{text}"
    );
}

#[tokio::test]
async fn acp_stdio_bridge_end_to_end() {
    let text = acp_tool_e2e("stdio").await;
    assert!(text.contains("[seq 11] 张三: 早会结论"), "{text}");
    assert!(
        text.contains("[seq 12] research (Agent): 已整理完毕"),
        "{text}"
    );
}

const A16_CONV: &str = "11111111-1111-1111-1111-111111111111";

fn push_seen(seen: &Arc<Mutex<Vec<String>>>, method: &str, auth: &str, extra: &str) {
    seen.lock()
        .unwrap()
        .push(format!("{method} {auth} {extra}"));
}

/// 假服务端：列会话 + 发消息，记下方法、Authorization 和 POST 正文。
async fn fake_send_backend() -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_get = seen.clone();
    let seen_post = seen.clone();
    let app = Router::new()
        .route(
            "/v1/agent-tools/conversations",
            get(move |h: HeaderMap| {
                let seen = seen_get.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    push_seen(&seen, "GET", &auth, "");
                    Json(json!([{
                        "id": A16_CONV,
                        "kind": "group",
                        "title": "项目群",
                        "member_count": 2,
                        "last_seq": 1,
                        "readonly": false
                    }]))
                }
            }),
        )
        .route(
            "/v1/agent-tools/messages",
            post(move |h: HeaderMap, body: Bytes| {
                let seen = seen_post.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    push_seen(&seen, "POST", &auth, &String::from_utf8_lossy(&body));
                    Json(json!({"message_id": "msg-1"}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let t = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), t, seen)
}

/// 只读档：先审批，正文只取前 80 字；拒绝则不 POST，工具错误是「主人拒绝了发送」。
#[tokio::test]
async fn send_message_read_only_reject_does_not_post() {
    let (backend, _bt, seen) = fake_send_backend().await;
    let mcp = McpServer::start().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = agent_host::runtime::ApprovalRegistry::new();
    let link = agent_host::runtime::ApprovalLink {
        tx,
        registry: registry.clone(),
    };
    let h = mcp.register_with(backend, "TOK".into(), "read_only".into(), Some(link));
    let url = h.url.clone();
    let long = "字".repeat(90);
    let call = tokio::spawn(async move {
        rpc(
            &url,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"send_message","arguments":{
                    "conversation_id": A16_CONV,
                    "text": long
                }}
            }),
        )
        .await
    });
    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("应发出审批")
        .expect("审批通道未关");
    assert_eq!(notice.tool_call["kind"], "send_message");
    let title = notice.tool_call["title"].as_str().unwrap();
    assert!(title.starts_with("发消息到「项目群」："), "{title}");
    assert!(title.contains(&"字".repeat(80)), "{title}");
    assert!(!title.contains(&"字".repeat(81)), "{title}");
    assert!(notice.tool_call["locations"]
        .as_array()
        .is_some_and(|a| a.is_empty()));
    let opts = notice.options.as_array().unwrap();
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "allow_once" && o["kind"] == "allow_once"));
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "reject_once" && o["kind"] == "reject_once"));
    assert!(
        seen.lock().unwrap().iter().all(|s| !s.starts_with("POST")),
        "批准前不应发消息：{:?}",
        seen.lock().unwrap()
    );
    assert!(registry.resolve(&notice.request_id, "reject_once".into()));
    let (st, v) = call.await.unwrap();
    assert_eq!(st, 200);
    let v = v.unwrap();
    assert_eq!(v["result"]["isError"], true);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("主人拒绝了发送"), "{text}");
    assert!(seen.lock().unwrap().iter().all(|s| !s.starts_with("POST")));
}

// ===== Agent A20 =====
// 只读档发消息审批要完整披露：tool_call.detail = 完整正文、locations = 将上传的
// 绝对路径；展示不下的（正文 >4000 / 路径 >20 条 / 单条 >500 字）直接报错，
// 不发审批也不上传。批准后只上传审批时展示的那一份清单。

/// 假服务端：列会话 + 收消息 + 收文件上传，全部按序记录。
async fn fake_send_upload_backend() -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>)
{
    use axum::body::Bytes;
    use axum::extract::Multipart;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_get = seen.clone();
    let seen_msg = seen.clone();
    let seen_file = seen.clone();
    let app = Router::new()
        .route(
            "/v1/agent-tools/conversations",
            get(move || {
                let seen = seen_get.clone();
                async move {
                    push_seen(&seen, "GET", "", "");
                    Json(json!([{
                        "id": A16_CONV,
                        "kind": "group",
                        "title": "项目群",
                        "member_count": 2,
                        "last_seq": 1,
                        "readonly": false
                    }]))
                }
            }),
        )
        .route(
            "/v1/agent-tools/messages",
            post(move |body: Bytes| {
                let seen = seen_msg.clone();
                async move {
                    push_seen(&seen, "POST", "", &String::from_utf8_lossy(&body));
                    Json(json!({"message_id": "msg-1"}))
                }
            }),
        )
        .route(
            "/v1/agent-tools/files",
            post(move |mut mp: Multipart| {
                let seen = seen_file.clone();
                async move {
                    while let Ok(Some(field)) = mp.next_field().await {
                        let name = field.file_name().unwrap_or("?").to_string();
                        let data = field.bytes().await.unwrap_or_default();
                        push_seen(&seen, "UPLOAD", "", &format!("{name}:{}", data.len()));
                    }
                    Json(json!({"attachment_id": "att-1"}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let t = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), t, seen)
}

fn send_call(url: String, args: Value) -> tokio::task::JoinHandle<(u16, Option<Value>)> {
    tokio::spawn(async move {
        rpc(
            &url,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                   "params":{"name":"send_message","arguments":args}}),
        )
        .await
    })
}

fn is_err_text(v: &Value) -> &str {
    assert_eq!(v["result"]["isError"], true, "{v}");
    v["result"]["content"][0]["text"].as_str().unwrap()
}

/// 主路径：detail 是完整正文（不止前 80 字），locations 是实际读取的绝对路径；
/// 批准前不上传不发消息，批准后只上传展示过的那份清单。
#[tokio::test]
async fn send_message_read_only_discloses_full_text_and_paths() {
    let (backend, _bt, seen) = fake_send_upload_backend().await;
    let mcp = McpServer::start().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = agent_host::runtime::ApprovalRegistry::new();
    let link = agent_host::runtime::ApprovalLink {
        tx,
        registry: registry.clone(),
    };
    let h = mcp.register_with(backend, "TOK".into(), "read_only".into(), Some(link));
    let url = h.url.clone();

    // 真实文件，用带 .. 的绝对路径传入：展示/上传的应是规范化后的绝对路径
    let dir = std::env::temp_dir().join(format!("a20-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let fp = dir.join("note.txt");
    std::fs::write(&fp, b"file-body").unwrap();
    let given = dir.join("sub").join("..").join("note.txt");
    let shown = std::path::absolute(&given).unwrap();
    let shown = shown.to_str().expect("路径应可展示").to_string();
    let text = format!("第一行\n{}", "字".repeat(120));
    let call = send_call(
        url,
        json!({"conversation_id": A16_CONV, "text": text,
               "file_paths": [given.to_str().unwrap()]}),
    );

    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("应发出审批")
        .expect("审批通道未关");
    let tc = &notice.tool_call;
    assert_eq!(tc["kind"], "send_message");
    assert_eq!(tc["detail"].as_str().unwrap(), text, "detail 应是完整正文");
    assert_eq!(
        tc["locations"],
        json!([shown]),
        "locations 应是实际读取的绝对路径"
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|s| !s.starts_with("POST") && !s.starts_with("UPLOAD")),
        "批准前不应上传或发消息：{:?}",
        seen.lock().unwrap()
    );

    assert!(registry.resolve(&notice.request_id, "allow_once".into()));
    let (st, v) = call.await.unwrap();
    assert_eq!(st, 200);
    let out = v.unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(out.contains("已发送"), "{out}");
    let log = seen.lock().unwrap().clone();
    let up = log
        .iter()
        .position(|s| s == "UPLOAD  note.txt:9")
        .expect("应上传一次 note.txt");
    let post = log
        .iter()
        .position(|s| s.starts_with("POST") && s.contains("att-1"))
        .expect("批准后应发消息");
    assert!(up < post, "应先上传再发消息：{log:?}");
    assert_eq!(
        log.iter().filter(|s| s.starts_with("UPLOAD")).count(),
        1,
        "只上传审批展示的那份清单：{log:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 超限直接报错：正文 >4000、路径 >20 条、单条路径 >500 字 —— 不发审批、不上传。
/// 边界：正文恰好 4000 字照常发审批，detail 一字不少。
#[tokio::test]
async fn send_message_read_only_over_limit_no_approval() {
    let (backend, _bt, seen) = fake_send_upload_backend().await;
    let mcp = McpServer::start().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = agent_host::runtime::ApprovalRegistry::new();
    let link = agent_host::runtime::ApprovalLink {
        tx,
        registry: registry.clone(),
    };
    let h = mcp.register_with(backend, "TOK".into(), "read_only".into(), Some(link));

    let (st, v) = rpc(
        &h.url,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
               "params":{"name":"send_message","arguments":{
                   "conversation_id": A16_CONV, "text": "超".repeat(4001)}}}),
    )
    .await;
    assert_eq!(st, 200);
    assert!(is_err_text(&v.unwrap()).contains("4000"), "应提示正文超限");

    let many: Vec<String> = (0..21).map(|i| format!("f{i}.txt")).collect();
    let (_st, v) = rpc(
        &h.url,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
               "params":{"name":"send_message","arguments":{
                   "conversation_id": A16_CONV, "text": "t", "file_paths": many}}}),
    )
    .await;
    assert!(is_err_text(&v.unwrap()).contains("20"), "应提示条数超限");

    let long_path = format!("C:\\{}.txt", "长".repeat(600));
    let (_st, v) = rpc(
        &h.url,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
               "params":{"name":"send_message","arguments":{
                   "conversation_id": A16_CONV, "text": "t", "file_paths": [long_path]}}}),
    )
    .await;
    assert!(
        is_err_text(&v.unwrap()).contains("500"),
        "应提示单条路径超限"
    );

    assert!(rx.try_recv().is_err(), "超限场景不应发出任何审批通知");
    assert!(
        seen.lock().unwrap().is_empty(),
        "超限场景不应有任何上传 / 发消息 / 查会话：{:?}",
        seen.lock().unwrap()
    );

    // 边界：4000 字正文照常发审批，detail 完整保留
    let text = "界".repeat(4000);
    let call = send_call(
        h.url.clone(),
        json!({"conversation_id": A16_CONV, "text": text}),
    );
    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("4000 字应照常发审批")
        .expect("审批通道未关");
    assert_eq!(
        notice.tool_call["detail"].as_str().unwrap().chars().count(),
        4000
    );
    assert!(registry.resolve(&notice.request_id, "reject_once".into()));
    let (_st, v) = call.await.unwrap();
    assert!(is_err_text(&v.unwrap()).contains("主人拒绝了发送"));
}

/// workspace / full 不走审批，直接 POST。
#[tokio::test]
async fn send_message_workspace_and_full_post_directly() {
    for tier in ["workspace", "full"] {
        let (backend, _bt, seen) = fake_send_backend().await;
        let mcp = McpServer::start().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = agent_host::runtime::ApprovalRegistry::new();
        let link = agent_host::runtime::ApprovalLink { tx, registry };
        let h = mcp.register_with(backend, "TOK".into(), tier.into(), Some(link));
        let (st, v) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rpc(
                &h.url,
                json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"send_message","arguments":{
                        "conversation_id": A16_CONV,
                        "text": "hello"
                    }}
                }),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{tier} 不应等待审批"));
        assert!(rx.try_recv().is_err(), "{tier} 不应发审批");
        assert_eq!(st, 200, "{tier}");
        let v = v.unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("已发送 message_id=msg-1"), "{tier} {text}");
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("POST") && s.contains("Invocation TOK")));
    }
}
