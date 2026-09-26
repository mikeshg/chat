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
