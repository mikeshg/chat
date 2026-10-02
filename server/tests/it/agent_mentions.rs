use super::*;

// ===== Agent A11 =====
// 主人在私聊 / 群里 @ 自己的 Agent。回复进原会话，状态帧给全部成员。

async fn send_ws_targets<S>(s: &mut S, conv: &str, cmid: &str, content: &str, targets: &[&str])
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    s.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": conv,
            "client_msg_id": cmid,
            "content": content,
            "agent_targets": targets,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

pub(crate) async fn delete_agent_req(base: &str, u: &TestUser, id: &str) -> u16 {
    http()
        .delete(format!("{base}/v1/agents/{id}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn mention_ids(msg: &Value) -> Vec<String> {
    msg["mentions"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// 等到会话里出现这些 Agent 回复（顺序不限）。超时 panic。
pub(crate) async fn wait_agent_reply(
    base: &str,
    u: &TestUser,
    conv: &str,
    want: &[&str],
) -> Vec<String> {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let texts = agent_texts(&conv_messages(base, u, conv).await);
        let ok = want.iter().all(|w| texts.iter().any(|t| t == w));
        if ok && texts.len() >= want.len() {
            return texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等 Agent 回复 {want:?} 超时，当前 {texts:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

/// 1. 群里主人手打 @bot → 成员收到回复和调用帧，双方未读都算上这条回复。
#[tokio::test]
async fn a11_group_owner_typed_at_reaches_members() {
    let base = spawn_named_db(&format!("chat_it_a11g_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let member = register(&base, &format!("a11m_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let gid = a11_group(&base, &owner, &[&member]).await;

    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let (mut sm, _) = tokio_tungstenite::connect_async(ws_url(&base, &member).await)
        .await
        .unwrap();
    send_ws(&mut so, &gid, "m1", "@bot echo:hi").await;

    let mut frames = until_frame(&mut sm, "agent_invocation", A01Dur::from_secs(20)).await;
    let mut saw_reply = frames.iter().any(|f| {
        f["type"] == "message"
            && f["message"]["agent"].is_object()
            && f["message"]["content"] == "hi"
    });
    while !saw_reply {
        frames.extend(until_frame(&mut sm, "message", A01Dur::from_secs(20)).await);
        saw_reply = frames.iter().any(|f| {
            f["type"] == "message"
                && f["message"]["agent"].is_object()
                && f["message"]["content"] == "hi"
        });
    }
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["content"] == "hi")
        .unwrap();
    assert_eq!(reply["message"]["agent"]["id"], agent_id);
    assert_eq!(reply["message"]["agent"]["name"], "bot");
    assert_eq!(reply["message"]["sender_id"], owner.id.as_str());
    assert!(
        frames.iter().any(|f| f["type"] == "agent_invocation"),
        "成员应收到 agent_invocation: {frames:?}"
    );

    // 主人的触发消息推进了自己的已读；回复对主人是 +1。
    // 成员没读触发消息，未读是触发消息 + 回复 = 2，其中回复贡献 +1。
    assert_eq!(a11_unread(&base, &owner, &gid).await, 1);
    assert_eq!(a11_unread(&base, &member, &gid).await, 2);
    host.shutdown().await;
}

/// 2. 别人 @ 主人的 Agent 不产生调用。
#[tokio::test]
async fn a11_other_member_at_does_not_invoke() {
    let base = spawn_named_db(&format!("chat_it_a11n_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let member = register(&base, &format!("a11m_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, _) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200);
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut sm, _) = tokio_tungstenite::connect_async(ws_url(&base, &member).await)
        .await
        .unwrap();
    send_ws(&mut sm, &gid, "m1", "@bot echo:x").await;
    let frames = frames_for(&mut sm, A01Dur::from_millis(1200)).await;
    assert!(
        !frames.iter().any(|f| f["type"] == "agent_invocation"),
        "非主人不应触发调用: {frames:?}"
    );
    let rows = list_inv(&base, &owner, &gid).await;
    assert!(rows.as_array().unwrap().is_empty(), "{rows}");
    assert!(agent_texts(&conv_messages(&base, &owner, &gid).await).is_empty());
    host.shutdown().await;
}

/// invocation_start.shared：专属会话里只有主人 → false；群里 @ → true（宿主据此让读其他会话先审批）
#[tokio::test]
async fn invocation_start_marks_shared_when_others_present() {
    let base = spawn_named_db(&format!("chat_it_shr_{}", uniq())).await;
    let owner = register(&base, &format!("shro_{}", uniq())).await;
    let member = register(&base, &format!("shrm_{}", uniq())).await;
    let (mut sh, host_id) = spawn_raw_host(&base, &owner).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let own_conv = agent["conversation_id"].as_str().unwrap().to_string();
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    for (conv, text, shared) in [
        (own_conv.as_str(), "hi", false),
        (gid.as_str(), "@bot hi", true),
    ] {
        send_ws(&mut so, conv, &uuid::Uuid::new_v4().to_string(), text).await;
        let f = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
        let start = f.iter().find(|x| x["type"] == "invocation_start").unwrap();
        assert_eq!(start["shared"], shared, "{text}: {start}");
    }
}

/// 3. 同名：主人的手打 @bot 调用 Agent 且不提及成员；别人的 @bot 只提及成员。
#[tokio::test]
async fn a11_same_name_prefers_own_agent() {
    let base = spawn_named_db(&format!("chat_it_a11s_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let bot_user = register(&base, "bot").await;
    let carol = register(&base, "carol").await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, _) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200);
    let gid = a11_group(&base, &owner, &[&bot_user, &carol]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let (mut sc, _) = tokio_tungstenite::connect_async(ws_url(&base, &carol).await)
        .await
        .unwrap();

    send_ws(&mut so, &gid, "m1", "@bot echo:a").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["a"]).await;
    assert_eq!(texts, vec!["a".to_string()]);
    let msgs = conv_messages(&base, &owner, &gid).await;
    let trigger = msgs.iter().find(|m| m["content"] == "@bot echo:a").unwrap();
    assert!(
        !mention_ids(trigger).contains(&bot_user.id),
        "主人的 @bot 不应提及同名成员: {trigger}"
    );

    send_ws(&mut sc, &gid, "m2", "@bot").await;
    let _ = until_frame(&mut so, "message", A01Dur::from_secs(10)).await;
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let rows = list_inv(&base, &owner, &gid).await;
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "别人的 @bot 不应再触发: {rows}"
    );
    let msgs = conv_messages(&base, &carol, &gid).await;
    let from_carol = msgs
        .iter()
        .find(|m| m["content"] == "@bot" && m["agent"].is_null())
        .unwrap();
    assert!(
        mention_ids(from_carol).contains(&bot_user.id),
        "别人的 @bot 应提及成员: {from_carol}"
    );

    // 大小写：@BOT 仍命中主人的 Agent bot
    send_ws(&mut so, &gid, "m3", "@BOT echo:c").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["a", "c"]).await;
    assert!(texts.contains(&"c".to_string()), "{texts:?}");
    let msgs = conv_messages(&base, &owner, &gid).await;
    let cased = msgs.iter().find(|m| m["content"] == "@BOT echo:c").unwrap();
    assert!(!mention_ids(cased).contains(&bot_user.id), "{cased}");

    // Agent 回复正文里的 @ 不再解析成提及
    send_ws(&mut so, &gid, "m4", "@bot echo:@carol").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["a", "c", "@carol"]).await;
    assert!(texts.contains(&"@carol".to_string()), "{texts:?}");
    let msgs = conv_messages(&base, &owner, &gid).await;
    let reply = msgs
        .iter()
        .find(|m| m["agent"].is_object() && m["content"] == "@carol")
        .unwrap();
    assert!(
        mention_ids(reply).is_empty(),
        "Agent 消息不应解析提及: {reply}"
    );
    host.shutdown().await;
}

/// 4. agent_targets 里别人的 Agent 忽略；自己的手打 @ 仍调用。
#[tokio::test]
async fn a11_foreign_agent_target_ignored() {
    let base = spawn_named_db(&format!("chat_it_a11f_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let other = register(&base, &format!("a11b_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (hb, host_b) = spawn_host(&base, &other, uuid::Uuid::new_v4()).await;
    let (st, mine) = create_agent(&base, &owner, &host_id, "mine").await;
    assert_eq!(st, 200);
    let (st, foreign) = create_agent(&base, &other, &host_b, "other").await;
    assert_eq!(st, 200);
    let foreign_id = foreign["id"].as_str().unwrap().to_string();
    hb.shutdown().await;
    let gid = a11_group(&base, &owner, &[&other]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();

    send_ws_targets(&mut so, &gid, "m1", "echo:nope", &[foreign_id.as_str()]).await;
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let rows = list_inv(&base, &owner, &gid).await;
    assert!(
        rows.as_array().unwrap().is_empty(),
        "别人的 id 应被忽略: {rows}"
    );

    send_ws_targets(&mut so, &gid, "m2", "@mine echo:y", &[foreign_id.as_str()]).await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["y"]).await;
    assert_eq!(texts, vec!["y".to_string()]);
    let rows = list_inv(&base, &owner, &gid).await;
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{rows}");
    assert_eq!(arr[0]["agent_id"], mine["id"]);
    assert_ne!(arr[0]["agent_id"], foreign_id.as_str());
    host.shutdown().await;
}

/// 5. 一条消息 @ 两个自己的 Agent → 两个调用、两条回复。
#[tokio::test]
async fn a11_two_agents_one_message() {
    let base = spawn_named_db(&format!("chat_it_a11t_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let member = register(&base, &format!("a11m_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, a1) = create_agent(&base, &owner, &host_id, "bot1").await;
    assert_eq!(st, 200);
    let (st, a2) = create_agent(&base, &owner, &host_id, "bot2").await;
    assert_eq!(st, 200);
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    send_ws(&mut so, &gid, "m1", "@bot1 @bot2 echo:z").await;
    let mut texts = wait_agent_reply(&base, &member, &gid, &["z", "z"]).await;
    texts.sort();
    assert_eq!(texts, vec!["z".to_string(), "z".to_string()]);
    let rows = list_inv(&base, &member, &gid).await;
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 2, "{rows}");
    let mut ids: Vec<&str> = arr
        .iter()
        .map(|r| r["agent_id"].as_str().unwrap())
        .collect();
    ids.sort();
    let mut want = vec![a1["id"].as_str().unwrap(), a2["id"].as_str().unwrap()];
    want.sort();
    assert_eq!(ids, want);
    assert!(arr.iter().all(|r| r["status"] == "succeeded"), "{rows}");
    let trigger = arr[0]["trigger_message_id"].as_str().unwrap();
    assert!(arr.iter().all(|r| r["trigger_message_id"] == trigger));
    host.shutdown().await;
}

/// 6. 私聊里的上下文与专属会话互不影响。
#[tokio::test]
async fn a11_dm_context_isolated_from_agent_conversation() {
    let base = spawn_named_db(&format!("chat_it_a11d_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let peer = register(&base, &format!("a11p_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let dedicated = agent["conversation_id"].as_str().unwrap().to_string();
    befriend(&base, &owner, &peer).await;
    let dm = dm_conv(&base, &owner).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();

    send_ws(&mut so, &dedicated, "e1", "echo:secret").await;
    let texts = wait_agent_reply(&base, &owner, &dedicated, &["secret"]).await;
    assert_eq!(texts, vec!["secret".to_string()]);

    send_ws(&mut so, &dm, "d1", "@bot recall").await;
    let texts = wait_agent_reply(&base, &owner, &dm, &["(empty)"]).await;
    assert_eq!(texts, vec!["(empty)".to_string()]);

    send_ws(&mut so, &dm, "d2", "@bot echo:fromdm").await;
    let texts = wait_agent_reply(&base, &owner, &dm, &["(empty)", "fromdm"]).await;
    assert!(texts.contains(&"fromdm".to_string()), "{texts:?}");

    send_ws(&mut so, &dedicated, "e2", "recall").await;
    // 第一条回复是 "secret"，recall 的正文才是 "echo:secret"
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    let reply = loop {
        let msgs = conv_messages(&base, &owner, &dedicated).await;
        let reply: Vec<&str> = msgs
            .iter()
            .filter(|m| m["agent"].is_object())
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect();
        if reply.iter().any(|t| t.contains("echo:secret")) {
            break reply.into_iter().map(str::to_string).collect::<Vec<_>>();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "专属会话 recall 超时: {reply:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    };
    assert!(
        reply.iter().any(|t| t.contains("echo:secret")),
        "专属会话应记得自己的指令: {reply:?}"
    );
    assert!(
        reply.iter().all(|t| !t.contains("fromdm")),
        "专属会话不应看到私聊上下文: {reply:?}"
    );
    host.shutdown().await;
}

/// 7. 群里宿主离线 → 成员看到 queued；上线后回复出现在群里。别人不能取消。
#[tokio::test]
async fn a11_group_offline_queues_for_members() {
    let base = spawn_named_db(&format!("chat_it_a11q_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let member = register(&base, &format!("a11m_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(&base, &owner, install).await;
    let (st, _) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200);
    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let (mut sm, _) = tokio_tungstenite::connect_async(ws_url(&base, &member).await)
        .await
        .unwrap();
    send_ws(&mut so, &gid, "q1", "@bot echo:back").await;
    let f = wait_inv(
        &mut sm,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let inv_id = f["invocation"]["id"].as_str().unwrap().to_string();
    let (code, _) = cancel_inv(&base, &member, &inv_id).await;
    assert_eq!(code, 404, "群成员不能取消主人的调用");
    let rows = list_inv(&base, &member, &gid).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "queued");

    let (h2, _) = spawn_host(&base, &owner, install).await;
    let texts = wait_agent_reply(&base, &member, &gid, &["back"]).await;
    assert_eq!(texts, vec!["back".to_string()]);
    h2.shutdown().await;
}

/// 8. 已删除的 Agent 不被手打 @ 触发。
#[tokio::test]
async fn a11_deleted_agent_not_triggered() {
    let base = spawn_named_db(&format!("chat_it_a11x_{}", uniq())).await;
    let owner = register(&base, &format!("a11o_{}", uniq())).await;
    let member = register(&base, &format!("a11m_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200);
    let id = agent["id"].as_str().unwrap();
    assert_eq!(delete_agent_req(&base, &owner, id).await, 204);
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    send_ws(&mut so, &gid, "m1", "@bot echo:x").await;
    tokio::time::sleep(A01Dur::from_millis(1000)).await;
    let rows = list_inv(&base, &owner, &gid).await;
    assert!(rows.as_array().unwrap().is_empty(), "{rows}");
    assert!(agent_texts(&conv_messages(&base, &member, &gid).await).is_empty());
    host.shutdown().await;
}
