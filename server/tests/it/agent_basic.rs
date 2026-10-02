use super::*;

// ===== Agent A01 =====
// 端到端跑真 agent-host 库 + node tests/fake-acp-agent.mjs（需 PATH 有 node）。

impl agent_host::TicketSource for AgentTickets {
    fn ticket(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>> {
        let (base, access) = (self.base.clone(), self.access.clone());
        Box::pin(async move {
            let v: Value = http()
                .get(format!("{base}/v1/ws/ticket"))
                .header("authorization", authz(&access))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            v["ticket"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "no ticket".into())
        })
    }
}

/// REST 授权与校验：别人的宿主/Agent 列表不可见、按 id 建 Agent 404、
/// 名字规则 400、大小写不敏感重名 409。
#[tokio::test]
async fn agent_rest_scope_and_validation() {
    let base = spawn_named_db(&format!("chat_t_a01rest_{}", uniq())).await;
    let a = register(&base, &format!("a01a_{}", uniq())).await;
    let b = register(&base, &format!("a01b_{}", uniq())).await;
    let c = http();

    // 未连宿主前 /agent-hosts 为空
    let hosts: Value = c
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts.as_array().unwrap().len(), 0);

    // 不存在的宿主 → 404
    let (st, _) = create_agent(&base, &a, &uuid::Uuid::new_v4().to_string(), "myagent").await;
    assert_eq!(st, 404);

    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;

    // 别人的宿主 id 建 Agent → 404（不泄露存在性）
    let (st, _) = create_agent(&base, &b, &host_id, "steal").await;
    assert_eq!(st, 404);

    // 名字校验：空 / 过长 / 非白名单字符 / 首字符 '-'（1–32 字符合法，"na" 是合法名）
    for bad in [
        "",
        "-bad",
        "bad name",
        "中文名",
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    ] {
        let (st, body) = create_agent(&base, &a, &host_id, bad).await;
        assert_eq!(st, 400, "bad name {bad:?} 应 400: {body}");
        assert_eq!(body["error"], "bad_name");
    }
    // runtime 校验：空 command、>50 args
    let r = c
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name":"ok1","host_id":host_id,"runtime":{"command":"","args":[]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = c
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name":"ok2","host_id":host_id,
                      "runtime":{"command":"node","args":vec!["x";51]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 正常创建 + 大小写不敏感重名 409
    let (st, agent) = create_agent(&base, &a, &host_id, "myAgent").await;
    assert_eq!(st, 200, "{agent}");
    assert_eq!(agent["name"], "myAgent");
    assert!(agent["conversation_id"].as_str().is_some());
    assert_eq!(agent["host_online"], true);
    let (st, body) = create_agent(&base, &a, &host_id, "MYAGENT").await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "name_taken");

    // 可见性：b 的 hosts/agents 列表都为空；a 的都有一条
    for u in [&a, &b] {
        let hosts: Value = c
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let agents: Value = c
            .get(format!("{base}/v1/agents"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if u.id == a.id {
            assert_eq!(hosts.as_array().unwrap().len(), 1);
            assert_eq!(hosts[0]["online"], true);
            assert_eq!(agents.as_array().unwrap().len(), 1);
        } else {
            assert!(hosts.as_array().unwrap().is_empty());
            assert!(agents.as_array().unwrap().is_empty());
        }
    }
    ha.shutdown().await;
}

/// 专属会话出现在会话列表、带 agent 字段；非成员读历史 403；
/// 拉人/退出/改名/转让/解散/建投票全部 400。
#[tokio::test]
async fn agent_conv_restrictions() {
    let base = spawn_named_db(&format!("chat_t_a01conv_{}", uniq())).await;
    let a = register(&base, &format!("a01c_{}", uniq())).await;
    let b = register(&base, &format!("a01d_{}", uniq())).await;
    // 先成为好友：拉人入口的好友校验通过后才能命中类型守卫 400（同 p24 self 用例）
    befriend(&base, &a, &b).await;
    let c = http();

    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "helper").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // 会话列表：kind='agent' + agent {id,name,host_online}
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let conv = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "agent")
        .expect("agent conv in list");
    assert_eq!(conv["agent"]["id"], agent_id.as_str());
    assert_eq!(conv["agent"]["name"], "helper");
    assert_eq!(conv["agent"]["host_online"], true);
    assert!(conv["name"].is_null());
    let conv_id = conv["id"].as_str().unwrap().to_string();

    // 非成员读历史 → 403
    let r = c
        .get(format!("{base}/v1/conversations/{conv_id}/messages"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 全部结构性操作对 agent 会话拒绝（400）
    let cases: Vec<(String, String, Value)> = vec![
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/members"),
            json!({"user_ids": [b.id]}),
        ),
        (
            "DELETE".into(),
            format!("{base}/v1/conversations/{conv_id}/members/me"),
            json!(null),
        ),
        (
            "PATCH".into(),
            format!("{base}/v1/conversations/{conv_id}"),
            json!({"name": "x"}),
        ),
        (
            "DELETE".into(),
            format!("{base}/v1/conversations/{conv_id}"),
            json!(null),
        ),
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/transfer"),
            json!({"user_id": b.id}),
        ),
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/polls"),
            json!({"question": "q?", "options": ["a", "b"]}),
        ),
    ];
    for (method, url, body) in cases {
        let req = match method.as_str() {
            "POST" => c.post(url),
            "PATCH" => c.patch(url),
            _ => c.delete(url),
        };
        let req = req.header("authorization", authz(&a.access));
        let r = if body.is_null() {
            req.send().await.unwrap()
        } else {
            req.json(&body).send().await.unwrap()
        };
        assert_eq!(r.status(), 400, "{method} {body} 应 400");
    }
    // 踢人（唯一成员是 owner，对他人操作也 400）
    let r = c
        .delete(format!(
            "{base}/v1/conversations/{conv_id}/members/{}",
            b.id
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    ha.shutdown().await;
}

/// 宿主生命周期：agent_host_status 上下线广播、GET /agent-hosts 在线态、
/// 宿主连接不产生好友 presence、同 install_id 重连复用 host_id、
/// 别的用户同 install_id 是另一条记录。
#[tokio::test]
async fn agent_host_lifecycle() {
    let base = spawn_named_db(&format!("chat_t_a01host_{}", uniq())).await;
    let a = register(&base, &format!("a01e_{}", uniq())).await;
    let b = register(&base, &format!("a01f_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let c = http();

    // b 先连、a 后连：先把 a 上线广播给 b 的 presence 帧收掉，后面的静默窗口才只反映宿主连接
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    until_frame(&mut sb, "presence", A01Dur::from_secs(5)).await;

    let install = uuid::Uuid::new_v4();
    let (h1, host_id) = spawn_host(&base, &a, install).await;

    // a 收 agent_host_status online=true
    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let st = frames
        .iter()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(st["host_id"], host_id.as_str());
    assert_eq!(st["online"], true);

    // GET /agent-hosts → online
    let hosts: Value = c
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(hosts[0]["online"], true);

    // 宿主连接不产生 presence：1s 窗口内 b 不应收到 user_id=a 的 presence 帧
    let frames = frames_for(&mut sb, A01Dur::from_secs(1)).await;
    assert!(
        !frames
            .iter()
            .any(|f| f["type"] == "presence" && f["user_id"] == a.id.as_str()),
        "宿主连接不应向好友广播 presence: {frames:?}"
    );

    // 断开 → online=false
    h1.shutdown().await;
    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let st = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(st["online"], false);

    // 同 install_id 重连 → 同一 host_id
    let (h2, host_id2) = spawn_host(&base, &a, install).await;
    assert_eq!(host_id2, host_id);
    // 另一用户用同一 install_id → 不同 host_id（(user,install) 唯一下各管各的）
    let (h3, host_id3) = spawn_host(&base, &b, install).await;
    assert_ne!(host_id3, host_id);
    h2.shutdown().await;
    h3.shutdown().await;
}

/// 主路径：echo → running → Agent 消息（归属正确）→ succeeded；
/// 未读 +1；Agent 消息不触发主人的 read_sync。
#[tokio::test]
async fn agent_invoke_echo_end_to_end() {
    let base = spawn_named_db(&format!("chat_t_a01echo_{}", uniq())).await;
    let a = register(&base, &format!("a01g_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "echoer").await;
    assert_eq!(st, 200);
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "e1", "echo:hello world").await;

    // 先等 running（调用开始即推帧）
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    assert_eq!(inv["invocation"]["conversation_id"], conv.as_str());
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();

    // 再等终态；期间的 Agent 消息帧一并收集
    let mut frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    while !frames
        .iter()
        .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
    }
    // 多留 300ms 收尾巴帧
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(300)).await);

    let final_inv = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id.as_str())
        .expect("succeeded frame");
    assert_eq!(final_inv["invocation"]["status"], "succeeded");

    // Agent 消息：sender_id=主人、agent.id/name 正确、正文 hello world
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 消息");
    assert_eq!(agent_msg["message"]["agent"]["id"], agent_id.as_str());
    assert_eq!(agent_msg["message"]["agent"]["name"], "echoer");
    assert_eq!(agent_msg["message"]["sender_id"], a.id.as_str());
    assert_eq!(agent_msg["message"]["content"], "hello world");
    let agent_seq = agent_msg["message"]["seq"].as_i64().unwrap();

    // Agent 消息不给主人发 read_sync（read_sync 只覆盖主人自己的触发消息）
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "read_sync"
                && f["conversation_id"] == conv.as_str()
                && f["seq"].as_i64().unwrap() >= agent_seq
        }),
        "Agent 消息不应带 read_sync: {frames:?}"
    );

    // 未读 = 1（主人自己的触发消息不算未读，Agent 回复算）
    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv.as_str())
        .unwrap();
    assert_eq!(c["unread_count"].as_i64().unwrap(), 1);
    ha.shutdown().await;
}

/// fail / crash → failed + 非空 error；不产生 Agent 消息。
#[tokio::test]
async fn agent_invoke_fail_and_crash() {
    let base = spawn_named_db(&format!("chat_t_a01fail_{}", uniq())).await;
    let a = register(&base, &format!("a01h_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "x").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    for (i, prompt) in ["fail", "crash"].iter().enumerate() {
        send_ws(&mut sa, &conv, &format!("f{i}"), prompt).await;
        // 等到该调用进终态（running 帧先，failed 帧后）
        let mut done = false;
        let mut collected = Vec::new();
        for _ in 0..2 {
            collected.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
            if collected
                .iter()
                .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "failed")
            {
                done = true;
                break;
            }
        }
        assert!(done, "{prompt} 应 failed: {collected:?}");
        let failed = collected
            .iter()
            .find(|f| f["invocation"]["status"] == "failed")
            .unwrap();
        assert!(
            failed["invocation"]["error"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "error 不应为空"
        );
    }
    // 没有产生任何 Agent 消息
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["agent"].is_object()),
        "失败调用不应产生 Agent 消息"
    );
    ha.shutdown().await;
}

/// 宿主离线时发消息：排队（A04），不是立即 failed。
#[tokio::test]
async fn agent_invoke_host_offline() {
    let base = spawn_named_db(&format!("chat_t_a01off_{}", uniq())).await;
    let a = register(&base, &format!("a01i_{}", uniq())).await;
    // 先在本机会话上建好 Agent，再断开：留下 host 行 + 离线态（A26 起离线不能创建）
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "off").await;
    assert_eq!(st, 200, "{agent}");
    ha.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "o1", "echo:hi").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "queued");
    assert!(inv["invocation"]["error"].is_null());
    assert!(inv["invocation"]["started_at"].is_null());
}

/// 调用跑到一半宿主断开 → 宽限内仍是 running（A23：不立刻判失败）。
#[tokio::test]
async fn agent_invoke_host_disconnect_mid_run() {
    let base = spawn_named_db(&format!("chat_t_a01dc_{}", uniq())).await;
    let a = register(&base, &format!("a01j_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "sleeper").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:10000").await;
    // 先等到 running
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();

    ha.shutdown().await; // 宿主进程下线
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let rows = list_inv(&base, &a, &conv).await;
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == inv_id)
        .unwrap();
    assert_eq!(row["status"], "running", "断线后宽限内应保持运行: {row}");
    assert!(row["error"].is_null(), "{row}");
    let frames = frames_for(&mut sa, A01Dur::from_millis(300)).await;
    assert!(
        frames
            .iter()
            .all(|f| f["invocation"]["error"] != "host_disconnected"),
        "宽限内不应判断开失败: {frames:?}"
    );
}

/// long:9000 → 三条按序 Agent 消息，拼接等于原输出。
#[tokio::test]
async fn agent_invoke_long_output_split() {
    let base = spawn_named_db(&format!("chat_t_a01long_{}", uniq())).await;
    let a = register(&base, &format!("a01k_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "longer").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "l1", "long:9000").await;
    // 收到 succeeded 为止的全部帧
    let mut frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(20)).await;
    while !frames
        .iter()
        .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(20)).await);
    }
    let mut agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    agent_msgs.sort_by_key(|f| f["message"]["seq"].as_i64().unwrap());
    assert_eq!(agent_msgs.len(), 3, "9000 字符应拆成 3 条: {frames:?}");
    let joined: String = agent_msgs
        .iter()
        .map(|f| f["message"]["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined.len(), 9000);
    assert!(joined.chars().all(|ch| ch == 'x'));
    ha.shutdown().await;
}

/// 同一 Agent 多次调用并发执行：两个 sleep:1500 总耗时约 1.5s 而非 3s。
#[tokio::test]
async fn agent_invoke_concurrent() {
    let base = spawn_named_db(&format!("chat_t_a01par_{}", uniq())).await;
    let a = register(&base, &format!("a01m_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "par").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    // 各睡 3s：并行约 3s + 起进程开销，串行至少 6s；阈值留足余量，机器繁忙时也不误报
    send_ws(&mut sa, &conv, "p1", "sleep:3000").await;
    send_ws(&mut sa, &conv, "p2", "sleep:3000").await;

    let mut frames: Vec<Value> = Vec::new();
    while frames
        .iter()
        .filter(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .count()
        < 2
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await);
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < A01Dur::from_millis(5500),
        "并发两次 3s 调用应约 3-4s 完成，实际 {elapsed:?}（串行会 6s+）"
    );
    ha.shutdown().await;
}

/// 调用历史：GET /conversations/{id}/agent-invocations 返回终态；非成员 403。
#[tokio::test]
async fn agent_invocation_history() {
    let base = spawn_named_db(&format!("chat_t_a01hist_{}", uniq())).await;
    let a = register(&base, &format!("a01n_{}", uniq())).await;
    let b = register(&base, &format!("a01o_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "hist").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    send_ws(&mut sa, &conv, "h1", "echo:one").await;
    send_ws(&mut sa, &conv, "h2", "fail").await;
    // 等两条终态
    let mut frames: Vec<Value> = Vec::new();
    while frames
        .iter()
        .filter(|f| {
            f["type"] == "agent_invocation"
                && (f["invocation"]["status"] == "succeeded"
                    || f["invocation"]["status"] == "failed")
        })
        .count()
        < 2
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
    }

    let rows: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let statuses: Vec<&str> = rows.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert!(statuses.contains(&"succeeded"));
    assert!(statuses.contains(&"failed"));

    // 非成员 → 403
    let r = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    ha.shutdown().await;
}

// ===== Agent 设置层（第 2 批前预重构） =====

#[tokio::test]
async fn agent_settings_defaults_and_overrides() {
    let base = spawn_named_db(&format!("chat_t_agset_{}", uniq())).await;
    let a = register(&base, &format!("agseta_{}", uniq())).await;
    let b = register(&base, &format!("agsetb_{}", uniq())).await;
    let c = http();
    let settings = |u: &TestUser| {
        c.get(format!("{base}/v1/users/me/agent-settings"))
            .header("authorization", authz(&u.access))
    };

    // 没设置过 → 默认值
    let s: Value = settings(&a).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["output_mode"], "stream");
    assert_eq!(s["queue_timeout_secs"], 86400);

    // 部分更新：只改输出方式；非法值 400
    let put = |u: &TestUser, body: Value| {
        c.put(format!("{base}/v1/users/me/agent-settings"))
            .header("authorization", authz(&u.access))
            .json(&body)
            .send()
    };
    let r = put(&a, json!({"output_mode": "status"})).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let s: Value = settings(&a).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["output_mode"], "status");
    assert_eq!(s["queue_timeout_secs"], 86400);
    assert_eq!(
        put(&a, json!({"output_mode": "loud"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    assert_eq!(
        put(&a, json!({"queue_timeout_secs": -1}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    assert_eq!(
        put(&a, json!({"queue_timeout_secs": 0}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    // 别人的设置不受影响
    let sb: Value = settings(&b).send().await.unwrap().json().await.unwrap();
    assert_eq!(sb["output_mode"], "stream");

    // Agent 覆盖：新建时为 null；PATCH 设值、显式 null 清除、缺省字段不动
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "setter").await;
    assert!(agent["output_mode"].is_null() && agent["queue_timeout_secs"].is_null());
    let id = agent["id"].as_str().unwrap();
    let patch = |u: &TestUser, body: Value| {
        c.patch(format!("{base}/v1/agents/{id}"))
            .header("authorization", authz(&u.access))
            .json(&body)
            .send()
    };
    let v: Value = patch(
        &a,
        json!({"output_mode": "steps", "queue_timeout_secs": 600}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(v["output_mode"], "steps");
    assert_eq!(v["queue_timeout_secs"], 600);
    let v: Value = patch(&a, json!({"output_mode": null}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["output_mode"].is_null());
    assert_eq!(v["queue_timeout_secs"], 600);
    assert_eq!(
        patch(&a, json!({"output_mode": "x"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    // 别人的 Agent → 404
    assert_eq!(
        patch(&b, json!({"output_mode": "steps"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );
    // 列表里带覆盖值
    let list: Value = c
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["queue_timeout_secs"], 600);
    h.shutdown().await;
}
