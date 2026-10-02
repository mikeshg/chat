use super::*;

// ===== Agent A12 =====
// 权限档位 + 审批卡片。真宿主 + 假 ACP agent 的 perm:<kind>:<路径>。

fn reply_count(msgs: &[Value], text: &str) -> usize {
    msgs.iter()
        .filter(|m| m["kind"] != "approval" && m["agent"].is_object() && m["content"] == text)
        .count()
}

async fn wait_reply_count(base: &str, u: &TestUser, conv: &str, text: &str, n: usize) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        let got = reply_count(&msgs, text);
        if got >= n {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等回复 {text} ×{n} 超时，当前 {got}：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

pub(crate) async fn wait_pending_approval(base: &str, u: &TestUser, conv: &str) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = msgs
            .iter()
            .rev()
            .find(|m| m["kind"] == "approval" && m["approval"]["status"] == "pending")
        {
            return m.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等审批卡片超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

async fn wait_approval_status(
    base: &str,
    u: &TestUser,
    conv: &str,
    id: &str,
    status: &str,
) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(15);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = msgs.iter().find(|m| m["approval"]["id"] == id) {
            if m["approval"]["status"] == status {
                return m.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等审批 {id} 变成 {status} 超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

pub(crate) fn latest_inv_status(rows: &Value) -> Option<&str> {
    rows.as_array()?.first()?.get("status")?.as_str()
}

/// 1 + 7：只读档位工作目录内的 read 自动放行且没有卡片；edit 出卡片、调用变 awaiting_approval，并计入未读。
#[tokio::test]
async fn a12_read_only_auto_read_and_approval_card_counts_unread() {
    let base = spawn_named_db(&format!("chat_it_a12ro_{}", uniq())).await;
    let a = register(&base, &format!("a12o_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "permbot").await;
    assert_eq!(st, 200, "{agent}");
    assert_eq!(agent["permission_tier"], "read_only");
    let agent_id = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "r1", "perm:read:notes.txt").await;
    wait_reply_count(&base, &a, &conv, "allowed:allow", 1).await;
    let msgs = conv_messages(&base, &a, &conv).await;
    assert!(
        msgs.iter().all(|m| m["kind"] != "approval"),
        "只读档位的 read 不应产生审批卡片: {msgs:?}"
    );
    assert_eq!(
        a11_unread(&base, &a, &conv).await,
        1,
        "自动放行的回复计入未读"
    );

    send_ws(&mut sa, &conv, "e1", "perm:edit:C:/outside/secret.txt").await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    assert_eq!(card["agent"]["id"], agent_id);
    assert!(
        card["content"].as_str().unwrap_or("").contains("写入"),
        "卡片正文应是可读摘要: {card}"
    );
    assert!(
        card["content"]
            .as_str()
            .unwrap_or("")
            .contains("secret.txt"),
        "摘要应带上路径: {card}"
    );
    let options = card["approval"]["options"].as_array().unwrap();
    assert!(options
        .iter()
        .any(|o| o["optionId"] == "allow" && o["kind"] == "allow_once"));
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(
        latest_inv_status(&rows),
        Some("awaiting_approval"),
        "edit 应停在等待审批: {rows}"
    );
    // 第二条是主人自己发的，已读游标推过它（也盖过上一条回复）。
    // 卡片是 Agent 发的，所以未读正好回到 1；若卡片被当成自己的消息，这里会是 0。
    assert_eq!(
        a11_unread(&base, &a, &conv).await,
        1,
        "审批卡片计入主人未读"
    );
    host.shutdown().await;
}

/// 2：主人允许 → allowed:allow 且调用成功、卡片 decided；拒绝 → rejected。
#[tokio::test]
async fn a12_owner_allow_and_reject() {
    let base = spawn_named_db(&format!("chat_it_a12dec_{}", uniq())).await;
    let a = register(&base, &format!("a12d_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "decider").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    send_ws(&mut sa, &conv, "a1", "perm:edit:C:/outside/a.txt").await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap().to_string();
    let (st, body) = decide_approval(&base, &a, &id, "allow").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["approval"]["status"], "decided");
    assert_eq!(body["approval"]["decision"], "allow");
    wait_reply_count(&base, &a, &conv, "allowed:allow", 1).await;
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(latest_inv_status(&rows), Some("succeeded"), "{rows}");
    let updated = wait_approval_status(&base, &a, &conv, &id, "decided").await;
    assert_eq!(updated["approval"]["decision"], "allow");

    send_ws(&mut sa, &conv, "a2", "perm:edit:C:/outside/b.txt").await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap().to_string();
    let (st, body) = decide_approval(&base, &a, &id, "reject").await;
    assert_eq!(st, 200, "{body}");
    wait_reply_count(&base, &a, &conv, "rejected", 1).await;
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(latest_inv_status(&rows), Some("succeeded"), "{rows}");
    let updated = wait_approval_status(&base, &a, &conv, &id, "decided").await;
    assert_eq!(updated["approval"]["decision"], "reject");
    host.shutdown().await;
}

/// 3：workspace 目录内 edit 自动放行，目录外 edit 与 execute 走审批；full 全部自动放行。
#[tokio::test]
async fn a12_workspace_and_full_tiers() {
    let base = spawn_named_db(&format!("chat_it_a12ws_{}", uniq())).await;
    let a = register(&base, &format!("a12w_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "worker").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let dir = std::env::temp_dir().join(format!("chat_a12_ws_{}", uniq()));
    std::fs::create_dir_all(&dir).unwrap();
    let inside = dir.join("note.txt");
    let outside = std::env::temp_dir().join(format!("chat_a12_out_{}", uniq()));
    let patched_res = patch_agent(
        &base,
        &a,
        agent_id,
        json!({
            "permission_tier": "workspace",
            "cwd": dir.to_string_lossy(),
        }),
    )
    .await;
    let st = patched_res.status().as_u16();
    let patched: Value = patched_res.json().await.unwrap_or(json!(null));
    assert_eq!(st, 200, "{patched}");
    assert_eq!(patched["permission_tier"], "workspace");
    assert_eq!(patched["cwd"], dir.to_string_lossy().as_ref());

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let inside_prompt = format!("perm:edit:{}", inside.display());
    send_ws(&mut sa, &conv, "w1", &inside_prompt).await;
    wait_reply_count(&base, &a, &conv, "allowed:allow", 1).await;
    let msgs = conv_messages(&base, &a, &conv).await;
    assert!(
        msgs.iter().all(|m| m["kind"] != "approval"),
        "工作目录内的 edit 不应审批: {msgs:?}"
    );

    let outside_prompt = format!("perm:edit:{}", outside.display());
    send_ws(&mut sa, &conv, "w2", &outside_prompt).await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    assert!(
        card["content"].as_str().unwrap_or("").contains("写入"),
        "{card}"
    );
    // 先决定掉，避免后面的断言被这张 pending 卡片挡住
    let (st, _) = decide_approval(
        &base,
        &a,
        card["approval"]["id"].as_str().unwrap(),
        "reject",
    )
    .await;
    assert_eq!(st, 200);
    wait_reply_count(&base, &a, &conv, "rejected", 1).await;

    let exec_prompt = format!("perm:execute:{}", inside.display());
    send_ws(&mut sa, &conv, "w3", &exec_prompt).await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    assert!(
        card["approval"]["request"]["kind"] == "execute",
        "execute 即使在目录内也要审批: {card}"
    );
    let (st, _) = decide_approval(
        &base,
        &a,
        card["approval"]["id"].as_str().unwrap(),
        "reject",
    )
    .await;
    assert_eq!(st, 200);
    wait_reply_count(&base, &a, &conv, "rejected", 2).await;

    let patched_res = patch_agent(&base, &a, agent_id, json!({"permission_tier": "full"})).await;
    let st = patched_res.status().as_u16();
    let patched: Value = patched_res.json().await.unwrap_or(json!(null));
    assert_eq!(st, 200, "{patched}");
    assert_eq!(patched["permission_tier"], "full");
    let before = conv_messages(&base, &a, &conv).await;
    let cards_before = before.iter().filter(|m| m["kind"] == "approval").count();
    send_ws(&mut sa, &conv, "w4", &exec_prompt).await;
    wait_reply_count(&base, &a, &conv, "allowed:allow", 2).await;
    let after = conv_messages(&base, &a, &conv).await;
    let cards_after = after.iter().filter(|m| m["kind"] == "approval").count();
    assert_eq!(cards_after, cards_before, "full 不应再生成审批卡片");
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 4：群里调用要审批时，卡片只在专属会话；群成员只有 awaiting_approval，历史里没有审批摘要。
#[tokio::test]
async fn a12_group_approval_stays_in_agent_conversation() {
    let base = spawn_named_db(&format!("chat_it_a12g_{}", uniq())).await;
    let owner = register(&base, &format!("a12go_{}", uniq())).await;
    let member = register(&base, &format!("a12gm_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let agent_conv = agent["conversation_id"].as_str().unwrap().to_string();
    let gid = a11_group(&base, &owner, &[&member]).await;

    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let (mut sm, _) = tokio_tungstenite::connect_async(ws_url(&base, &member).await)
        .await
        .unwrap();
    send_ws(&mut so, &gid, "g1", "@bot perm:edit:A12SECRET/tool.sh").await;
    let frame = wait_inv(
        &mut sm,
        |f| {
            f["invocation"]["status"] == "awaiting_approval"
                && f["invocation"]["conversation_id"] == gid
        },
        A01Dur::from_secs(20),
    )
    .await;
    assert_eq!(frame["invocation"]["status"], "awaiting_approval");
    let extra = frames_for(&mut sm, A01Dur::from_millis(600)).await;
    let mut seen = vec![frame];
    seen.extend(extra);
    assert!(
        !seen
            .iter()
            .any(|f| { f["type"] == "message" && f["message"]["kind"] == "approval" }),
        "群成员不应收到审批卡片: {seen:?}"
    );
    assert!(
        !seen.iter().any(|f| {
            f["type"] == "message"
                && f["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("请求写入")
        }),
        "群成员的帧里不应有审批摘要: {seen:?}"
    );

    let group_msgs = conv_messages(&base, &member, &gid).await;
    assert!(
        group_msgs.iter().all(|m| m["kind"] != "approval"),
        "群历史不应有审批卡片: {group_msgs:?}"
    );
    assert!(
        group_msgs
            .iter()
            .all(|m| !m["content"].as_str().unwrap_or("").contains("请求写入")),
        "群历史不应出现审批摘要: {group_msgs:?}"
    );
    let card = wait_pending_approval(&base, &owner, &agent_conv).await;
    assert!(
        card["content"].as_str().unwrap_or("").contains("A12SECRET"),
        "卡片应在专属会话且带上路径: {card}"
    );
    assert_eq!(card["conversation_id"], agent_conv.as_str());
    host.shutdown().await;
}

/// 5：别人决定 404；非法 option 400；重复决定 409。
#[tokio::test]
async fn a12_decide_permissions() {
    let base = spawn_named_db(&format!("chat_it_a12p_{}", uniq())).await;
    let owner = register(&base, &format!("a12po_{}", uniq())).await;
    let other = register(&base, &format!("a12px_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "guard").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "p1", "perm:edit:C:/outside/c.txt").await;
    let card = wait_pending_approval(&base, &owner, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap();

    let (st, body) = decide_approval(&base, &other, id, "allow").await;
    assert_eq!(st, 404, "别人决定应 404: {body}");
    let missing = uuid::Uuid::new_v4().to_string();
    let (st, _) = decide_approval(&base, &owner, &missing, "allow").await;
    assert_eq!(st, 404);

    let (st, body) = decide_approval(&base, &owner, id, "nope").await;
    assert_eq!(st, 400, "非法选项应 400: {body}");
    assert_eq!(body["error"], "bad_option");
    let still = wait_approval_status(&base, &owner, &conv, id, "pending").await;
    assert_eq!(still["approval"]["status"], "pending");

    let (st, body) = decide_approval(&base, &owner, id, "allow").await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = decide_approval(&base, &owner, id, "allow").await;
    assert_eq!(st, 409, "重复决定应 409: {body}");
    assert_eq!(body["error"], "not_pending");
    wait_reply_count(&base, &owner, &conv, "allowed:allow", 1).await;
    host.shutdown().await;
}

/// 6：等待审批时取消 → 审批 expired，卡片更新，没有 Agent 回复。
#[tokio::test]
async fn a12_cancel_while_awaiting_expires_approval() {
    let base = spawn_named_db(&format!("chat_it_a12c_{}", uniq())).await;
    let a = register(&base, &format!("a12c_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "canceler").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "c1", "perm:execute:C:/tmp/run.sh").await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap().to_string();
    let rows = list_inv(&base, &a, &conv).await;
    let inv_id = rows.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(rows.as_array().unwrap()[0]["status"], "awaiting_approval");

    let (st, body) = cancel_inv(&base, &a, &inv_id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    let updated = wait_approval_status(&base, &a, &conv, &id, "expired").await;
    assert_eq!(updated["approval"]["status"], "expired");
    assert!(updated["approval"]["decision"].is_null());
    // 给宿主一点时间，确认取消后没有把 rejected/allowed 写进历史
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let msgs = conv_messages(&base, &a, &conv).await;
    assert_eq!(reply_count(&msgs, "rejected"), 0, "{msgs:?}");
    assert_eq!(reply_count(&msgs, "allowed:allow"), 0, "{msgs:?}");
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "cancelled");
    host.shutdown().await;
}

// ===== Agent A22 =====
// 同一次调用可以有多张 pending 审批。假宿主连发 approval_request；
// 决定一张不把调用放回 running，直到没有剩余 pending。

fn a22_approval_frame(inv_id: &str, request_id: &str, title: &str) -> Value {
    json!({
        "type": "approval_request",
        "invocation_id": inv_id,
        "request_id": request_id,
        "tool_call": {"title": title, "kind": "execute", "locations": [title]},
        "options": [
            {"optionId": "allow", "name": "允许", "kind": "allow_once"},
            {"optionId": "reject", "name": "拒绝", "kind": "reject_once"}
        ]
    })
}

pub(crate) async fn a22_send_approval(
    sh: &mut RawHostWs,
    inv_id: &str,
    request_id: &str,
    title: &str,
) {
    sh.send(WsMsg::Text(
        a22_approval_frame(inv_id, request_id, title)
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

fn a22_find_approval(msgs: &[Value], request_id: &str) -> Option<Value> {
    msgs.iter()
        .find(|m| m["kind"] == "approval" && m["approval"]["request"]["request_id"] == request_id)
        .cloned()
}

fn a22_pending_count(msgs: &[Value]) -> usize {
    msgs.iter()
        .filter(|m| m["kind"] == "approval" && m["approval"]["status"] == "pending")
        .count()
}

pub(crate) async fn a22_inv_status(base: &str, u: &TestUser, conv: &str) -> String {
    latest_inv_status(&list_inv(base, u, conv).await)
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn a22_wait_approval(
    base: &str,
    u: &TestUser,
    conv: &str,
    request_id: &str,
) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(10);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = a22_find_approval(&msgs, request_id) {
            return m;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等审批 {request_id} 超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

pub(crate) async fn a22_wait_decision(sh: &mut RawHostWs, request_id: &str) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 approval_decision {request_id} 超时");
        let frames = until_frame(sh, "approval_decision", left).await;
        if let Some(f) = frames
            .into_iter()
            .rev()
            .find(|f| f["request_id"] == request_id)
        {
            return f;
        }
    }
}

/// 用户连接上的 agent_stream 步骤，用来确认宿主侧 approval_request 已经处理完。
async fn a22_wait_step(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>, step_id: &str) {
    let deadline = tokio::time::Instant::now() + A01Dur::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "等步骤 {step_id} 超时");
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(v)) if v["type"] == "agent_stream" && v["step"]["id"] == step_id => return,
            Ok(Some(_)) => {}
            Ok(None) => panic!("用户连接已关，没等到步骤 {step_id}"),
            Err(_) => panic!("等步骤 {step_id} 超时"),
        }
    }
}

fn a22_step_frame(inv_id: &str, step_id: &str) -> Value {
    json!({
        "type": "invocation_event",
        "invocation_id": inv_id,
        "event": {"type": "step_start", "id": step_id, "title": "barrier", "kind": "other"}
    })
}

/// 主路径：同一调用两张卡都进专属会话；决定第一张后仍 awaiting_approval，
/// 宿主收到对应 approval_decision；决定第二张后调用回到 running。
#[tokio::test]
async fn a22_two_approvals_decide_one_by_one() {
    let base = spawn_named_db(&format!("chat_it_a22two_{}", uniq())).await;
    let a = register(&base, &format!("a22t_{}", uniq())).await;
    let (mut sh, _su, inv_id, _token, conv, _agent) = a13_running_invocation(&base, &a).await;

    a22_send_approval(&mut sh, &inv_id, "req-a", "first-tool").await;
    a22_send_approval(&mut sh, &inv_id, "req-b", "second-tool").await;

    let card_a = a22_wait_approval(&base, &a, &conv, "req-a").await;
    let card_b = a22_wait_approval(&base, &a, &conv, "req-b").await;
    assert_eq!(card_a["approval"]["status"], "pending");
    assert_eq!(card_b["approval"]["status"], "pending");
    assert!(
        card_a["content"]
            .as_str()
            .unwrap_or("")
            .contains("first-tool"),
        "{card_a}"
    );
    assert!(
        card_b["content"]
            .as_str()
            .unwrap_or("")
            .contains("second-tool"),
        "{card_b}"
    );
    assert_eq!(card_a["conversation_id"], conv.as_str());
    assert_eq!(card_b["conversation_id"], conv.as_str());
    let msgs = conv_messages(&base, &a, &conv).await;
    assert_eq!(
        msgs.iter().filter(|m| m["kind"] == "approval").count(),
        2,
        "专属会话应正好两张卡: {msgs:?}"
    );
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "awaiting_approval");

    let id_a = card_a["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id_a, "allow").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["approval"]["status"], "decided");
    assert_eq!(body["approval"]["decision"], "allow");
    let dec_a = a22_wait_decision(&mut sh, "req-a").await;
    assert_eq!(dec_a["type"], "approval_decision");
    assert_eq!(dec_a["option_id"], "allow");
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "awaiting_approval");
    let mid = conv_messages(&base, &a, &conv).await;
    assert_eq!(
        a22_find_approval(&mid, "req-a").unwrap()["approval"]["status"],
        "decided"
    );
    assert_eq!(
        a22_find_approval(&mid, "req-b").unwrap()["approval"]["status"],
        "pending"
    );

    let id_b = card_b["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id_b, "reject").await;
    assert_eq!(st, 200, "{body}");
    let dec_b = a22_wait_decision(&mut sh, "req-b").await;
    assert_eq!(dec_b["option_id"], "reject");
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "running");
    let done = conv_messages(&base, &a, &conv).await;
    assert_eq!(
        a22_find_approval(&done, "req-a").unwrap()["approval"]["decision"],
        "allow"
    );
    assert_eq!(
        a22_find_approval(&done, "req-b").unwrap()["approval"]["status"],
        "decided"
    );
    assert_eq!(
        a22_find_approval(&done, "req-b").unwrap()["approval"]["decision"],
        "reject"
    );
    assert_eq!(a22_pending_count(&done), 0);
}

/// 两张 pending 时取消：全部 expired。取消之后再来的审批请求不再建卡。
#[tokio::test]
async fn a22_cancel_expires_all_pending_approvals() {
    let base = spawn_named_db(&format!("chat_it_a22c_{}", uniq())).await;
    let a = register(&base, &format!("a22c_{}", uniq())).await;
    let (mut sh, _su, inv_id, _token, conv, _agent) = a13_running_invocation(&base, &a).await;

    a22_send_approval(&mut sh, &inv_id, "c-1", "cancel-one").await;
    a22_send_approval(&mut sh, &inv_id, "c-2", "cancel-two").await;
    let card1 = a22_wait_approval(&base, &a, &conv, "c-1").await;
    let card2 = a22_wait_approval(&base, &a, &conv, "c-2").await;
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "awaiting_approval");

    let (st, body) = cancel_inv(&base, &a, &inv_id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    let id1 = card1["approval"]["id"].as_str().unwrap();
    let id2 = card2["approval"]["id"].as_str().unwrap();
    let gone1 = wait_approval_status(&base, &a, &conv, id1, "expired").await;
    let gone2 = wait_approval_status(&base, &a, &conv, id2, "expired").await;
    assert!(gone1["approval"]["decision"].is_null());
    assert!(gone2["approval"]["decision"].is_null());

    a22_send_approval(&mut sh, &inv_id, "c-late", "too-late").await;
    tokio::time::sleep(A01Dur::from_millis(500)).await;
    let msgs = conv_messages(&base, &a, &conv).await;
    assert!(
        a22_find_approval(&msgs, "c-late").is_none(),
        "已取消的调用不应再收卡: {msgs:?}"
    );
    assert_eq!(msgs.iter().filter(|m| m["kind"] == "approval").count(), 2);
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "cancelled");
}

/// 不是这条调用所属宿主的连接发 approval_request：不建卡。
#[tokio::test]
async fn a22_other_host_approval_is_ignored() {
    let base = spawn_named_db(&format!("chat_it_a22h_{}", uniq())).await;
    let a = register(&base, &format!("a22h_{}", uniq())).await;
    let (mut sh, _su, inv_id, _token, conv, _agent) = a13_running_invocation(&base, &a).await;
    let (mut other, other_host) = spawn_raw_host(&base, &a).await;
    assert_ne!(other_host, "");

    a22_send_approval(&mut other, &inv_id, "foreign", "stolen").await;
    let quiet = std::time::Instant::now() + A01Dur::from_millis(800);
    while std::time::Instant::now() < quiet {
        let msgs = conv_messages(&base, &a, &conv).await;
        assert!(
            a22_find_approval(&msgs, "foreign").is_none(),
            "别的宿主不能给这次调用建卡: {msgs:?}"
        );
        assert!(msgs.iter().all(|m| m["kind"] != "approval"));
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }

    a22_send_approval(&mut sh, &inv_id, "real", "owned").await;
    let card = a22_wait_approval(&base, &a, &conv, "real").await;
    assert_eq!(card["approval"]["status"], "pending");
    tokio::time::sleep(A01Dur::from_millis(400)).await;
    let msgs = conv_messages(&base, &a, &conv).await;
    assert!(a22_find_approval(&msgs, "foreign").is_none(), "{msgs:?}");
    assert_eq!(msgs.iter().filter(|m| m["kind"] == "approval").count(), 1);
    let _ = other.send(WsMsg::Close(None)).await;
}

/// 宿主断开：宽限内调用仍是 awaiting_approval，pending 卡不过期（A23）。
#[tokio::test]
async fn a22_host_disconnect_keeps_pending_approvals() {
    let base = spawn_named_db(&format!("chat_it_a22d_{}", uniq())).await;
    let a = register(&base, &format!("a22d_{}", uniq())).await;
    let (mut sh, _su, _inv, _token, conv, _agent) = a13_running_invocation(&base, &a).await;
    let rows = list_inv(&base, &a, &conv).await;
    let inv_id = rows.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    a22_send_approval(&mut sh, &inv_id, "d-1", "disc-one").await;
    a22_send_approval(&mut sh, &inv_id, "d-2", "disc-two").await;
    let card1 = a22_wait_approval(&base, &a, &conv, "d-1").await;
    let card2 = a22_wait_approval(&base, &a, &conv, "d-2").await;

    sh.close(None).await.unwrap();
    drop(sh);

    tokio::time::sleep(A01Dur::from_millis(600)).await;
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(
        rows.as_array().unwrap()[0]["status"],
        "awaiting_approval",
        "断线后宽限内应继续等待审批: {rows}"
    );
    let _ = (card1, card2);
    let msgs = conv_messages(&base, &a, &conv).await;
    assert_eq!(
        a22_find_approval(&msgs, "d-1").unwrap()["approval"]["status"],
        "pending",
        "{msgs:?}"
    );
    assert_eq!(
        a22_find_approval(&msgs, "d-2").unwrap()["approval"]["status"],
        "pending",
        "{msgs:?}"
    );
}

/// 决定最后一张与新的 approval_request 同时到达：不能出现 running 且仍有 pending 卡。
/// 新请求在调用仍进行时一定建卡，所以结束后应是 awaiting_approval。
#[tokio::test]
async fn a22_decide_last_races_new_request() {
    let base = spawn_named_db(&format!("chat_it_a22r_{}", uniq())).await;
    let a = register(&base, &format!("a22r_{}", uniq())).await;
    let (mut sh, su, inv_id, _token, conv, _agent) = a13_running_invocation(&base, &a).await;

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut su = su;
        while let Some(Ok(msg)) = su.next().await {
            if let WsMsg::Text(t) = msg {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let _ = frame_tx.send(v);
                }
            }
        }
    });

    let mut open_req = "req-0".to_string();
    a22_send_approval(&mut sh, &inv_id, &open_req, "seed").await;
    let mut open_card = a22_wait_approval(&base, &a, &conv, &open_req).await;
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "awaiting_approval");

    for i in 1..=6 {
        let new_req = format!("req-{i}");
        let step_id = format!("barrier-{i}");
        let approval_id = open_card["approval"]["id"].as_str().unwrap().to_string();
        let decide = decide_approval(&base, &a, &approval_id, "allow");
        let send = async {
            a22_send_approval(&mut sh, &inv_id, &new_req, "raced").await;
            sh.send(WsMsg::Text(
                a22_step_frame(&inv_id, &step_id).to_string().into(),
            ))
            .await
            .unwrap();
        };
        let ((st, body), ()) = tokio::join!(decide, send);
        assert_eq!(st, 200, "第 {i} 轮决定失败: {body}");
        a22_wait_step(&mut frame_rx, &step_id).await;
        let dec = a22_wait_decision(&mut sh, &open_req).await;
        assert_eq!(dec["request_id"], open_req);
        assert_eq!(dec["option_id"], "allow");

        let new_card = a22_wait_approval(&base, &a, &conv, &new_req).await;
        assert_eq!(new_card["approval"]["status"], "pending", "{new_card}");
        let msgs = conv_messages(&base, &a, &conv).await;
        assert_eq!(
            a22_find_approval(&msgs, &open_req).unwrap()["approval"]["status"],
            "decided"
        );
        let status = a22_inv_status(&base, &a, &conv).await;
        let pending = a22_pending_count(&msgs);
        assert!(
            !(status == "running" && pending > 0),
            "第 {i} 轮出现 running 但仍有 pending 卡: status={status} pending={pending} msgs={msgs:?}"
        );
        assert_eq!(status, "awaiting_approval", "{msgs:?}");
        assert!(pending >= 1, "{msgs:?}");
        open_req = new_req;
        open_card = new_card;
    }

    let last_id = open_card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, last_id, "reject").await;
    assert_eq!(st, 200, "{body}");
    let dec = a22_wait_decision(&mut sh, &open_req).await;
    assert_eq!(dec["option_id"], "reject");
    assert_eq!(a22_inv_status(&base, &a, &conv).await, "running");
    let msgs = conv_messages(&base, &a, &conv).await;
    assert_eq!(a22_pending_count(&msgs), 0, "{msgs:?}");
}

// ===== Agent A20 =====
// 发消息审批的完整披露：宿主上报的 tool_call.detail（完整正文）原样进卡片
// request（含内嵌 tool_call），超过 4000 字按字截断；locations 照旧。

/// 裸宿主连接（role=host）：host_hello 登记后返回 (ws, host_id)。
/// 只用来收 invocation_start 和上报 approval_request，不跑真实运行时。
async fn a20_raw_host(
    base: &str,
    u: &TestUser,
) -> (
    impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMsg, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
    String,
) {
    let ticket: Value = http()
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = format!(
        "{}/v1/ws?ticket={}&role=host",
        base.replacen("http://", "ws://", 1),
        ticket["ticket"].as_str().unwrap()
    );
    let (mut hb, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    hb.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),
               "label":"a20","platform":"windows"})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = until_frame(&mut hb, "host_ready", A01Dur::from_secs(10)).await;
    let host_id = ready.iter().find(|f| f["type"] == "host_ready").unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    (hb, host_id)
}

/// 往专属会话发一条消息发起调用，从宿主连接上拿到 invocation_start 的调用号。
async fn a20_invoke<S, H>(sa: &mut S, hb: &mut H, conv: &str, cmid: &str) -> String
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
    H: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    send_ws(sa, conv, cmid, "干活").await;
    let frames = until_frame(hb, "invocation_start", A01Dur::from_secs(15)).await;
    frames
        .iter()
        .rev()
        .find(|f| f["type"] == "invocation_start")
        .expect("宿主应收到 invocation_start")["invocation_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn a20_ask<H>(hb: &mut H, inv: &str, request_id: &str, detail: &str)
where
    H: SinkExt<WsMsg> + Unpin,
    <H as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    hb.send(WsMsg::Text(
        json!({
            "type": "approval_request",
            "invocation_id": inv,
            "request_id": request_id,
            "tool_call": {
                "title": "发消息到「专属会话」",
                "kind": "send_message",
                "locations": ["C:\\tmp\\a.txt"],
                "detail": detail,
            },
            "options": [
                {"optionId": "allow_once", "name": "本次允许", "kind": "allow_once"},
                {"optionId": "reject_once", "name": "拒绝", "kind": "reject_once"},
            ],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn a20_approval_detail_preserved_and_truncated() {
    let base = spawn_named_db(&format!("chat_it_a20_{}", uniq())).await;
    let a = register(&base, &format!("a20_{}", uniq())).await;
    let (mut hb, host_id) = a20_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "discloser").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // 完整保留：detail（含换行）原样进卡片 request 与内嵌 tool_call
    let inv1 = a20_invoke(&mut sa, &mut hb, &conv, "d1").await;
    let detail = "第一行\n第二行\n第三行";
    a20_ask(&mut hb, &inv1, "a20-r1", detail).await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    let req = &card["approval"]["request"];
    assert_eq!(req["detail"].as_str().unwrap(), detail, "{req}");
    assert_eq!(
        req["tool_call"]["detail"].as_str().unwrap(),
        detail,
        "{req}"
    );
    assert_eq!(req["locations"], json!(["C:\\tmp\\a.txt"]), "{req}");
    decide_approval(
        &base,
        &a,
        card["approval"]["id"].as_str().unwrap(),
        "reject_once",
    )
    .await;

    // 超过 4000 字：request 与内嵌 tool_call 都按字截断到 4000
    let inv2 = a20_invoke(&mut sa, &mut hb, &conv, "d2").await;
    let long = "字".repeat(4500);
    a20_ask(&mut hb, &inv2, "a20-r2", &long).await;
    let card = wait_pending_approval(&base, &a, &conv).await;
    let req = &card["approval"]["request"];
    assert_eq!(
        req["detail"].as_str().unwrap().chars().count(),
        4000,
        "detail 应截断到 4000 字：{req}"
    );
    assert_eq!(
        req["tool_call"]["detail"].as_str().unwrap().chars().count(),
        4000,
        "内嵌 tool_call.detail 同样截断：{req}"
    );
    decide_approval(
        &base,
        &a,
        card["approval"]["id"].as_str().unwrap(),
        "reject_once",
    )
    .await;

    // 没有 detail 的老请求照常处理，不破坏旧宿主
    let inv3 = a20_invoke(&mut sa, &mut hb, &conv, "d3").await;
    hb.send(WsMsg::Text(
        json!({
            "type": "approval_request",
            "invocation_id": inv3,
            "request_id": "a20-r3",
            "tool_call": {"title": "改文件", "kind": "edit", "locations": ["C:\\x"]},
            "options": [{"optionId": "allow", "name": "允许", "kind": "allow_once"}],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let card = wait_pending_approval(&base, &a, &conv).await;
    assert_eq!(card["approval"]["request"]["kind"], "edit");
}
