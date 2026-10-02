use super::*;

// ===== Agent A04 =====
// 离线排队、上线按创建顺序执行、超时清扫、取消。
// 断线后运行中的调用在宽限内保持（A23），重连且不报告该调用时才 failed。

pub(crate) async fn offline_agent(
    base: &str,
    u: &TestUser,
    name: &str,
) -> (String, String, uuid::Uuid) {
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(base, u, install).await;
    // A26：只能在这台电脑在线、且用宿主连接所在的登录会话时添加。先建好再断开。
    let (st, agent) = create_agent(base, u, &host_id, name).await;
    assert_eq!(st, 200, "{agent}");
    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    (conv, agent["id"].as_str().unwrap().to_string(), install)
}

/// 1. 离线发消息 → queued；上线后 running → 回复 → succeeded。
#[tokio::test]
async fn agent_a04_queue_then_run() {
    let base = spawn_named_db(&format!("chat_it_a04q_{}", uniq())).await;
    let a = register(&base, &format!("a04a_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "qagent").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "q1", "echo:ping").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    assert!(f["invocation"]["error"].is_null());
    let id = f["invocation"]["id"].as_str().unwrap().to_string();

    let (h, _) = spawn_host(&base, &a, install).await;
    let run = wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id && f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    assert!(run["invocation"]["started_at"].is_string());
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id && f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    assert_eq!(texts, vec!["ping".to_string()]);
    h.shutdown().await;
}

/// 2. 离线连发两条，上线后按创建顺序进入 running，各得到一条回复。
#[tokio::test]
async fn agent_a04_queue_order() {
    let base = spawn_named_db(&format!("chat_it_a04ord_{}", uniq())).await;
    let a = register(&base, &format!("a04b_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "order").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "o1", "echo:one").await;
    send_ws(&mut sa, &conv, "o2", "echo:two").await;
    let mut queued = Vec::new();
    while queued.len() < 2 {
        let f = wait_inv(
            &mut sa,
            |f| f["invocation"]["status"] == "queued",
            A01Dur::from_secs(10),
        )
        .await;
        let id = f["invocation"]["id"].as_str().unwrap().to_string();
        if !queued.contains(&id) {
            queued.push(id);
        }
    }

    let (h, _) = spawn_host(&base, &a, install).await;
    // 一条循环收齐，避免 running / succeeded 交错时把帧丢掉
    let mut running = Vec::new();
    let mut done = Vec::new();
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    while done.len() < 2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let f = wait_inv(
            &mut sa,
            |f| {
                let id = f["invocation"]["id"].as_str().unwrap_or("");
                queued.iter().any(|q| q.as_str() == id)
            },
            left,
        )
        .await;
        let id = f["invocation"]["id"].as_str().unwrap().to_string();
        let status = f["invocation"]["status"].as_str().unwrap_or("");
        if status == "running" && !running.contains(&id) {
            running.push(id.clone());
        }
        if status == "succeeded" && !done.contains(&id) {
            done.push(id);
        }
    }
    assert_eq!(running, queued, "应按创建顺序下发");
    let mut texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    texts.sort();
    assert_eq!(texts, vec!["one".to_string(), "two".to_string()]);
    h.shutdown().await;
}

/// 3. Agent 覆盖超时 2 秒、清扫间隔调小 → timed_out；之后上线不再执行。
#[tokio::test]
async fn agent_a04_queue_timeout() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04to_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a04c_{}", uniq())).await;
    let (conv, agent_id, install) = offline_agent(&base, &a, "slowq").await;
    let r = http()
        .patch(format!("{base}/v1/agents/{agent_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"queue_timeout_secs": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "t1", "echo:late").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "timed_out",
        A01Dur::from_secs(8),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();

    let (h, _) = spawn_host(&base, &a, install).await;
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    let rows = list_inv(&base, &a, &conv).await;
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(row["status"], "timed_out");
    assert!(agent_texts(&conv_messages(&base, &a, &conv).await).is_empty());
    h.shutdown().await;
}

/// 4. 个人默认 0（永不）→ 清扫间隔过了仍是 queued。
#[tokio::test]
async fn agent_a04_queue_never() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04nv_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a04d_{}", uniq())).await;
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&a.access))
        .json(&json!({"queue_timeout_secs": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (conv, _, _) = offline_agent(&base, &a, "forever").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "n1", "echo:stay").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();
    let frames = frames_for(&mut sa, A01Dur::from_millis(1200)).await;
    assert!(
        !frames
            .iter()
            .any(|f| f["invocation"]["id"] == id && f["invocation"]["status"] == "timed_out"),
        "永不超时不应被清扫: {frames:?}"
    );
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "queued");
}

/// 5. 取消 queued 不上线执行；取消 running 无回复；已完成 409；别人 404。
#[tokio::test]
async fn agent_a04_cancel_rules() {
    let base = spawn_named_db(&format!("chat_it_a04ca_{}", uniq())).await;
    let a = register(&base, &format!("a04e_{}", uniq())).await;
    let b = register(&base, &format!("a04f_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "can").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, _) = cancel_inv(&base, &a, &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(st, 404);

    send_ws(&mut sa, &conv, "c1", "echo:nope").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let qid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, _) = cancel_inv(&base, &b, &qid).await;
    assert_eq!(st, 404, "别人取消应 404");
    let (st, body) = cancel_inv(&base, &a, &qid).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == qid && f["invocation"]["status"] == "cancelled",
        A01Dur::from_secs(10),
    )
    .await;

    let (h, _) = spawn_host(&base, &a, install).await;
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    assert!(
        agent_texts(&conv_messages(&base, &a, &conv).await).is_empty(),
        "已取消的排队调用不应执行"
    );

    send_ws(&mut sa, &conv, "c2", "sleep:10000").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let rid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, body) = cancel_inv(&base, &a, &rid).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    assert!(
        !agent_texts(&conv_messages(&base, &a, &conv).await)
            .iter()
            .any(|t| t == "done"),
        "取消运行中的调用不应有回复"
    );

    send_ws(&mut sa, &conv, "c3", "echo:ok").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let sid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, body) = cancel_inv(&base, &a, &sid).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "already_finished");
    let (st, _) = cancel_inv(&base, &b, &sid).await;
    assert_eq!(st, 404);
    h.shutdown().await;
}

/// 命令行里带 --ignore-cancel 的 node 还在不在（Windows：用 PowerShell 查进程表）。
#[cfg(windows)]
fn ignore_cancel_nodes() -> usize {
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "@(Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object { $_.CommandLine -like '*--ignore-cancel*' }).Count",
        ])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// 6. --ignore-cancel：取消后约 5 秒子进程结束，宿主不再上报该调用。
#[cfg(windows)]
#[tokio::test]
async fn agent_a04_cancel_ignore() {
    let base = spawn_named_db(&format!("chat_it_a04ig_{}", uniq())).await;
    let a = register(&base, &format!("a04g_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(&base, &a, install).await;
    let mut args = fake_acp_args();
    args.push("--ignore-cancel".into());
    let (st, agent) = create_agent_with(&base, &a, &host_id, "stubborn", args).await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "i1", "sleep:30000").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();
    // 等子进程真正起来再取消
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let (st, body) = cancel_inv(&base, &a, &id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    tokio::time::sleep(A01Dur::from_millis(1000)).await;
    assert!(
        ignore_cancel_nodes() >= 1,
        "取消后 1 秒内不应立刻杀掉忽略取消的子进程"
    );
    tokio::time::sleep(A01Dur::from_millis(6500)).await;
    assert_eq!(ignore_cancel_nodes(), 0, "约 5 秒宽限后子进程应被结束");
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "cancelled");
    assert!(agent_texts(&conv_messages(&base, &a, &conv).await).is_empty());
    h.shutdown().await;
}

/// 7. 运行中断开：宽限内仍 running，排队保持；重连不报告该调用时才 failed，排队的接着执行。
#[tokio::test]
async fn agent_a04_disconnect_keeps_queue() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04dc_{}", uniq())).await;
    let a = register(&base, &format!("a04h_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(&base, &a, install).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "disc").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:20000").await;
    let fa = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let id_a = fa["invocation"]["id"].as_str().unwrap().to_string();
    send_ws(&mut sa, &conv, "d2", "sleep:4000").await;
    let fb = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running" && f["invocation"]["id"] != id_a,
        A01Dur::from_secs(15),
    )
    .await;
    let id_b = fb["invocation"]["id"].as_str().unwrap().to_string();
    // 在线时第二条也会直接 running。把它改回 queued，复现「断开时运行中与排队并存」。
    let n = sqlx::query(
        "UPDATE agent_invocations
            SET status = 'queued', started_at = NULL, finished_at = NULL,
                error = NULL, queue_deadline = NULL
          WHERE id = $1 AND status = 'running'",
    )
    .bind(uuid::Uuid::parse_str(&id_b).unwrap())
    .execute(&st.pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1, "第二条应还在 running，才能拨回 queued");

    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(500)).await;
    let rows = list_inv(&base, &a, &conv).await;
    let arow = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id_a)
        .unwrap();
    assert_eq!(arow["status"], "running", "断线后宽限内应保持运行: {arow}");
    let b = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id_b)
        .unwrap();
    assert_eq!(b["status"], "queued");

    // 新进程不报告旧调用 → 核对时判 host_disconnected，排队的按创建顺序执行。
    let (h2, _) = spawn_host(&base, &a, install).await;
    let failed = wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id_a && f["invocation"]["status"] == "failed",
        A01Dur::from_secs(15),
    )
    .await;
    assert_eq!(failed["invocation"]["error"], "host_disconnected");
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id_b && f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(20),
    )
    .await;
    let texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    assert_eq!(texts, vec!["done".to_string()]);
    let rows = list_inv(&base, &a, &conv).await;
    let arow = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id_a)
        .unwrap();
    assert_eq!(arow["status"], "failed");
    h2.shutdown().await;
}

// ===== Agent A03：三种输出方式 + 调用状态标签 =====

/// 收集帧直到指定调用进入终态（succeeded/failed），返回含终态帧的全部帧。
async fn until_inv_done<S>(s: &mut S, inv_id: &str, timeout: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 {inv_id} 终态超时；已收到: {out:?}");
        match tokio::time::timeout(left, s.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let done = v["type"] == "agent_invocation"
                        && v["invocation"]["id"] == inv_id
                        && matches!(
                            v["invocation"]["status"].as_str(),
                            Some("succeeded") | Some("failed")
                        );
                    out.push(v);
                    if done {
                        return out;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("等 {inv_id} 终态时 ws 异常: {other:?}; 已收到: {out:?}"),
        }
    }
}

/// A03 夹具：在已连接的会话里发送 prompt 并等到 running 帧，返回调用 id。
async fn a03_invoke<S>(conv: &str, prompt: &str, sa: &mut S) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMsg>
        + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    send_ws(sa, conv, &format!("a03{}", uniq()), prompt).await;
    let frames = until_frame(sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .expect("应有 running 帧");
    assert_eq!(inv["invocation"]["status"], "running");
    inv["invocation"]["id"].as_str().unwrap().to_string()
}

/// stream：agent_stream 帧（含 delta 与 step）、历史无中间内容、定稿带 2 条 ok 步骤。
#[tokio::test]
async fn agent_a03_stream_mode() {
    let base = spawn_named_db(&format!("chat_t_a03s_{}", uniq())).await;
    let a = register(&base, &format!("a03s_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "streamer").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let mut frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(400)).await);

    let streams: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "agent_stream")
        .collect();
    assert!(
        !streams.is_empty(),
        "stream 模式应收到 agent_stream: {frames:?}"
    );
    assert!(streams
        .iter()
        .all(|f| f["invocation_id"] == inv_id && f["conversation_id"] == conv.as_str()));
    assert!(
        streams.iter().any(|f| f["delta"].is_string()),
        "至少一帧带 delta: {streams:?}"
    );
    let step_frames: Vec<&&Value> = streams.iter().filter(|f| f["step"].is_object()).collect();
    assert!(
        step_frames
            .iter()
            .any(|f| f["step"]["id"] == "step-1" && f["step"]["ok"].is_null()),
        "应有 step-1 的进行中帧: {step_frames:?}"
    );
    assert!(
        step_frames
            .iter()
            .any(|f| f["step"]["id"] == "step-1" && f["step"]["ok"] == true)
            && step_frames
                .iter()
                .any(|f| f["step"]["id"] == "step-2" && f["step"]["ok"] == true),
        "两个步骤都应有完成帧: {step_frames:?}"
    );
    // 终态帧之后不再有 agent_stream
    let tail: Vec<&Value> = frames
        .iter()
        .skip_while(|f| {
            !(f["type"] == "agent_invocation"
                && f["invocation"]["id"] == inv_id
                && f["invocation"]["status"] == "succeeded")
        })
        .collect();
    assert!(
        !tail.iter().any(|f| f["type"] == "agent_stream"),
        "终态后不应再有 agent_stream: {tail:?}"
    );

    // 定稿消息：Agent 归属 + agent_steps 两条且全 ok
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有定稿 Agent 消息");
    let steps = agent_msg["message"]["agent_steps"]
        .as_array()
        .expect("定稿消息应带 agent_steps");
    assert_eq!(steps.len(), 2);
    assert!(steps.iter().all(|s| s["ok"] == true));

    // 历史：触发消息 + 一条定稿，没有任何中间增量消息
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2, "历史只应有触发消息和定稿: {list:?}");
    assert!(list[0]["agent"].is_null());
    assert_eq!(list[1]["agent_steps"].as_array().unwrap().len(), 2);
    assert!(list[1]["content"].as_str().unwrap().ends_with("done"));
    ha.shutdown().await;
}

/// status：不推增量；步骤变化推带 current_step 的 agent_invocation；结束一条结果消息。
#[tokio::test]
async fn agent_a03_status_mode() {
    let base = spawn_named_db(&format!("chat_t_a03st_{}", uniq())).await;
    let a = register(&base, &format!("a03st_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "stater").await;
    assert_eq!(st, 200);
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    // 单个 Agent 覆盖为 status
    let r = http()
        .patch(format!("{base}/v1/agents/{agent_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"output_mode": "status"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let mut frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(400)).await);

    assert!(
        !frames.iter().any(|f| f["type"] == "agent_stream"),
        "status 模式不得推 agent_stream: {frames:?}"
    );
    // running 期间应有带 current_step 的状态帧；终态帧 current_step 清空
    assert!(
        frames.iter().any(|f| f["type"] == "agent_invocation"
            && f["invocation"]["id"] == inv_id
            && f["invocation"]["current_step"].is_string()),
        "应有带 current_step 的状态帧: {frames:?}"
    );
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .unwrap();
    assert!(term["invocation"]["current_step"].is_null());

    // 恰好一条 Agent 结果消息（无步骤消息），带 agent_steps
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2, "status 模式只应有触发 + 结果两条: {list:?}");
    let steps = list[1]["agent_steps"].as_array().expect("结果消息带步骤");
    assert_eq!(steps.len(), 2);
    assert!(list[1]["content"].as_str().unwrap().ends_with("done"));
    ha.shutdown().await;
}

/// steps：个人默认设为 steps、Agent 不覆盖 → 每步一条 ✓ 消息，正文最后且 agent_steps 为空。
#[tokio::test]
async fn agent_a03_steps_mode() {
    let base = spawn_named_db(&format!("chat_t_a03p_{}", uniq())).await;
    let a = register(&base, &format!("a03p_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "stepper").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    // 只设个人默认；Agent 覆盖保持 null（跟随默认）
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&a.access))
        .json(&json!({"output_mode": "steps"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;

    // 不推 agent_stream（每步消息走普通 message 帧）
    assert!(!frames.iter().any(|f| f["type"] == "agent_stream"));

    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 4, "触发 + 2 条步骤 + 正文: {list:?}");
    assert_eq!(list[1]["content"], "✓ 步骤 1");
    assert_eq!(list[2]["content"], "✓ 步骤 2");
    assert!(list[1]["agent"].is_object() && list[2]["agent"].is_object());
    let body = &list[3];
    assert!(body["agent"].is_object());
    assert!(body["content"].as_str().unwrap().ends_with("done"));
    assert!(
        body["agent_steps"].is_null()
            || body["agent_steps"].as_array().is_some_and(|a| a.is_empty()),
        "steps 模式正文的 agent_steps 应为空: {body:?}"
    );
    ha.shutdown().await;
}

/// stepfail：步骤记录 ok=false，调用仍 succeeded。
#[tokio::test]
async fn agent_a03_stepfail() {
    let base = spawn_named_db(&format!("chat_t_a03f_{}", uniq())).await;
    let a = register(&base, &format!("a03f_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "failer").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let inv_id = a03_invoke(&conv, "stepfail", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id)
        .unwrap();
    assert_eq!(term["invocation"]["status"], "succeeded");

    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let steps = list[1]["agent_steps"].as_array().expect("应有步骤记录");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["ok"], false);
    assert_eq!(steps[0]["title"], "读取配置");
    ha.shutdown().await;
}

/// 限频：一次调用产生的 agent_stream 帧数 ≤ 时长/200ms + 少量余量。
#[tokio::test]
async fn agent_a03_stream_rate_limit() {
    let base = spawn_named_db(&format!("chat_t_a03r_{}", uniq())).await;
    let a = register(&base, &format!("a03r_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "dripper").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // drip:1500 → 假 agent 每 ~30ms 一个 chunk，全程 ~1.5s；不限频会产生 ~50 帧
    let t0 = std::time::Instant::now();
    let inv_id = a03_invoke(&conv, "drip:1500", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(20)).await;
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let n = frames
        .iter()
        .filter(|f| f["type"] == "agent_stream")
        .count();
    let bound = elapsed_ms / 200 + 5;
    assert!(
        n >= 2 && n as u64 <= bound,
        "agent_stream 帧数 {n} 应 ≤ {bound}（时长 {elapsed_ms}ms /200ms + 余量）且 ≥2"
    );
    ha.shutdown().await;
}

/// 别的宿主伪造 invocation_event：被忽略，调用不受影响。
#[tokio::test]
async fn agent_a03_event_forgery_ignored() {
    let base = spawn_named_db(&format!("chat_t_a03g_{}", uniq())).await;
    let a = register(&base, &format!("a03g_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "real").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // 先让真实调用跑起来（sleep:2000 保证伪造帧落在 running 窗口内）
    let inv_id = a03_invoke(&conv, "sleep:2000", &mut sa).await;

    // 另开一条宿主连接（不同 install_id → 不同 host_id）：伪造者
    let ticket: Value = http()
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let host_ws_url = format!(
        "{}/v1/ws?ticket={}&role=host",
        base.replacen("http://", "ws://", 1),
        ticket["ticket"].as_str().unwrap()
    );
    let (mut hb, _) = tokio_tungstenite::connect_async(host_ws_url).await.unwrap();
    hb.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),
               "label":"forge","platform":"windows"})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = until_frame(&mut hb, "host_ready", A01Dur::from_secs(10)).await;
    let forge_host = ready.iter().find(|f| f["type"] == "host_ready").unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(forge_host, host_id, "伪造者应是不相关的另一条宿主连接");

    // 伪造三类事件 + 伪造 finish：全部应被忽略
    for ev in [
        json!({"type":"text","text":"FORGED"}),
        json!({"type":"step_start","id":"evil-1","title":"伪造步骤","kind":"other"}),
        json!({"type":"step_end","id":"evil-1","ok":true}),
    ] {
        hb.send(WsMsg::Text(
            json!({"type":"invocation_event","invocation_id":inv_id,"event":ev})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    hb.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv_id,"ok":true,"text":"FORGED BODY"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // 自然终态（sleep:2000 → 真宿主回 done）
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "agent_stream" && serde_json::to_string(f).unwrap().contains("FORGED")
        }),
        "伪造事件不得产生 agent_stream: {frames:?}"
    );
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id)
        .unwrap();
    // 伪造 finish 不会抢先终结；真宿主结束为 succeeded
    assert_eq!(term["invocation"]["status"], "succeeded");
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有定稿消息");
    assert_eq!(agent_msg["message"]["content"], "done");
    ha.shutdown().await;
}
