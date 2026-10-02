use super::*;

// ===== Agent A08 =====
// 使用权：全局开关、默认、逐用户覆盖、收回时取消并断开宿主、宿主跟随登录会话。

async fn put_settings(base: &str, admin: &TestUser, body: Value) -> Value {
    let r = http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "settings {v}");
    v
}

async fn put_access(base: &str, admin: &TestUser, user_id: &str, allowed: Option<bool>) -> u16 {
    http()
        .put(format!("{base}/v1/admin/users/{user_id}/agent-access"))
        .header("authorization", authz(&admin.access))
        .json(&json!({ "allowed": allowed }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn me_json(base: &str, access: &str) -> Value {
    http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn deny_agent(base: &str, method: &str, path: &str, access: &str, body: Option<Value>) {
    let c = http();
    let url = format!("{base}/v1{path}");
    let req = match method {
        "GET" => c.get(url),
        "POST" => c.post(url),
        "PUT" => c.put(url),
        "PATCH" => c.patch(url),
        "DELETE" => c.delete(url),
        _ => unreachable!(),
    };
    let req = req.header("authorization", authz(access));
    let r = if let Some(b) = &body {
        req.json(b).send().await.unwrap()
    } else {
        req.send().await.unwrap()
    };
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 403, "{method} {path} -> {v}");
    assert_eq!(v["error"], "agent_access_denied");
}

/// 无权时升级成功但马上被关掉，不会 host_ready。
async fn assert_host_rejected(base: &str, u: &TestUser) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let install = uuid::Uuid::new_v4();
    let _ = sock
        .send(WsMsg::Text(
            json!({"type":"host_hello","install_id":install,"label":"nope","platform":"test"})
                .to_string()
                .into(),
        ))
        .await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(3);
    let mut ready = false;
    let mut ended = false;
    while std::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if t.contains("host_ready") {
                    ready = true;
                    break;
                }
            }
            Ok(Some(Ok(WsMsg::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                ended = true;
                break;
            }
            Err(_) => break,
            _ => {}
        }
    }
    assert!(!ready, "无权宿主不应拿到 host_ready");
    assert!(ended, "无权宿主连接应在握手后被关闭");
}

/// 读宿主文本帧直到断言成立。`__closed__` 表示套接字已结束。
async fn host_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pred: impl Fn(&str, Option<&Value>) -> bool,
    timeout: A01Dur,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等宿主帧超时");
        let t = tokio::time::timeout(left, rx.recv())
            .await
            .expect("等宿主帧超时")
            .expect("宿主管道已关");
        let parsed = serde_json::from_str::<Value>(&t).ok();
        if pred(&t, parsed.as_ref()) {
            return t;
        }
    }
}

/// 真 WS 宿主：hello 之后把后续文本帧转进通道。关掉 close 发送端即断开。
async fn raw_host(
    base: &str,
    u: &TestUser,
    install: uuid::Uuid,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::oneshot::Sender<()>,
) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":install,"label":"raw","platform":"test"})
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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut close_rx => break,
                incoming = sock.next() => {
                    match incoming {
                        Some(Ok(WsMsg::Text(t))) => { let _ = tx.send(t.to_string()); }
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
        }
        let _ = tx.send("__closed__".into());
    });
    (id, rx, close_tx)
}

async fn wait_online(base: &str, access: &str, host_id: &str, online: bool) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let hosts: Value = http()
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let on = hosts
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == host_id)
            .and_then(|h| h["online"].as_bool())
            .unwrap_or(false);
        if on == online {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "host {host_id} online={on} 期望 {online}: {hosts}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

/// 1. 全局关：普通用户 Agent REST 403、宿主连不上、专属会话发消息被拒且不落库；管理员照常。
#[tokio::test]
async fn agent_a08_global_off() {
    let base = spawn_named_db(&format!("chat_it_a08off_{}", uniq())).await;
    let admin = register(&base, &format!("a08ad_{}", uniq())).await;
    let user = register(&base, &format!("a08u_{}", uniq())).await;
    befriend(&base, &admin, &user).await;

    let (h, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &user, &host_id, "runner").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    h.shutdown().await;

    let settings = put_settings(&base, &admin, json!({"agent_enabled": false})).await;
    assert_eq!(settings["agent_enabled"], false);
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);
    assert_eq!(me_json(&base, &admin.access).await["agent_access"], true);

    deny_agent(&base, "GET", "/agent-hosts", &user.access, None).await;
    deny_agent(&base, "GET", "/agents", &user.access, None).await;
    deny_agent(
        &base,
        "POST",
        "/agents",
        &user.access,
        Some(json!({"name":"x","host_id":host_id,"runtime":{"command":"node","args":[]}})),
    )
    .await;
    deny_agent(
        &base,
        "PATCH",
        &format!("/agents/{agent_id}"),
        &user.access,
        Some(json!({"cwd":"/tmp"})),
    )
    .await;
    deny_agent(
        &base,
        "DELETE",
        &format!("/agents/{agent_id}"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/agents/{agent_id}/probe"),
        &user.access,
        None,
    )
    .await;
    deny_agent(&base, "GET", "/users/me/agent-settings", &user.access, None).await;
    deny_agent(
        &base,
        "PUT",
        "/users/me/agent-settings",
        &user.access,
        Some(json!({"output_mode":"stream"})),
    )
    .await;
    deny_agent(
        &base,
        "GET",
        &format!("/conversations/{conv}/agent-invocations"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/conversations/{conv}/agent-context/reset"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/agent-invocations/{}/cancel", uuid::Uuid::new_v4()),
        &user.access,
        None,
    )
    .await;
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(b"not-an-image".as_slice())
            .file_name("a.png")
            .mime_str("image/png")
            .unwrap(),
    );
    let avatar = http()
        .put(format!("{base}/v1/agents/{agent_id}/avatar"))
        .header("authorization", authz(&user.access))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(avatar.status(), 403);
    assert_eq!(
        avatar.json::<Value>().await.unwrap()["error"],
        "agent_access_denied"
    );
    // 头像 GET 不鉴权
    let guest = http()
        .get(format!("{base}/v1/agents/{agent_id}/avatar"))
        .send()
        .await
        .unwrap();
    assert_eq!(guest.status(), 404);

    assert_host_rejected(&base, &user).await;

    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;
    send_ws(&mut su, &conv, "blocked", "should-not-store").await;
    let err = next_of(&mut su, "error").await;
    assert_eq!(err["code"], "agent_access_denied");
    let msgs = conv_messages(&base, &user, &conv).await;
    assert!(
        msgs.iter().all(|m| m["content"] != "should-not-store"),
        "无权时专属会话消息不应落库: {msgs:?}"
    );

    let gid = mk_group(&base, &user, &[admin.id.clone()]).await;
    send_ws(&mut su, &gid, "at1", "@runner hello-group").await;
    let quiet = frames_for(&mut su, A01Dur::from_millis(400)).await;
    assert!(
        quiet.iter().all(|f| f["type"] != "agent_invocation"),
        "群里 @ 不应触发调用: {quiet:?}"
    );
    let group_msgs = conv_messages(&base, &user, &gid).await;
    assert!(
        group_msgs
            .iter()
            .any(|m| m["content"] == "@runner hello-group"),
        "群消息本身要留下: {group_msgs:?}"
    );

    // 管理员不受全局开关影响
    let agents: Value = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(agents.as_array().unwrap().is_empty());
    let (ha, ahost) = spawn_host(&base, &admin, uuid::Uuid::new_v4()).await;
    let (st, created) = create_agent(&base, &admin, &ahost, "boss").await;
    assert_eq!(st, 200, "{created}");
    let aconv = created["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    send_ws(&mut sa, &aconv, "ok1", "echo:admin-ok").await;
    wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    ha.shutdown().await;
}

/// 2. 默认禁止 + 覆盖允许；改回 null 后又跟随默认。
#[tokio::test]
async fn agent_a08_default_and_override() {
    let base = spawn_named_db(&format!("chat_it_a08def_{}", uniq())).await;
    let admin = register(&base, &format!("a08bd_{}", uniq())).await;
    let allow = register(&base, &format!("a08ba_{}", uniq())).await;
    let other = register(&base, &format!("a08bb_{}", uniq())).await;

    let s = put_settings(&base, &admin, json!({"agent_default_allowed": false})).await;
    assert_eq!(s["agent_default_allowed"], false);
    deny_agent(&base, "GET", "/agents", &allow.access, None).await;
    deny_agent(&base, "GET", "/agents", &other.access, None).await;

    assert_eq!(put_access(&base, &admin, &allow.id, Some(true)).await, 204);
    let ok = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&allow.access))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    deny_agent(&base, "GET", "/agent-hosts", &other.access, None).await;

    assert_eq!(put_access(&base, &admin, &allow.id, None).await, 204);
    deny_agent(
        &base,
        "GET",
        "/users/me/agent-settings",
        &allow.access,
        None,
    )
    .await;

    let missing = put_access(&base, &admin, &uuid::Uuid::new_v4().to_string(), Some(true)).await;
    assert_eq!(missing, 404);

    let users: Value = http()
        .get(format!("{base}/v1/admin/users"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == allow.id.as_str())
        .unwrap();
    assert!(row["agent_access"].is_null());
    assert_eq!(row["agent_access_effective"], false);
    let adm = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == admin.id.as_str())
        .unwrap();
    assert_eq!(adm["agent_access_effective"], true);
}

/// 3. 收回使用权：running 收到取消并断开，queued 也取消；恢复后可以再调用。
#[tokio::test]
async fn agent_a08_revoke_cancels() {
    let base = spawn_named_db(&format!("chat_it_a08rv_{}", uniq())).await;
    let admin = register(&base, &format!("a08rd_{}", uniq())).await;
    let user = register(&base, &format!("a08ru_{}", uniq())).await;
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;

    let install = uuid::Uuid::new_v4();
    let (host_id, mut host_rx, host_keep) = raw_host(&base, &user, install).await;
    let (st, agent) = create_agent(&base, &user, &host_id, "runner").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let install2 = uuid::Uuid::new_v4();
    let (host2, mut rx2, host2_keep) = raw_host(&base, &user, install2).await;
    let (st, agent2) = create_agent(&base, &user, &host2, "waiter").await;
    assert_eq!(st, 200, "{agent2}");
    let conv2 = agent2["conversation_id"].as_str().unwrap().to_string();
    drop(host2_keep);
    host_until(&mut rx2, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    // 等服务端把这条宿主标离线，再发消息才会进排队而不是直接 running
    let off_deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let left = off_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "宿主 2 没有离线");
        let frames = until_frame(&mut su, "agent_host_status", left).await;
        if frames
            .iter()
            .any(|f| f["host_id"] == host2.as_str() && f["online"] == false)
        {
            break;
        }
    }

    send_ws(&mut su, &conv2, "q1", "echo:later").await;
    let queued = wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let qid = queued["invocation"]["id"].as_str().unwrap().to_string();

    send_ws(&mut su, &conv, "r1", "echo:hold").await;
    let running = wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(10),
    )
    .await;
    let rid = running["invocation"]["id"].as_str().unwrap().to_string();
    host_until(
        &mut host_rx,
        |_, v| v.is_some_and(|v| v["type"] == "invocation_start" && v["invocation_id"] == rid),
        A01Dur::from_secs(5),
    )
    .await;

    assert_eq!(put_access(&base, &admin, &user.id, Some(false)).await, 204);
    host_until(
        &mut host_rx,
        |_, v| v.is_some_and(|v| v["type"] == "invocation_cancel" && v["invocation_id"] == rid),
        A01Dur::from_secs(5),
    )
    .await;
    host_until(&mut host_rx, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(host_keep);

    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut saw_run = false;
    let mut saw_q = false;
    let mut saw_off = false;
    while !(saw_run && saw_q && saw_off) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "收回后的帧没收齐 run={saw_run} q={saw_q} off={saw_off}"
        );
        let msg = tokio::time::timeout(left, su.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "agent_invocation" {
            let id = v["invocation"]["id"].as_str().unwrap_or("");
            if v["invocation"]["status"] == "cancelled"
                && v["invocation"]["error"] == "agent_access_revoked"
            {
                if id == rid {
                    saw_run = true;
                }
                if id == qid {
                    saw_q = true;
                }
            }
        }
        if v["type"] == "agent_host_status" && v["host_id"] == host_id && v["online"] == false {
            saw_off = true;
        }
    }

    assert_eq!(put_access(&base, &admin, &user.id, Some(true)).await, 204);
    let (h, id2) = spawn_host(&base, &user, install).await;
    assert_eq!(id2, host_id);
    send_ws(&mut su, &conv, "r2", "echo:back").await;
    wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let rows = list_inv(&base, &user, &conv).await;
    let old = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == rid)
        .unwrap();
    assert_eq!(old["status"], "cancelled");
    assert_eq!(old["error"], "agent_access_revoked");
    let queued_rows = list_inv(&base, &user, &conv2).await;
    assert_eq!(queued_rows[0]["status"], "cancelled");
    assert_eq!(queued_rows[0]["error"], "agent_access_revoked");
    h.shutdown().await;
}

/// 4. 禁用账号：进行中的调用取消，宿主断开（online=false）。
#[tokio::test]
async fn agent_a08_disable_drops_host() {
    let base = spawn_named_db(&format!("chat_it_a08dis_{}", uniq())).await;
    let admin = register(&base, &format!("a08dd_{}", uniq())).await;
    let user = register(&base, &format!("a08du_{}", uniq())).await;
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;
    let (h, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let online = until_frame(&mut su, "agent_host_status", A01Dur::from_secs(5)).await;
    let (_, agent) = create_agent(&base, &user, &host_id, "sleeper").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    assert!(online
        .iter()
        .any(|f| f["host_id"] == host_id && f["online"] == true));
    send_ws(&mut su, &conv, "s1", "sleep:20000").await;
    wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;

    let r = http()
        .post(format!("{base}/v1/admin/users/{}/disable", user.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut saw_cancel = false;
    let mut saw_off = false;
    while !(saw_cancel && saw_off) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "禁用后没收齐 cancel={saw_cancel} off={saw_off}"
        );
        let msg = tokio::time::timeout(left, su.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "agent_invocation"
            && v["invocation"]["status"] == "cancelled"
            && v["invocation"]["error"] == "agent_access_revoked"
        {
            saw_cancel = true;
        }
        if v["type"] == "agent_host_status" && v["host_id"] == host_id && v["online"] == false {
            saw_off = true;
        }
    }
    h.shutdown().await;
}

/// 5. 移出宿主所用的登录会话会断开该宿主；改密只断开其他会话的宿主。
#[tokio::test]
async fn agent_a08_host_follows_session() {
    let base = spawn_named_db(&format!("chat_it_a08ses_{}", uniq())).await;
    let reg = register(&base, &format!("a08s_{}", uniq())).await;
    let dev_a = login_device(&base, &reg.username, "secret123", "设备甲").await;
    let dev_b = login_device(&base, &reg.username, "secret123", "设备乙").await;

    let (id_a, mut rx_a, keep_a) = raw_host(&base, &dev_a, uuid::Uuid::new_v4()).await;
    let (id_b, mut rx_b, keep_b) = raw_host(&base, &dev_b, uuid::Uuid::new_v4()).await;
    let (id_r, mut rx_r, keep_r) = raw_host(&base, &reg, uuid::Uuid::new_v4()).await;
    wait_online(&base, &dev_a.access, &id_a, true).await;
    wait_online(&base, &dev_a.access, &id_b, true).await;
    wait_online(&base, &dev_a.access, &id_r, true).await;

    let sessions = sessions_of(&base, &dev_a.access).await;
    let sid_b = sessions
        .iter()
        .find(|s| s["device_label"] == "设备乙")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let r = http()
        .delete(format!("{base}/v1/users/me/sessions/{sid_b}"))
        .header("authorization", authz(&dev_a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    host_until(&mut rx_b, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(keep_b);
    wait_online(&base, &dev_a.access, &id_b, false).await;
    wait_online(&base, &dev_a.access, &id_a, true).await;
    wait_online(&base, &dev_a.access, &id_r, true).await;

    let r = http()
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"secret456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let new_access = body["access_token"].as_str().unwrap();
    host_until(&mut rx_r, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(keep_r);
    wait_online(&base, new_access, &id_r, false).await;
    wait_online(&base, new_access, &id_a, true).await;
    let stray = tokio::time::timeout(A01Dur::from_millis(400), rx_a.recv()).await;
    if let Ok(Some(t)) = stray {
        assert_ne!(t, "__closed__", "当前会话的宿主不应被改密断开");
    }
    drop(keep_a);
}

/// 6. /users/me 的 agent_access 随设置变化，变化时收到 invalidate。
#[tokio::test]
async fn agent_a08_me_and_invalidate() {
    let base = spawn_named_db(&format!("chat_it_a08me_{}", uniq())).await;
    let admin = register(&base, &format!("a08md_{}", uniq())).await;
    let user = register(&base, &format!("a08mu_{}", uniq())).await;
    assert_eq!(me_json(&base, &user.access).await["agent_access"], true);

    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;

    assert_eq!(put_access(&base, &admin, &user.id, Some(false)).await, 204);
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);

    assert_eq!(put_access(&base, &admin, &user.id, None).await, 204);
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], true);

    put_settings(&base, &admin, json!({"agent_enabled": false})).await;
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);
    assert_eq!(me_json(&base, &admin.access).await["agent_access"], true);
}
