// Agent A23：掉线不杀进程，重连后补发 finish；取消先于启动则不起 Agent。
// 本地假 WS 服务端 + tests/fake-acp-agent.mjs。不连真实聊天服务端。

use agent_host::{spawn, HostConfig};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

fn fake_agent() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fake-acp-agent.mjs")
        .to_string_lossy()
        .to_string()
}

fn agent_json(id: Uuid) -> Value {
    json!({
        "id": id,
        "name": "fake",
        "runtime": {"command": "node", "args": [fake_agent()]},
        "cwd": "",
        "permission_tier": "read_only",
    })
}

async fn recv_text(ws: &mut Ws, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "等宿主帧超时");
        let msg = tokio::time::timeout(left, ws.next())
            .await
            .expect("等宿主帧超时")
            .expect("宿主连接已关")
            .expect("ws 错误");
        match msg {
            Message::Text(t) => return serde_json::from_str(&t).expect("非 JSON"),
            Message::Ping(p) => ws.send(Message::Pong(p)).await.expect("pong"),
            Message::Close(_) => panic!("宿主主动关闭"),
            _ => {}
        }
    }
}

async fn accept(listener: &TcpListener) -> Ws {
    let (sock, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
        .await
        .expect("宿主没有重连")
        .unwrap();
    tokio_tungstenite::accept_async(sock).await.unwrap()
}

fn spawn_host(base: &str, install: Uuid) -> agent_host::HostHandle {
    let data = std::env::temp_dir().join(format!("a23-host-{install}"));
    let _ = std::fs::create_dir_all(&data);
    spawn(
        HostConfig {
            server_url: base.into(),
            install_id: install,
            label: "a23".into(),
            platform: "test".into(),
            mcp_bridge: None,
            registry_url: "http://127.0.0.1:9/no-registry.json".into(),
            data_dir: data,
        },
        std::sync::Arc::new(|| async { Ok::<_, String>("ticket".into()) }),
    )
}

async fn hello_and_ready(ws: &mut Ws) -> Value {
    let hello = recv_text(ws, Duration::from_secs(5)).await;
    assert_eq!(hello["type"], "host_hello", "{hello}");
    ws.send(Message::Text(
        json!({"type": "host_ready", "host_id": Uuid::new_v4()})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    hello
}

fn start_frame(inv: Uuid, prompt: &str) -> String {
    json!({
        "type": "invocation_start",
        "invocation_id": inv,
        "agent": agent_json(Uuid::new_v4()),
        "prompt": prompt,
        "context": {"key": "k", "session_id": Value::Null},
    })
    .to_string()
}

/// 调用进行中断线：进程还在，新连接的 host_hello 带上这个 id，并补发 finish。
#[tokio::test]
async fn disconnect_mid_run_replays_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let host = spawn_host(&format!("http://127.0.0.1:{port}"), Uuid::new_v4());

    let mut ws = accept(&listener).await;
    let hello = hello_and_ready(&mut ws).await;
    assert!(hello["running"].as_array().unwrap().is_empty(), "{hello}");

    let inv = Uuid::new_v4();
    ws.send(Message::Text(start_frame(inv, "sleep:3000").into()))
        .await
        .unwrap();
    // 给假 Agent 一点时间真正跑起来，再拆连接。
    tokio::time::sleep(Duration::from_millis(600)).await;
    ws.close(None).await.ok();
    drop(ws);

    let mut ws = accept(&listener).await;
    let hello = hello_and_ready(&mut ws).await;
    let running = hello["running"].as_array().expect("hello 应带 running");
    assert!(
        running
            .iter()
            .any(|v| v.as_str() == Some(inv.to_string().as_str())),
        "重连应报告还在跑的调用: {hello}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "重连后应补发 invocation_finish");
        let finish = recv_text(&mut ws, left).await;
        if finish["type"] != "invocation_finish" {
            continue;
        }
        assert_eq!(finish["invocation_id"], inv.to_string());
        assert_eq!(finish["ok"], true, "{finish}");
        assert_eq!(finish["text"], "done", "{finish}");
        break;
    }
    host.shutdown().await;
}

/// 还没登记的调用先收到 cancel，随后的 start 不起进程、不回 finish。
#[tokio::test]
async fn cancel_before_start_drops_invocation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let host = spawn_host(&format!("http://127.0.0.1:{port}"), Uuid::new_v4());

    let mut ws = accept(&listener).await;
    hello_and_ready(&mut ws).await;
    let inv = Uuid::new_v4();
    ws.send(Message::Text(
        json!({"type": "invocation_cancel", "invocation_id": inv})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        start_frame(inv, "echo:should-not-run").into(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_ne!(
                    v["type"], "invocation_finish",
                    "提前取消的调用不应回 finish: {v}"
                );
                assert_ne!(
                    v["type"], "invocation_event",
                    "提前取消的调用不应起 Agent: {v}"
                );
            }
            Ok(Some(Ok(Message::Ping(p)))) => ws.send(Message::Pong(p)).await.unwrap(),
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    host.shutdown().await;
}

/// 等审批时断线：重连后 hello 带 request_id，决定送达后调用完成。
#[tokio::test]
async fn approval_survives_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let host = spawn_host(&format!("http://127.0.0.1:{port}"), Uuid::new_v4());

    let mut ws = accept(&listener).await;
    hello_and_ready(&mut ws).await;
    let inv = Uuid::new_v4();
    ws.send(Message::Text(
        start_frame(inv, "perm:edit:C:/outside/a23.txt").into(),
    ))
    .await
    .unwrap();

    let mut request_id = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while request_id.is_empty() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "等 approval_request 超时");
        let v = recv_text(&mut ws, left).await;
        if v["type"] == "approval_request" {
            assert_eq!(v["invocation_id"], inv.to_string());
            request_id = v["request_id"].as_str().unwrap().to_string();
        }
    }
    ws.close(None).await.ok();
    drop(ws);

    let mut ws = accept(&listener).await;
    let hello = hello_and_ready(&mut ws).await;
    let pending = hello["pending_approvals"]
        .as_array()
        .expect("hello 应带 pending_approvals");
    assert!(
        pending
            .iter()
            .any(|v| v.as_str() == Some(request_id.as_str())),
        "重连应报告还在等的审批: {hello}"
    );
    let running = hello["running"].as_array().unwrap();
    assert!(
        running
            .iter()
            .any(|v| v.as_str() == Some(inv.to_string().as_str())),
        "等审批的调用仍应算在跑: {hello}"
    );

    ws.send(Message::Text(
        json!({
            "type": "approval_decision",
            "request_id": request_id,
            "option_id": "allow",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "决定之后应收到 finish");
        let v = recv_text(&mut ws, left).await;
        if v["type"] == "invocation_finish" {
            assert_eq!(v["invocation_id"], inv.to_string());
            assert_eq!(v["ok"], true, "{v}");
            assert_eq!(v["text"], "allowed:allow", "{v}");
            break;
        }
    }
    host.shutdown().await;
}
