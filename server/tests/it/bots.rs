use super::*;

async fn create_bot(base: &str, u: &TestUser, name: &str, webhook: &str) -> Value {
    let r = http()
        .post(format!("{base}/v1/bots"))
        .header("authorization", authz(&u.access))
        .json(&json!({"username": name, "webhook_url": webhook}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    r.json().await.unwrap()
}

async fn hook(base: &str, token: &str, body: Value) -> u16 {
    http()
        .post(format!("{base}/v1/hooks/{token}"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn dm_with(convs: &Value, peer: &str) -> String {
    convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "dm" && c["peer"]["id"] == peer)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 等会话里出现满足条件的消息（bot 回调是异步的）
async fn wait_message(
    base: &str,
    u: &TestUser,
    conv: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..50 {
        if let Some(m) = conv_messages(base, u, conv)
            .await
            .into_iter()
            .find(|m| pred(m))
        {
            return m;
        }
        tokio::time::sleep(Dur::from_millis(100)).await;
    }
    panic!("message never arrived");
}

/// 假 bot：记下收到的 (签名头, body)，回 {"content": "pong: <原文>"}
struct FakeBot {
    url: String,
    hits: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

async fn fake_bot() -> FakeBot {
    let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = hits.clone();
    let app = axum::Router::new().fallback(axum::routing::post(
        move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let h = h.clone();
            async move {
                let v: Value = serde_json::from_slice(&body).unwrap();
                let sig = headers
                    .get("x-chat-signature")
                    .and_then(|x| x.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let reply = format!("pong: {}", v["message"]["content"].as_str().unwrap());
                h.lock().unwrap().push((sig, v));
                axum::Json(json!({ "content": reply }))
            }
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeBot { url, hits }
}

#[tokio::test]
async fn bot_lifecycle_and_inbound_hook() {
    let base = spawn().await;
    let owner = register(&base, &format!("bo{}", uniq())).await;
    let other = register(&base, &format!("bx{}", uniq())).await;
    let name = format!("bot{}", uniq());
    let bot = create_bot(&base, &owner, &name, "").await;
    let bot_id = bot["id"].as_str().unwrap();
    let token = bot["token"].as_str().unwrap().to_string();

    // 同名再建 → 409
    let dup = http()
        .post(format!("{base}/v1/bots"))
        .header("authorization", authz(&owner.access))
        .json(&json!({"username": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status().as_u16(), 409);

    // 主人的好友和私聊里都有它，带 is_bot
    let friends: Value = http()
        .get(format!("{base}/v1/friends"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let f = friends
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["user"]["id"] == bot_id)
        .unwrap();
    assert_eq!(f["user"]["is_bot"], true);
    let dm = dm_with(&conv_list(&base, &owner).await, bot_id);

    // 入站：默认发到和主人的私聊
    assert_eq!(
        hook(&base, &token, json!({"content": "build ok"})).await,
        200
    );
    let m = wait_message(&base, &owner, &dm, |m| m["content"] == "build ok").await;
    assert_eq!(m["sender_id"], bot_id);
    assert_eq!(hook(&base, "nope", json!({"content": "x"})).await, 404);

    // 别人搜不到、加不了；bot 也登录不了
    let found: Value = http()
        .get(format!("{base}/v1/users/search?q={name}"))
        .header("authorization", authz(&other.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 0);
    let req = http()
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&other.access))
        .json(&json!({"username": name, "message": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(req.status().as_u16(), 404);
    let login = http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": name, "password": "!"}))
        .send()
        .await
        .unwrap();
    assert!(login.status().is_client_error());

    // 不是成员的会话发不进去
    let foreign = self_conv(&base, &other).await["id"].clone();
    assert_eq!(
        hook(
            &base,
            &token,
            json!({"content": "x", "conversation_id": foreign})
        )
        .await,
        403
    );

    // 换令牌：旧的立即失效
    let reset: Value = http()
        .post(format!("{base}/v1/bots/{bot_id}/token"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(reset["token"], token.as_str());
    assert_eq!(hook(&base, &token, json!({"content": "x"})).await, 404);

    // 别人删不了；主人删掉后入站失效、私聊只读
    let r = http()
        .delete(format!("{base}/v1/bots/{bot_id}"))
        .header("authorization", authz(&other.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = http()
        .delete(format!("{base}/v1/bots/{bot_id}"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let new_token = reset["token"].as_str().unwrap();
    assert_eq!(hook(&base, new_token, json!({"content": "x"})).await, 404);
    let list: Value = http()
        .get(format!("{base}/v1/bots"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn bot_webhook_replies_in_dm_and_when_mentioned_in_group() {
    let base = spawn().await;
    let fake = fake_bot().await;
    let owner = register(&base, &format!("wo{}", uniq())).await;
    let b = register(&base, &format!("wb{}", uniq())).await;
    befriend(&base, &owner, &b).await;
    let name = format!("echo{}", uniq());
    let bot = create_bot(&base, &owner, &name, &fake.url).await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let dm = dm_with(&conv_list(&base, &owner).await, &bot_id);

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    ws_send(
        &mut ws,
        &dm,
        json!({"client_msg_id": "c1", "content": "ping"}),
    )
    .await;
    let reply = wait_message(&base, &owner, &dm, |m| m["content"] == "pong: ping").await;
    assert_eq!(reply["sender_id"], bot_id.as_str());
    {
        let hits = fake.hits.lock().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.starts_with("sha256=") && hits[0].0.len() == 7 + 64);
        assert_eq!(hits[0].1["conversation_type"], "dm");
        assert_eq!(hits[0].1["message"]["sender_id"], owner.id.as_str());
    }

    // 群里：没 @ 不回调，@ 了才回调；bot 自己的回复不再触发回调
    let group = mk_group(&base, &owner, &[b.id.clone(), bot_id.clone()]).await;
    let (mut wb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    ws_send(
        &mut wb,
        &group,
        json!({"client_msg_id": "g1", "content": "hi all"}),
    )
    .await;
    let mention = format!("@{name} status?");
    ws_send(
        &mut wb,
        &group,
        json!({"client_msg_id": "g2", "content": mention}),
    )
    .await;
    let want = format!("pong: {mention}");
    wait_message(&base, &b, &group, |m| m["content"] == want.as_str()).await;
    tokio::time::sleep(Dur::from_millis(300)).await;
    let hits = fake.hits.lock().unwrap();
    assert_eq!(hits.len(), 2, "only the DM and the @ trigger the webhook");
    assert_eq!(hits[1].1["conversation_type"], "group");
}

#[tokio::test]
async fn group_with_only_bots_dissolves_when_last_human_leaves() {
    let base = spawn().await;
    let owner = register(&base, &format!("lo{}", uniq())).await;
    let bot = create_bot(&base, &owner, &format!("lb{}", uniq()), "").await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let group = mk_group(&base, &owner, &[bot_id.clone()]).await;

    // 不能把群主转给 bot
    let r = http()
        .post(format!("{base}/v1/conversations/{group}/transfer"))
        .header("authorization", authz(&owner.access))
        .json(&json!({"user_id": bot_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);

    let r = http()
        .delete(format!("{base}/v1/conversations/{group}/members/me"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let token = bot["token"].as_str().unwrap();
    assert_eq!(
        hook(
            &base,
            token,
            json!({"content": "x", "conversation_id": group})
        )
        .await,
        403,
        "group is gone, not handed to the bot"
    );
}
