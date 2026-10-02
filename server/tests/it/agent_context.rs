use super::*;

// ===== Agent A02 =====
// 同一 AgentConversation 接着聊：运行时会话 id 由成功的 finish 记住，重置后不再带上。

struct A02Boot {
    base: String,
    user: TestUser,
    host: agent_host::HostHandle,
    host_id: String,
}

async fn a02_boot(tag: &str) -> A02Boot {
    let base = spawn_named_db(&format!("chat_t_a02{tag}_{}", uniq())).await;
    let user = register(&base, &format!("a02{tag}_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    A02Boot {
        base,
        user,
        host,
        host_id,
    }
}

/// 带 `--caps=` 的变体。不改 create_agent 的签名。
async fn create_agent_caps(
    base: &str,
    u: &TestUser,
    host_id: &str,
    name: &str,
    caps: &str,
) -> (u16, Value) {
    let mut args = fake_acp_args();
    args.push(format!("--caps={caps}"));
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .json(&json!({
            "name": name,
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
        }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

async fn a02_agent(b: &A02Boot, name: &str, caps: Option<&str>) -> String {
    let (st, agent) = match caps {
        Some(c) => create_agent_caps(&b.base, &b.user, &b.host_id, name, c).await,
        None => create_agent(&b.base, &b.user, &b.host_id, name).await,
    };
    assert_eq!(st, 200, "create agent {name}: {agent}");
    agent["conversation_id"].as_str().unwrap().to_string()
}

async fn a02_conn(
    base: &str,
    u: &TestUser,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap()
        .0
}

/// 等到这次调用进入终态，返回 (status, Agent 回复拼接)。
async fn await_agent_turn<S>(sock: &mut S) -> (String, String)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut frames: Vec<Value> = Vec::new();
    let mut status = String::new();
    for _ in 0..6 {
        frames.extend(until_frame(sock, "agent_invocation", A01Dur::from_secs(20)).await);
        if let Some(st) = frames.iter().rev().find_map(|f| {
            if f["type"] != "agent_invocation" {
                return None;
            }
            let s = f["invocation"]["status"].as_str()?;
            if matches!(s, "succeeded" | "failed" | "cancelled" | "timed_out") {
                Some(s.to_string())
            } else {
                None
            }
        }) {
            status = st;
            break;
        }
    }
    assert!(!status.is_empty(), "调用未进入终态: {frames:?}");
    let reply: String = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .map(|f| f["message"]["content"].as_str().unwrap_or(""))
        .collect();
    (status, reply)
}

async fn a02_say<S>(sock: &mut S, conv: &str, cmid: &str, content: &str) -> (String, String)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMsg>
        + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    send_ws(sock, conv, cmid, content).await;
    await_agent_turn(sock).await
}

async fn reset_context(base: &str, u: &TestUser, conv: &str) -> u16 {
    http()
        .post(format!(
            "{base}/v1/conversations/{conv}/agent-context/reset"
        ))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 1. 同一专属会话先 echo 再 recall，回复带上之前的指令。
#[tokio::test]
async fn agent_context_continues_in_same_conversation() {
    let b = a02_boot("go").await;
    let conv = a02_agent(&b, "cont", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "one");
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:one");
    b.host.shutdown().await;
}

/// 2. 重置后再 recall 是空会话；非成员被拒；私聊可重置、文件传输助手 400。
#[tokio::test]
async fn agent_context_reset_clears_and_rejects() {
    let b = a02_boot("rs").await;
    let other = register(&b.base, &format!("a02rsb_{}", uniq())).await;
    let conv = a02_agent(&b, "rst", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));

    assert_eq!(reset_context(&b.base, &b.user, &conv).await, 204);
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "(empty)");

    assert_eq!(reset_context(&b.base, &other, &conv).await, 403);
    befriend(&b.base, &b.user, &other).await;
    let dm = dm_conv(&b.base, &b.user).await;
    // 私聊 / 群也能开新话题（清请求者自己的 Agent 在这里的上下文）；文件传输助手仍不支持
    assert_eq!(reset_context(&b.base, &b.user, &dm).await, 204);
    let selfc = self_conv(&b.base, &b.user).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reset_context(&b.base, &b.user, &selfc).await, 400);
    b.host.shutdown().await;
}

/// 3. load 能力同样能续上，且回放的旧回复正文不混进这次回复。
#[tokio::test]
async fn agent_context_load_drops_replay() {
    let b = a02_boot("ld").await;
    let conv = a02_agent(&b, "loader", Some("load")).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    // 回放出来的旧回复是 "one"；混进来会变成 "oneecho:one"
    assert_eq!(reply, "echo:one");
    b.host.shutdown().await;
}

/// 4. 不声明恢复能力时退回新会话，第二次调用仍然成功，recall 为空。
#[tokio::test]
async fn agent_context_without_caps_starts_fresh() {
    let b = a02_boot("no").await;
    let conv = a02_agent(&b, "fresh", Some("none")).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "(empty)");
    b.host.shutdown().await;
}

/// 5. 两个 Agent 的上下文互不影响。
#[tokio::test]
async fn agent_context_isolated_per_agent() {
    let b = a02_boot("iso").await;
    let c1 = a02_agent(&b, "iso1", None).await;
    let c2 = a02_agent(&b, "iso2", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &c1, "a1", "echo:alpha").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "alpha"));
    let (st, reply) = a02_say(&mut sa, &c2, "b1", "echo:beta").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "beta"));
    let (st, reply) = a02_say(&mut sa, &c1, "a2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:alpha");
    let (st, reply) = a02_say(&mut sa, &c2, "b2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:beta");
    b.host.shutdown().await;
}

/// 6. 失败的调用不覆盖已记住的运行时会话，之后 recall 仍能接上。
#[tokio::test]
async fn agent_context_failed_invocation_keeps_session() {
    let b = a02_boot("fl").await;
    let conv = a02_agent(&b, "keep", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "fail").await;
    assert_eq!(st, "failed");
    assert!(reply.is_empty(), "失败调用不应产生回复: {reply}");
    let (st, reply) = a02_say(&mut sa, &conv, "e3", "recall").await;
    assert_eq!(st, "succeeded");
    assert!(reply.contains("echo:one"), "失败后应仍接上旧会话: {reply}");
    assert_ne!(reply, "(empty)");
    b.host.shutdown().await;
}

// ===== Agent 验收补丁：私聊 / 群里也能开新话题（spec Q6） =====

#[tokio::test]
async fn agent_reset_context_in_group_only_touches_own_agents() {
    let base = spawn_named_db(&format!("chat_it_rstg_{}", uniq())).await;
    let owner = register(&base, &format!("rsto_{}", uniq())).await;
    let member = register(&base, &format!("rstm_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let reset = |u: &TestUser, conv: &str| {
        http()
            .post(format!(
                "{base}/v1/conversations/{conv}/agent-context/reset"
            ))
            .header("authorization", authz(&u.access))
            .send()
    };

    send_ws(&mut so, &gid, "r1", "@bot echo:one").await;
    wait_agent_reply(&base, &owner, &gid, &["one"]).await;

    // 群里别的成员重置：只清他自己的 Agent（他没有），主人的上下文还在
    assert_eq!(reset(&member, &gid).await.unwrap().status().as_u16(), 204);
    send_ws(&mut so, &gid, "r2", "@bot recall").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["one", "echo:one"]).await;
    assert!(texts.iter().any(|t| t.contains("echo:one")), "{texts:?}");

    // 主人重置后，bot 在这个群里从头开始
    assert_eq!(reset(&owner, &gid).await.unwrap().status().as_u16(), 204);
    send_ws(&mut so, &gid, "r3", "@bot recall").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["one", "echo:one", "(empty)"]).await;
    assert!(texts.contains(&"(empty)".to_string()), "{texts:?}");

    // 文件传输助手不支持；非成员被拒
    let selfc = self_conv(&base, &owner).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reset(&owner, &selfc).await.unwrap().status().as_u16(), 400);
    let outsider = register(&base, &format!("rstx_{}", uniq())).await;
    assert_eq!(reset(&outsider, &gid).await.unwrap().status().as_u16(), 403);
    host.shutdown().await;
}

// ===== Agent A21 =====
// 进行中调用的流式接收者随成员变更重算；开新话题之后，重置前派发的调用不得把旧会话写回。

type A21Sock =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn a21_open_host(base: &str, u: &TestUser) -> (String, A21Sock) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),"label":"a21","platform":"test"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let frames = until_frame(&mut sock, "host_ready", A01Dur::from_secs(5)).await;
    let id = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "host_ready")
        .unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    (id, sock)
}

async fn a21_open_user(base: &str, u: &TestUser) -> A21Sock {
    tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap()
        .0
}

async fn a21_next_frame<S>(sock: &mut S, pred: impl Fn(&Value) -> bool) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等宿主/成员帧超时: {seen:?}");
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let hit = pred(&v);
                    seen.push(v.clone());
                    if hit {
                        return v;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("读帧异常: {other:?}; 已收到: {seen:?}"),
        }
    }
}

async fn a21_wait_delta<S>(sock: &mut S, inv_id: &str, text: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    a21_next_frame(sock, |v| {
        v["type"] == "agent_stream"
            && v["invocation_id"] == inv_id
            && v["delta"].as_str().unwrap_or("").contains(text)
    })
    .await
}

async fn a21_no_delta<S>(sock: &mut S, inv_id: &str, text: &str)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let frames = frames_for(sock, A01Dur::from_millis(500)).await;
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "agent_stream"
                && f["invocation_id"] == inv_id
                && f["delta"].as_str().unwrap_or("").contains(text)
        }),
        "{inv_id} 不应再收到 {text}: {frames:?}"
    );
}

async fn a21_push_text<S>(host: &mut S, inv_id: &str, text: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    host.send(WsMsg::Text(
        json!({
            "type": "invocation_event",
            "invocation_id": inv_id,
            "event": {"type": "text", "text": text}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

async fn a21_finish<S>(host: &mut S, inv_id: &str, session_id: &str, text: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    host.send(WsMsg::Text(
        json!({
            "type": "invocation_finish",
            "invocation_id": inv_id,
            "ok": true,
            "text": text,
            "session_id": session_id
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

async fn a21_wait_status(base: &str, u: &TestUser, conv: &str, inv_id: &str, status: &str) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    loop {
        let rows: Value = http()
            .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == inv_id && r["status"] == status)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等 {inv_id} 变为 {status} 超时: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

fn a21_session(start: &Value) -> Value {
    start["context"]["session_id"].clone()
}

async fn a21_say_start(user: &mut A21Sock, host: &mut A21Sock, conv: &str, content: &str) -> Value {
    send_ws(user, conv, &format!("a21{}", uniq()), content).await;
    a21_next_frame(host, |v| v["type"] == "invocation_start").await
}

/// 群里 @ 自己的 Agent，假宿主推流。踢人后 B 不再收到，A 还收得到；解散后 A 也不再收到。
#[tokio::test]
async fn agent_a21_kick_and_dissolve_stop_stream() {
    let base = spawn_named_db(&format!("chat_it_a21k_{}", uniq())).await;
    let a = register(&base, &format!("a21ka_{}", uniq())).await;
    let b = register(&base, &format!("a21kb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "ka").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start = a21_say_start(&mut sa, &mut host, &gid, "@ka hi").await;
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    a21_push_text(&mut host, &inv_id, "A21BEFORE").await;
    let before_a = a21_wait_delta(&mut sa, &inv_id, "A21BEFORE").await;
    assert_eq!(before_a["conversation_id"], gid.as_str());
    a21_wait_delta(&mut sb, &inv_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    a21_push_text(&mut host, &inv_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &inv_id, "A21AFTER").await;
    a21_no_delta(&mut sb, &inv_id, "A21AFTER").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21THIRD").await;
    a21_no_delta(&mut sa, &inv_id, "A21THIRD").await;
}

/// B 退群后收不到后续增量；最后一人退出把会话删掉后，主人也不再收到。
#[tokio::test]
async fn agent_a21_leave_and_last_member_stop_stream() {
    let base = spawn_named_db(&format!("chat_it_a21l_{}", uniq())).await;
    let a = register(&base, &format!("a21la_{}", uniq())).await;
    let b = register(&base, &format!("a21lb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "la").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start = a21_say_start(&mut sa, &mut host, &gid, "@la hi").await;
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    a21_push_text(&mut host, &inv_id, "A21BEFORE").await;
    a21_wait_delta(&mut sa, &inv_id, "A21BEFORE").await;
    a21_wait_delta(&mut sb, &inv_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &inv_id, "A21AFTER").await;
    a21_no_delta(&mut sb, &inv_id, "A21AFTER").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21LAST").await;
    a21_no_delta(&mut sa, &inv_id, "A21LAST").await;
}

/// 子调用的接收者是「专属会话成员 ∪ 根会话成员」。
/// 踢掉只在群里的人后他收不到；主人退出群后仍在专属会话里，流不能断。
#[tokio::test]
async fn agent_a21_root_member_kept_when_still_in_other_conversation() {
    let base = spawn_named_db(&format!("chat_it_a21u_{}", uniq())).await;
    let a = register(&base, &format!("a21ua_{}", uniq())).await;
    let b = register(&base, &format!("a21ub_{}", uniq())).await;
    let c = register(&base, &format!("a21uc_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, parent) = create_agent(&base, &a, &host_id, "pa").await;
    assert_eq!(st, 200, "{parent}");
    let (st, child) = create_agent(&base, &a, &host_id, "ch").await;
    assert_eq!(st, 200, "{child}");
    let child_conv = child["conversation_id"].as_str().unwrap().to_string();
    let gid = a11_group(&base, &a, &[&b, &c]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;
    let mut sc = a21_open_user(&base, &c).await;

    let parent_start = a21_say_start(&mut sa, &mut host, &gid, "@pa go").await;
    let token = parent_start["tools_token"]
        .as_str()
        .expect("父调用应带 tools_token")
        .to_string();
    let resp = http()
        .post(format!("{base}/v1/agent-tools/invocations"))
        .header("authorization", format!("Invocation {token}"))
        .json(&json!({"agent_name": "ch", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let child_id = body["invocation_id"].as_str().unwrap().to_string();
    let child_start = a21_next_frame(&mut host, |v| v["type"] == "invocation_start").await;
    assert_eq!(child_start["invocation_id"], child_id.as_str());

    a21_push_text(&mut host, &child_id, "A21BEFORE").await;
    let frame = a21_wait_delta(&mut sa, &child_id, "A21BEFORE").await;
    assert_eq!(frame["conversation_id"], child_conv.as_str());
    a21_wait_delta(&mut sb, &child_id, "A21BEFORE").await;
    a21_wait_delta(&mut sc, &child_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", c.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &child_id, "A21MID").await;
    a21_wait_delta(&mut sa, &child_id, "A21MID").await;
    a21_wait_delta(&mut sb, &child_id, "A21MID").await;
    a21_no_delta(&mut sc, &child_id, "A21MID").await;

    // 主人退出群：专属会话里还有他，根会话里还有 B。两边都不能被拿掉。
    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &child_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &child_id, "A21AFTER").await;
    a21_wait_delta(&mut sb, &child_id, "A21AFTER").await;
    a21_no_delta(&mut sc, &child_id, "A21AFTER").await;
}

/// 专属会话：运行中开新话题后，旧调用的 session 不写回；之后的调用后完成者覆盖，下一次能续上。
#[tokio::test]
async fn agent_a21_reset_dedicated_blocks_inflight_session_writeback() {
    let base = spawn_named_db(&format!("chat_it_a21d_{}", uniq())).await;
    let a = register(&base, &format!("a21da_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "solo").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let mut sa = a21_open_user(&base, &a).await;

    let start1 = a21_say_start(&mut sa, &mut host, &conv, "one").await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start1["context"]["key"], conv.as_str());
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    a21_finish(&mut host, &id1, "old-session", "old").await;
    a21_wait_status(&base, &a, &conv, &id1, "succeeded").await;

    let start2 = a21_say_start(&mut sa, &mut host, &conv, "two").await;
    let id2 = start2["invocation_id"].as_str().unwrap().to_string();
    assert!(
        a21_session(&start2).is_null(),
        "重置前的调用写回应被丢掉: {}",
        start2["context"]
    );
    let start3 = a21_say_start(&mut sa, &mut host, &conv, "three").await;
    let id3 = start3["invocation_id"].as_str().unwrap().to_string();
    assert!(a21_session(&start3).is_null(), "{}", start3["context"]);

    a21_finish(&mut host, &id2, "s2", "two").await;
    a21_wait_status(&base, &a, &conv, &id2, "succeeded").await;
    a21_finish(&mut host, &id3, "s3", "three").await;
    a21_wait_status(&base, &a, &conv, &id3, "succeeded").await;

    let start4 = a21_say_start(&mut sa, &mut host, &conv, "four").await;
    assert_eq!(
        a21_session(&start4),
        json!("s3"),
        "后完成的调用应覆盖上下文: {}",
        start4["context"]
    );
}

/// 群里只清请求者自己的 Agent。别人的进行中调用仍写回；自己的旧调用不写回，新调用可以续上。
#[tokio::test]
async fn agent_a21_reset_group_only_blocks_own_agent() {
    let base = spawn_named_db(&format!("chat_it_a21g_{}", uniq())).await;
    let a = register(&base, &format!("a21ga_{}", uniq())).await;
    let b = register(&base, &format!("a21gb_{}", uniq())).await;
    let (host_a_id, mut host_a) = a21_open_host(&base, &a).await;
    let (host_b_id, mut host_b) = a21_open_host(&base, &b).await;
    let (st, agent_a) = create_agent(&base, &a, &host_a_id, "ga").await;
    assert_eq!(st, 200, "{agent_a}");
    let (st, agent_b) = create_agent(&base, &b, &host_b_id, "gb").await;
    assert_eq!(st, 200, "{agent_b}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start_a = a21_say_start(&mut sa, &mut host_a, &gid, "@ga hi").await;
    let id_a = start_a["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start_a["context"]["key"], gid.as_str());
    let start_b = a21_say_start(&mut sb, &mut host_b, &gid, "@gb hi").await;
    let id_b = start_b["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start_b["context"]["key"], gid.as_str());

    assert_eq!(reset_context(&base, &a, &gid).await, 204);
    a21_finish(&mut host_a, &id_a, "a-old", "from-a").await;
    a21_finish(&mut host_b, &id_b, "b-keep", "from-b").await;
    a21_wait_status(&base, &a, &gid, &id_a, "succeeded").await;
    a21_wait_status(&base, &b, &gid, &id_b, "succeeded").await;

    let again_a = a21_say_start(&mut sa, &mut host_a, &gid, "@ga again").await;
    assert!(
        a21_session(&again_a).is_null(),
        "自己的旧调用不应写回: {}",
        again_a["context"]
    );
    let again_b = a21_say_start(&mut sb, &mut host_b, &gid, "@gb again").await;
    assert_eq!(
        a21_session(&again_b),
        json!("b-keep"),
        "别人的 Agent 不受这次重置影响: {}",
        again_b["context"]
    );

    let id_new = again_a["invocation_id"].as_str().unwrap().to_string();
    a21_finish(&mut host_a, &id_new, "a-new", "fresh").await;
    a21_wait_status(&base, &a, &gid, &id_new, "succeeded").await;
    let third = a21_say_start(&mut sa, &mut host_a, &gid, "@ga third").await;
    assert_eq!(a21_session(&third), json!("a-new"), "{}", third["context"]);
}

/// 私聊走和群同一条重置路径：只挡请求者自己的 Agent，重置后的新调用仍能写回。
#[tokio::test]
async fn agent_a21_reset_dm_blocks_inflight_session_writeback() {
    let base = spawn_named_db(&format!("chat_it_a21m_{}", uniq())).await;
    let a = register(&base, &format!("a21ma_{}", uniq())).await;
    let b = register(&base, &format!("a21mb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "dmbot").await;
    assert_eq!(st, 200, "{agent}");
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let mut sa = a21_open_user(&base, &a).await;

    let start1 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot hi").await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start1["context"]["key"], dm.as_str());
    assert_eq!(reset_context(&base, &a, &dm).await, 204);
    a21_finish(&mut host, &id1, "dm-old", "old").await;
    a21_wait_status(&base, &a, &dm, &id1, "succeeded").await;

    let start2 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot next").await;
    assert!(
        a21_session(&start2).is_null(),
        "私聊重置前的调用不应写回: {}",
        start2["context"]
    );
    let id2 = start2["invocation_id"].as_str().unwrap().to_string();
    a21_finish(&mut host, &id2, "dm-new", "new").await;
    a21_wait_status(&base, &a, &dm, &id2, "succeeded").await;
    let start3 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot cont").await;
    assert_eq!(
        a21_session(&start3),
        json!("dm-new"),
        "{}",
        start3["context"]
    );
}

/// 定时调用的 schedule:<id> 不跟会话重置走：运行中重置专属会话，完成后仍写回，下一次还能续上。
#[tokio::test]
async fn agent_a21_schedule_context_survives_reset() {
    let (base, st) = spawn_named_state(&format!("chat_it_a21s_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a21sa_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "sched").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (code, sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨", "ping", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{sched}");
    let sched_id = sched["id"].as_str().unwrap().to_string();
    let key = format!("schedule:{sched_id}");

    a17_due(&st.pool, &sched_id).await;
    let start1 = a21_next_frame(&mut host, |v| {
        v["type"] == "invocation_start" && v["context"]["key"] == key.as_str()
    })
    .await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert!(a21_session(&start1).is_null(), "{}", start1["context"]);
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    a21_finish(&mut host, &id1, "sched-sess", "ping").await;
    a21_wait_status(&base, &a, &conv, &id1, "succeeded").await;

    let saved: Option<String> = sqlx::query_scalar(
        "SELECT runtime_session_id FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&key)
    .fetch_optional(&st.pool)
    .await
    .unwrap();
    assert_eq!(saved.as_deref(), Some("sched-sess"));
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    let still: Option<String> = sqlx::query_scalar(
        "SELECT runtime_session_id FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&key)
    .fetch_optional(&st.pool)
    .await
    .unwrap();
    assert_eq!(still.as_deref(), Some("sched-sess"));

    a17_due(&st.pool, &sched_id).await;
    let start2 = a21_next_frame(&mut host, |v| {
        v["type"] == "invocation_start"
            && v["context"]["key"] == key.as_str()
            && v["invocation_id"] != id1.as_str()
    })
    .await;
    assert_eq!(
        a21_session(&start2),
        json!("sched-sess"),
        "定时上下文不应被开新话题清掉: {}",
        start2["context"]
    );
}

/// 重启语义：调用行上留着派发时的旧代次，内存里的运行时快照已经没了，finish 不写回。
#[tokio::test]
async fn agent_a21_stale_epoch_without_run_does_not_restore_session() {
    let (base, st) = spawn_named_state(&format!("chat_it_a21r_{}", uniq())).await;
    let a = register(&base, &format!("a21ra_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "solo").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let mut sa = a21_open_user(&base, &a).await;

    let start = a21_say_start(&mut sa, &mut host, &conv, "one").await;
    let id = start["invocation_id"].as_str().unwrap().to_string();
    let inv_id = uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(reset_context(&base, &a, &conv).await, 204);

    // 重置后上下文代次已是 1、会话为空。调用行改回派发时的旧代次，并摘掉内存快照。
    let epoch: i64 = sqlx::query_scalar(
        "SELECT epoch FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&conv)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(epoch, 1);
    sqlx::query("UPDATE agent_invocations SET context_epoch = 0 WHERE id = $1")
        .bind(inv_id)
        .execute(&st.pool)
        .await
        .unwrap();
    assert!(
        st.inv_runs.remove(&inv_id).is_some(),
        "派发后应有运行时快照可摘"
    );
    assert!(st.inv_runs.get(&inv_id).is_none());

    a21_finish(&mut host, &id, "revived", "nope").await;
    a21_wait_status(&base, &a, &conv, &id, "succeeded").await;
    let saved: (String, i64) = sqlx::query_as(
        "SELECT runtime_session_id, epoch FROM agent_contexts
          WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&conv)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(saved.0, "", "旧代次的 finish 不应把会话写回来");
    assert_eq!(saved.1, 1);

    let next = a21_say_start(&mut sa, &mut host, &conv, "two").await;
    assert!(
        a21_session(&next).is_null(),
        "下一次派发仍不应带上被拒绝的会话: {}",
        next["context"]
    );
}
