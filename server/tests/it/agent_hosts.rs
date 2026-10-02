use super::*;

// ===== Agent A09 =====
// 宿主名称只在第一次 host_hello 写入；之后改名走 PATCH，重连不再覆盖。

async fn spawn_host_labeled(
    base: &str,
    u: &TestUser,
    install_id: uuid::Uuid,
    label: &str,
    platform: &str,
) -> (agent_host::HostHandle, String) {
    let h = agent_host::spawn(
        agent_host::HostConfig {
            server_url: base.into(),
            install_id,
            label: label.into(),
            platform: platform.into(),
            mcp_bridge: None,
            builtin_agent: None,
            registry_url: agent_host::DEFAULT_REGISTRY_URL.into(),
            data_dir: std::env::temp_dir().join("chat-agent-host"),
        },
        std::sync::Arc::new(AgentTickets {
            base: base.into(),
            access: u.access.clone(),
        }),
    );
    let id = tokio::time::timeout(A01Dur::from_secs(10), h.wait_host_id())
        .await
        .expect("host_ready 超时")
        .expect("未能拿到 host_id");
    (h, id.to_string())
}

async fn patch_host(base: &str, u: &TestUser, host_id: &str, label: &str) -> (u16, Value) {
    let r = http()
        .patch(format!("{base}/v1/agent-hosts/{host_id}"))
        .header("authorization", authz(&u.access))
        .json(&json!({ "label": label }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

async fn agent_hosts_of(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// 首次 host_hello 写入 label；改名后重连带另一个 label，名称保持改后的值，平台仍更新。
#[tokio::test]
async fn agent_a09_label_written_once() {
    let base = spawn_named_db(&format!("chat_it_a09lab_{}", uniq())).await;
    let a = register(&base, &format!("a09a_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h1, host_id) = spawn_host_labeled(&base, &a, install, "first-pc", "windows").await;

    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(hosts[0]["label"], "first-pc");
    assert_eq!(hosts[0]["platform"], "windows");

    let (st, body) = patch_host(&base, &a, &host_id, "  家里 Windows  ").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "家里 Windows");
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "家里 Windows");

    h1.shutdown().await;
    let (h2, host_id2) = spawn_host_labeled(&base, &a, install, "other-label", "linux").await;
    assert_eq!(host_id2, host_id);
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts.as_array().unwrap().len(), 1);
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(
        hosts[0]["label"], "家里 Windows",
        "重连的 host_hello 不得覆盖已改名称"
    );
    assert_eq!(hosts[0]["platform"], "linux", "重连仍更新 platform");
    h2.shutdown().await;
}

/// PATCH：空 / 超长 400；别人的宿主 404；改名后主人的普通连接收到带新 label 的更新。
#[tokio::test]
async fn agent_a09_patch_host_label() {
    let base = spawn_named_db(&format!("chat_it_a09pat_{}", uniq())).await;
    let a = register(&base, &format!("a09c_{}", uniq())).await;
    let b = register(&base, &format!("a09d_{}", uniq())).await;
    // 主人先连上，才能收到随后的上线帧和改名帧
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (h, host_id) = spawn_host_labeled(&base, &a, uuid::Uuid::new_v4(), "pc", "windows").await;
    until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let (st, body) = patch_host(&base, &a, &host_id, "书房").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "书房");
    assert_eq!(body["id"], host_id.as_str());
    assert_eq!(body["online"], true);

    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let upd = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(upd["host_id"], host_id.as_str());
    assert_eq!(upd["online"], true);
    assert_eq!(upd["label"], "书房");

    let others = frames_for(&mut sb, A01Dur::from_millis(400)).await;
    assert!(
        !others.iter().any(|f| f["type"] == "agent_host_status"),
        "改名只推给主人: {others:?}"
    );

    let too_long = "x".repeat(61);
    let too_long_han = "测".repeat(61);
    for bad in ["", "   ", too_long.as_str(), too_long_han.as_str()] {
        let (st, body) = patch_host(&base, &a, &host_id, bad).await;
        assert_eq!(st, 400, "label {bad:?} 应 400: {body}");
        assert_eq!(body["error"], "bad_label");
    }
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "书房", "非法改名不得落库");

    let sixty = "测".repeat(60);
    let (st, body) = patch_host(&base, &a, &host_id, &sixty).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], sixty);

    let padded = format!("  {}  ", "y".repeat(60));
    let (st, body) = patch_host(&base, &a, &host_id, &padded).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "y".repeat(60));

    let (st, body) = patch_host(&base, &b, &host_id, "stolen").await;
    assert_eq!(st, 404, "别人的宿主应 404: {body}");
    let (st, body) = patch_host(&base, &a, &uuid::Uuid::new_v4().to_string(), "nope").await;
    assert_eq!(st, 404, "不存在的宿主应 404: {body}");

    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "y".repeat(60));
    let b_hosts = agent_hosts_of(&base, &b).await;
    assert!(b_hosts.as_array().unwrap().is_empty());
    h.shutdown().await;
}

// ===== Agent A23 =====
// 宿主掉线不终止进行中的调用；重连按 host_hello 核对并补发取消 / 审批决定。
// 取消与排队转运行并发时，start 不会落在最后一条 cancel 之后。

struct A23Run {
    host: RawHostWs,
    user: RawHostWs,
    inv_id: String,
    conv: String,
    host_id: String,
    install: uuid::Uuid,
}

async fn a23_open_host(
    base: &str,
    u: &TestUser,
    install: uuid::Uuid,
    running: &[String],
    pending: &[String],
) -> (RawHostWs, String) {
    let (mut s, _) =
        tokio_tungstenite::connect_async(format!("{}&role=host", ws_url(base, u).await))
            .await
            .unwrap();
    s.send(WsMsg::Text(
        json!({
            "type": "host_hello",
            "install_id": install,
            "label": "a23",
            "platform": "test",
            "running": running,
            "pending_approvals": pending,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = next_of(&mut s, "host_ready").await;
    (s, ready["host_id"].as_str().unwrap().to_string())
}

async fn a23_running(base: &str, u: &TestUser) -> A23Run {
    let install = uuid::Uuid::new_v4();
    let (mut host, host_id) = a23_open_host(base, u, install, &[], &[]).await;
    let (code, agent) = create_agent(base, u, &host_id, "a23bot").await;
    assert_eq!(code, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut user, _) = tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap();
    send_ws(&mut user, &conv, &format!("a23-{}", uniq()), "hold").await;
    let frames = until_frame(&mut host, "invocation_start", A01Dur::from_secs(10)).await;
    let inv_id = frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap()["invocation_id"]
        .as_str()
        .unwrap()
        .to_string();
    A23Run {
        host,
        user,
        inv_id,
        conv,
        host_id,
        install,
    }
}

fn a23_row<'a>(rows: &'a Value, id: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap()
}

/// 运行中断线仍是 running；重连报告该 id（并清掉内存快照）后，事件和 finish 都被受理。
#[tokio::test]
async fn a23_reconnect_reports_running_and_finish_lands() {
    let (base, st) = spawn_named_state(&format!("chat_it_a23fin_{}", uniq())).await;
    let a = register(&base, &format!("a23f_{}", uniq())).await;
    let mut run = a23_running(&base, &a).await;
    run.host.close(None).await.ok();
    drop(run.host);

    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(a23_row(&rows, &run.inv_id)["status"], "running", "{rows}");

    let inv_uuid = uuid::Uuid::parse_str(&run.inv_id).unwrap();
    assert!(
        st.inv_runs.remove(&inv_uuid).is_some(),
        "断线不应摘掉运行时条目"
    );

    let (mut host, _) = a23_open_host(&base, &a, run.install, &[run.inv_id.clone()], &[]).await;
    assert!(
        st.inv_runs.contains_key(&inv_uuid),
        "重连报告后应重建运行时条目"
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut user = run.user;
    tokio::spawn(async move {
        while let Some(Ok(msg)) = user.next().await {
            if let WsMsg::Text(t) = msg {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let _ = tx.send(v);
                }
            }
        }
    });
    host.send(WsMsg::Text(
        json!({
            "type": "invocation_event",
            "invocation_id": run.inv_id,
            "event": {"type": "text", "text": "live-bit"}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "重建后的调用应推流式帧");
        let v = tokio::time::timeout(left, rx.recv())
            .await
            .expect("等流式帧超时")
            .expect("用户连接已关");
        if v["type"] == "agent_stream" && v["delta"] == "live-bit" {
            break;
        }
    }

    host_finish(&mut host, &run.inv_id).await;
    // host_finish 写的是 "done"。再发一条带正文的会当重复 finish 被忽略。
    // 上面 helper 固定 text=done，这里直接再断言 done。
    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    loop {
        let rows = list_inv(&base, &a, &run.conv).await;
        if a23_row(&rows, &run.inv_id)["status"] == "succeeded" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "补发的 finish 应被受理: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
    let texts = agent_texts(&conv_messages(&base, &a, &run.conv).await);
    assert_eq!(texts, vec!["done".to_string()], "{texts:?}");
    assert!(st.inv_runs.get(&inv_uuid).is_none());
}

/// 重连但不报告该调用 → failed/host_disconnected。
#[tokio::test]
async fn a23_reconnect_without_report_fails() {
    let base = spawn_named_db(&format!("chat_it_a23miss_{}", uniq())).await;
    let a = register(&base, &format!("a23m_{}", uniq())).await;
    let mut run = a23_running(&base, &a).await;
    run.host.close(None).await.ok();
    drop(run.host);
    tokio::time::sleep(A01Dur::from_millis(200)).await;
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(a23_row(&rows, &run.inv_id)["status"], "running");

    let (_host, _) = a23_open_host(&base, &a, run.install, &[], &[]).await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let rows = list_inv(&base, &a, &run.conv).await;
        let row = a23_row(&rows, &run.inv_id);
        if row["status"] == "failed" {
            assert_eq!(row["error"], "host_disconnected", "{row}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "不报告的调用应失败: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

/// 离线期间取消：重连报告该 id 时收到 invocation_cancel。
#[tokio::test]
async fn a23_offline_cancel_replayed_on_reconnect() {
    let base = spawn_named_db(&format!("chat_it_a23can_{}", uniq())).await;
    let a = register(&base, &format!("a23c_{}", uniq())).await;
    let mut run = a23_running(&base, &a).await;
    run.host.close(None).await.ok();
    drop(run.host);
    tokio::time::sleep(A01Dur::from_millis(200)).await;

    let (code, body) = cancel_inv(&base, &a, &run.inv_id).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["status"], "cancelled");

    let (mut host, _) = a23_open_host(&base, &a, run.install, &[run.inv_id.clone()], &[]).await;
    let frames = until_frame(&mut host, "invocation_cancel", A01Dur::from_secs(5)).await;
    let cancel = frames
        .iter()
        .find(|f| f["type"] == "invocation_cancel")
        .unwrap();
    assert_eq!(cancel["invocation_id"], run.inv_id);
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(a23_row(&rows, &run.inv_id)["status"], "cancelled");
}

/// 离线期间决定审批：重连后按报告的 request_id 补发 approval_decision。
#[tokio::test]
async fn a23_offline_approval_decision_replayed() {
    let base = spawn_named_db(&format!("chat_it_a23ap_{}", uniq())).await;
    let a = register(&base, &format!("a23a_{}", uniq())).await;
    let mut run = a23_running(&base, &a).await;
    a22_send_approval(&mut run.host, &run.inv_id, "a23-req", "离线审批").await;
    let card = a22_wait_approval(&base, &a, &run.conv, "a23-req").await;
    assert_eq!(
        a22_inv_status(&base, &a, &run.conv).await,
        "awaiting_approval"
    );

    run.host.close(None).await.ok();
    drop(run.host);
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    assert_eq!(
        a22_inv_status(&base, &a, &run.conv).await,
        "awaiting_approval"
    );

    let approval_id = card["approval"]["id"].as_str().unwrap();
    let (code, body) = decide_approval(&base, &a, approval_id, "allow").await;
    assert_eq!(code, 200, "{body}");

    let (mut host, _) = a23_open_host(
        &base,
        &a,
        run.install,
        &[run.inv_id.clone()],
        &["a23-req".into()],
    )
    .await;
    let dec = a22_wait_decision(&mut host, "a23-req").await;
    assert_eq!(dec["option_id"], "allow", "{dec}");
    assert_eq!(a22_inv_status(&base, &a, &run.conv).await, "running");
}

/// 宽限期看数据库里的 offline_since，不看内存。调小原子量后，回拨起点才会失败。
#[tokio::test]
async fn a23_grace_reads_offline_since() {
    let (base, st) = spawn_named_state(&format!("chat_it_a23gr_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    st.host_offline_grace_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a23g_{}", uniq())).await;
    let mut run = a23_running(&base, &a).await;
    let host_uuid = uuid::Uuid::parse_str(&run.host_id).unwrap();
    run.host.close(None).await.ok();
    drop(run.host);

    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let set: bool =
            sqlx::query_scalar("SELECT offline_since IS NOT NULL FROM agent_hosts WHERE id = $1")
                .bind(host_uuid)
                .fetch_one(&st.pool)
                .await
                .unwrap();
        if set {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "断开后应写下离线起点");
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
    // 内存快照丢掉也不影响宽限（重启后只剩这一列）。
    st.inv_runs.clear();
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(a23_row(&rows, &run.inv_id)["status"], "running");

    sqlx::query(
        "UPDATE agent_hosts SET offline_since = now() - interval '30 seconds' WHERE id = $1",
    )
    .bind(host_uuid)
    .execute(&st.pool)
    .await
    .unwrap();
    tokio::time::sleep(A01Dur::from_millis(700)).await;
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(
        a23_row(&rows, &run.inv_id)["status"],
        "running",
        "30 秒前离线、宽限 60 秒，不应失败: {rows}"
    );

    sqlx::query(
        "UPDATE agent_hosts SET offline_since = now() - interval '2 minutes' WHERE id = $1",
    )
    .bind(host_uuid)
    .execute(&st.pool)
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let rows = list_inv(&base, &a, &run.conv).await;
        let row = a23_row(&rows, &run.inv_id);
        if row["status"] == "failed" {
            assert_eq!(row["error"], "host_disconnected", "{row}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "超过宽限应失败: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

/// 在线宿主的 offline_since 被拨到过去：清扫不清调用，并把该列清掉。
#[tokio::test]
async fn a23_grace_sweep_leaves_online_host_running() {
    let (base, st) = spawn_named_state(&format!("chat_it_a23on_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    st.host_offline_grace_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a23on_{}", uniq())).await;
    let run = a23_running(&base, &a).await;
    let host_uuid = uuid::Uuid::parse_str(&run.host_id).unwrap();
    assert!(st.host_online(host_uuid), "宿主连接应仍在");

    // 读走服务端的 ping，避免这条连接被写成卡住后拆掉。
    let mut host = run.host;
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = host.next().await {} });

    sqlx::query(
        "UPDATE agent_hosts SET offline_since = now() - interval '2 minutes' WHERE id = $1",
    )
    .bind(host_uuid)
    .execute(&st.pool)
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let rows = list_inv(&base, &a, &run.conv).await;
        let row = a23_row(&rows, &run.inv_id);
        assert_eq!(
            row["status"], "running",
            "在线宿主不应被宽限清扫判失败: {row}"
        );
        let cleared: bool =
            sqlx::query_scalar("SELECT offline_since IS NULL FROM agent_hosts WHERE id = $1")
                .bind(host_uuid)
                .fetch_one(&st.pool)
                .await
                .unwrap();
        if cleared {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "清扫应清掉在线宿主的 offline_since"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let rows = list_inv(&base, &a, &run.conv).await;
    assert_eq!(a23_row(&rows, &run.inv_id)["status"], "running");
    let still_clear: bool =
        sqlx::query_scalar("SELECT offline_since IS NULL FROM agent_hosts WHERE id = $1")
            .bind(host_uuid)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert!(still_clear, "在线宿主的离线起点应保持清空");
    reader.abort();
}

/// 排队转运行和取消并发：宿主要么看不到 start，要么 start 后面还有 cancel。
#[tokio::test]
async fn a23_cancel_races_queue_flush() {
    let (base, st) = spawn_named_state(&format!("chat_it_a23race_{}", uniq())).await;
    let a = register(&base, &format!("a23r_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (mut first, host_id) = a23_open_host(&base, &a, install, &[], &[]).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "racer").await;
    assert_eq!(code, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    first.close(None).await.ok();
    drop(first);
    tokio::time::sleep(A01Dur::from_millis(200)).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..12 {
        send_ws(&mut sa, &conv, &format!("race-{i}"), "echo:x").await;
    }
    while ids.len() < 12 {
        let f = wait_inv(
            &mut sa,
            |f| f["invocation"]["status"] == "queued",
            A01Dur::from_secs(10),
        )
        .await;
        let id = f["invocation"]["id"].as_str().unwrap().to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    let (hb, _) =
        tokio_tungstenite::connect_async(format!("{}&role=host", ws_url(&base, &a).await))
            .await
            .unwrap();
    let (mut sink, mut stream) = hb.split();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            if let WsMsg::Text(t) = msg {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let _ = frame_tx.send(v);
                }
            }
        }
    });

    let base_c = base.clone();
    let access = a.access.clone();
    let ids_c = ids.clone();
    let canceller = tokio::spawn(async move {
        let u = TestUser {
            id: String::new(),
            username: String::new(),
            access,
            refresh: String::new(),
        };
        for _ in 0..50 {
            for id in &ids_c {
                let _ = cancel_inv(&base_c, &u, id).await;
            }
            tokio::time::sleep(A01Dur::from_millis(5)).await;
        }
    });

    sink.send(WsMsg::Text(
        json!({
            "type": "host_hello",
            "install_id": install,
            "label": "a23",
            "platform": "test",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + A01Dur::from_secs(10);
    loop {
        let rows = list_inv(&base, &a, &conv).await;
        let done = rows.as_array().unwrap().iter().all(|r| {
            matches!(
                r["status"].as_str(),
                Some("cancelled") | Some("succeeded") | Some("failed")
            )
        });
        if done && rows.as_array().unwrap().len() >= 12 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "取消应在超时前落定: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(30)).await;
    }
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    canceller.abort();
    drop(sink);

    let mut frames = Vec::new();
    while let Ok(v) = frame_rx.try_recv() {
        frames.push(v);
    }
    for id in &ids {
        let mut saw_start = false;
        let mut cancel_after_start = false;
        for f in &frames {
            if f["invocation_id"].as_str() != Some(id.as_str()) {
                continue;
            }
            match f["type"].as_str() {
                Some("invocation_start") => {
                    saw_start = true;
                    cancel_after_start = false;
                }
                Some("invocation_cancel") if saw_start => cancel_after_start = true,
                _ => {}
            }
        }
        if saw_start {
            assert!(
                cancel_after_start,
                "{id} 收到 start 之后没有 cancel；帧={frames:?}"
            );
        }
        let uid = uuid::Uuid::parse_str(id).unwrap();
        assert!(
            st.inv_runs.get(&uid).is_none(),
            "{id} 取消后不应残留运行时条目"
        );
    }
}

// ===== Agent A24 =====
// 宿主连接复核登录会话：register_host 后立刻复核一次，堵住「升级后、hello 前」
// 撤销/禁用/改密的缝隙；登记后读循环每个 ping 周期复核一次，失效按正常断开收尾。

/// 升级后暂不 hello 的裸宿主连接（票据用 dev 的 access 换，绑定 dev 的登录会话）。
async fn a24_pending_host(base: &str, dev: &TestUser) -> RawHostWs {
    let url = format!("{}&role=host", ws_url(base, dev).await);
    tokio_tungstenite::connect_async(&url).await.unwrap().0
}

/// 读宿主连接直到关闭（最多 dur）；返回期间收到的文本帧。
/// 未关闭返回 None。
async fn a24_until_close(sock: &mut RawHostWs, dur: A01Dur) -> Option<Vec<Value>> {
    let mut frames = Vec::new();
    let done = tokio::time::timeout(dur, async {
        loop {
            match sock.next().await {
                Some(Ok(WsMsg::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        frames.push(v);
                    }
                }
                // Ping/Pong 等控制帧不算应用帧
                Some(Ok(_)) => {}
                _ => break,
            }
        }
    })
    .await
    .is_ok();
    done.then_some(frames)
}

/// hello 前撤销会话：kick_session 只断开已登记宿主，漏掉这条挂起连接；
/// 它随后 hello → 登记后复核不过 → 关闭，没有 host_ready，宿主不在线。
#[tokio::test]
async fn a24_host_revoked_before_hello_never_registers() {
    let base = spawn().await;
    let u = register(&base, &format!("a24a_{}", uniq())).await;
    let dev = login_device(&base, &u.username, "secret123", "hostdev").await;

    // 宿主角色升级成功但暂不 hello（改过的客户端可把 hello 拖近 10 秒上限）
    let mut host = a24_pending_host(&base, &dev).await;

    // 另一台设备把宿主所在登录会话移出：disconnect_hosts 找不到未登记的它
    let list = sessions_of(&base, &u.access).await;
    let sid = list
        .iter()
        .find(|s| s["device_label"] == "hostdev")
        .expect("找不到 hostdev 的登录会话")["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{sid}"),
            &u.access
        )
        .await,
        204
    );

    // 现在才 hello：登记后复核发现会话已撤销 → 关连接，不发 host_ready
    host.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),
               "label":"stolen","platform":"test"})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let frames = a24_until_close(&mut host, A01Dur::from_secs(3))
        .await
        .expect("已撤销会话的宿主连接应被关闭");
    assert!(
        frames.iter().all(|f| f["type"] != "host_ready"),
        "不应收到 host_ready: {frames:?}"
    );

    // 宿主行落库（install_id 先 upsert）但登记已被摘掉 → 不在线
    let hosts: Value = http()
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stolen = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["label"] == "stolen")
        .expect("宿主行应已落库");
    assert_eq!(stolen["online"], false, "{hosts}");
}

/// 已登记宿主的读循环复核：直接在库里撤会话（绕开 kick_session，模拟漏网），
/// ping 周期调小 → 下一次 ping 复核失败 → 正常断开、宿主离线。
#[tokio::test]
async fn a24_registered_host_disconnects_on_ping_after_revoke() {
    let (base, st) = spawn_named_state(&format!("chat_it_a24_{}", uniq())).await;
    st.ws_ping_interval_ms
        .store(60, std::sync::atomic::Ordering::Relaxed);
    let u = register(&base, &format!("a24b_{}", uniq())).await;
    let (mut host, host_id) = spawn_raw_host(&base, &u).await;
    let hid = uuid::Uuid::parse_str(&host_id).unwrap();
    assert!(st.host_online(hid));

    // 绕开 kick_session 直接在库里撤销该登录会话（模拟踢设备漏掉这条宿主）
    let uid = uuid::Uuid::parse_str(&u.id).unwrap();
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE user_id = $1")
        .bind(uid)
        .execute(&st.pool)
        .await
        .unwrap();

    // 下一次 ping 复核发现失效 → 正常断开：关连接、走离线收尾
    let frames = a24_until_close(&mut host, A01Dur::from_secs(5))
        .await
        .expect("被撤销会话的已登记宿主应在下一次 ping 后断开");
    assert!(
        frames.iter().all(|f| f["type"] != "host_ready"),
        "{frames:?}"
    );

    // 离线收尾已跑：宿主不再在线
    let deadline = std::time::Instant::now() + A01Dur::from_secs(3);
    while st.host_online(hid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(A01Dur::from_millis(20)).await;
    }
    assert!(!st.host_online(hid), "断开后宿主应离线");
}
