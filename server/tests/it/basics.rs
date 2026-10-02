use super::*;

#[tokio::test]
async fn auth_flow() {
    let base = spawn().await;
    let c = http();
    let name = format!("u_{}", uniq());

    // 注册
    let u = register(&base, &name).await;
    assert!(!u.access.is_empty());

    // 重复用户名 → 409
    let r = c
        .post(format!("{base}/v1/auth/register"))
        .json(&json!({"username": name, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // 错误密码 → 401
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": name, "password": "wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 正确登录 + /users/me
    let r: Value = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": name, "password": "secret123"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let at = r["access_token"].as_str().unwrap();
    let me: Value = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(at))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["username"].as_str().unwrap(), name);

    // refresh 轮换
    let r: Value = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": u.refresh}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["access_token"].as_str().is_some());

    // 旧 refresh 已撤销
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": u.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn friend_flow_and_dm() {
    let base = spawn().await;
    let a = register(&base, &format!("a_{}", uniq())).await;
    let b = register(&base, &format!("b_{}", uniq())).await;
    let c = http();

    // 非好友拉群被拒
    let r = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "x", "member_ids": [b.id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    befriend(&base, &a, &b).await;

    // 双方好友列表可见 + DM 会话自动创建
    let friends: Value = c
        .get(format!("{base}/v1/friends"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(friends.as_array().unwrap().len(), 1);

    let conv = dm_conv(&base, &a).await;

    // 第三方 C 不是成员 → 403
    let c3 = register(&base, &format!("c_{}", uniq())).await;
    let r = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&c3.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 删除好友 → DM 只读（WS 层校验，REST 侧历史仍可读）
    let r = c
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
}

#[tokio::test]
async fn ws_message_and_sync() {
    let base = spawn().await;
    let a = register(&base, &format!("wa_{}", uniq())).await;
    let b = register(&base, &format!("wb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let c = http();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();

    // 双方都会收到 presence_init（可能在 presence 帧之后到达）
    next_of(&mut sa, "presence_init").await;
    next_of(&mut sb, "presence_init").await;

    // A 发消息 → A 收 message + ack；B 收 message
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":"cm1","content":"hello"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let msg_a = next_of(&mut sa, "message").await;
    assert_eq!(msg_a["message"]["seq"].as_i64().unwrap(), 1);
    let ack = next_of(&mut sa, "message_ack").await;
    assert_eq!(ack["client_msg_id"], "cm1");

    let f_b = next_of(&mut sb, "message").await;
    assert_eq!(f_b["message"]["content"], "hello");

    // B 的未读数 = 1；标记已读 → 未读归零
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unread = convs[0]["unread_count"].as_i64().unwrap();
    assert_eq!(unread, 1);

    sb.send(WsMsg::Text(
        json!({"type":"read","conversation_id":conv,"seq":1})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // 等待 read_sync 帧
    next_of(&mut sb, "read_sync").await;

    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(convs[0]["unread_count"].as_i64().unwrap(), 0);

    // 断线重连：sync 补齐
    drop(sb);
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"content":"m2"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // 等 ack 确认 m2 已提交，再重连 sync（否则存在提交-查询竞态）
    next_of(&mut sa, "message_ack").await;
    let (mut sb2, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb2, "presence_init").await;
    sb2.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":1})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let sync = next_of(&mut sb2, "sync_messages").await;
    assert_eq!(sync["messages"][0]["seq"].as_i64().unwrap(), 2);

    // 同一 client_msg_id 重放：不新分配 seq，历史不增加
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":"cm1","content":"hello again"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ack = next_of(&mut sa, "message_ack").await;
    assert_eq!(ack["message"]["seq"].as_i64().unwrap(), 1);
    assert_eq!(ack["message"]["content"], "hello");
    assert_eq!(ack["message"]["id"], msg_a["message"]["id"]);
    assert_eq!(ack["client_msg_id"], "cm1");

    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn dissolve_keeps_messages() {
    let base = spawn().await;
    let a = register(&base, &format!("da_{}", uniq())).await;
    let b = register(&base, &format!("db_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let c = http();
    let detail: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "keep", "member_ids": [b.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = detail["id"].as_str().unwrap();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":gid,"client_msg_id":"d1","content":"before dissolve"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    next_of(&mut sa, "message_ack").await;

    let r = c
        .delete(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let r = c
        .get(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let gid_uuid = uuid::Uuid::parse_str(gid).unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(gid_uuid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let dissolved: bool =
        sqlx::query_scalar("SELECT dissolved_at IS NOT NULL FROM conversations WHERE id = $1")
            .bind(gid_uuid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dissolved);
}

#[tokio::test]
async fn group_rules() {
    let base = spawn().await;
    let a = register(&base, &format!("ga_{}", uniq())).await;
    let b = register(&base, &format!("gb_{}", uniq())).await;
    let c3 = register(&base, &format!("gc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let c = http();

    // 建群：A + B（好友）
    let detail: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "test群", "member_ids": [b.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = detail["id"].as_str().unwrap();
    assert_eq!(detail["members"].as_array().unwrap().len(), 2);

    // B 拉非好友 C3 → 403
    let r = c
        .post(format!("{base}/v1/conversations/{gid}/members"))
        .header("authorization", authz(&b.access))
        .json(&json!({"user_ids": [c3.id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // B（非群主）不能改名
    let r = c
        .patch(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&b.access))
        .json(&json!({"name": "hacked"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // A 退群（群主）→ 自动转让给 B
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let detail: Value = c
        .get(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["owner_id"].as_str().unwrap(), b.id);
}
