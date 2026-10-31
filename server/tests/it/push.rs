use super::*;

// ===== V2 P3-3 =====
// ntfy 离线推送 + WS 心跳超时。全部用 spawn_named_state 独立库（改的是全局设置）。

/// 假 ntfy 服务器：收到的请求记进 hits——JSON body 原样，另加 "_method"/"_path"，
/// authorization 头并进 "_auth" 键。发布是 POST，清通知是 PUT /{topic}/{seq}/clear（无 body）。
struct FakeNtfy {
    base: String,
    hits: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

impl FakeNtfy {
    /// 等到至少 n 条记录或超时，返回快照
    async fn wait_hits(&self, n: usize, timeout_ms: u64) -> Vec<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let got = self.hits.lock().unwrap().clone();
            if got.len() >= n || std::time::Instant::now() >= deadline {
                return got;
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
    }

    fn hit_count(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

async fn fake_ntfy() -> FakeNtfy {
    let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = hits.clone();
    let app = axum::Router::new().fallback(
        move |method: axum::http::Method,
              uri: axum::http::Uri,
              headers: axum::http::HeaderMap,
              body: axum::body::Bytes| {
            let h = h.clone();
            async move {
                let mut v: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                v["_method"] = json!(method.as_str());
                v["_path"] = json!(uri.path());
                if let Some(a) = headers.get("authorization").and_then(|x| x.to_str().ok()) {
                    v["_auth"] = json!(a);
                }
                h.lock().unwrap().push(v);
                axum::http::StatusCode::OK
            }
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeNtfy { base, hits }
}

pub(crate) async fn admin_get_settings(base: &str, admin: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn push_view(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/users/me/push"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn push_put(base: &str, u: &TestUser, body: Value) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/users/me/push"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn add_device(base: &str, u: &TestUser, name: &str) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/users/me/push/devices"))
        .header("authorization", authz(&u.access))
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap()
}

async fn test_device(base: &str, u: &TestUser, id: &str) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/users/me/push/devices/{id}/test"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

async fn delete_device(base: &str, u: &TestUser, id: &str) -> reqwest::Response {
    http()
        .delete(format!("{base}/v1/users/me/push/devices/{id}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

/// 设备订阅地址中的主题部分（{base}/{topic}）
fn topic_of(d: &Value) -> String {
    d["subscribe_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string()
}

/// 改推送设置 + 登记一台手机，返回其主题
async fn push_on(base: &str, u: &TestUser, body: Value) -> String {
    assert_eq!(push_put(base, u, body).await.status(), 200);
    let r = add_device(base, u, "手机").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    topic_of(v["devices"].as_array().unwrap().last().unwrap())
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn ws_of(base: &str, u: &TestUser) -> Ws {
    let (mut s, _) = tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap();
    next_of(&mut s, "presence_init").await;
    s
}

/// 注册 admin + 两个用户并建好友关系；配置 ntfy；返回 (admin, a, b, conv)
async fn p33_setup(
    base: &str,
    ntfy: &FakeNtfy,
    tag: &str,
) -> (TestUser, TestUser, TestUser, String) {
    let admin = register(base, &format!("p33{tag}adm_{}", uniq())).await;
    let a = register(base, &format!("p33{tag}a_{}", uniq())).await;
    let b = register(base, &format!("p33{tag}b_{}", uniq())).await;
    befriend(base, &a, &b).await;
    let conv = dm_conv(base, &a).await;
    let r = admin_put_settings(base, &admin, json!({"push_ntfy_base": ntfy.base})).await;
    assert_eq!(r.status(), 200);
    (admin, a, b, conv)
}

/// 1. B 开推送并登记手机 → A 私聊发消息 → 假 ntfy 收到一条：topic 正确、
///    标题 = A 的昵称、正文不含消息内容（preview 默认关）、sequence_id = 会话。
///    test 端点给单台设备直达一条测试推送。
#[tokio::test]
async fn p33_push_dm_basic() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33a_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "a").await;

    let r = push_put(&base, &b, json!({"enabled": true})).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["available"], true);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["preview"], false);
    assert_eq!(v["devices"], json!([]));

    let r = add_device(&base, &b, "  我的手机 ").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let d = &v["devices"][0];
    assert_eq!(d["name"], "我的手机");
    let topic = topic_of(d);
    assert!(topic.starts_with("chat-"), "topic: {topic}");
    assert_eq!(topic.len(), 5 + 24);
    assert_eq!(d["subscribe_url"], format!("{}/{}", ntfy.base, topic));
    let long = "x".repeat(51);
    for bad in ["", "   ", long.as_str()] {
        assert_eq!(add_device(&base, &b, bad).await.status(), 400);
    }
    let id = d["id"].as_str().unwrap();

    // 测试推送：不受合并限制，立即到达
    assert_eq!(test_device(&base, &b, id).await.status(), 200);
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["topic"], topic);
    assert_eq!(hits[0]["title"], "Chat");
    assert!(hits[0]["sequence_id"].is_null());
    assert_eq!(test_device(&base, &a, id).await.status(), 404, "别人的设备");

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "pm1", "content": "hello p33"}),
    )
    .await;

    let hits = ntfy.wait_hits(2, 3000).await;
    assert_eq!(hits.len(), 2);
    let msg_hit = hits.iter().find(|h| h["title"] == a.username).unwrap();
    assert_eq!(msg_hit["topic"], topic);
    assert_eq!(msg_hit["message"], "你收到新消息");
    assert_eq!(msg_hit["priority"], 3);
    assert_eq!(msg_hit["tags"], json!(["speech_balloon"]));
    assert_eq!(msg_hit["sequence_id"], conv);
}

/// 2. B 在线且前台也照推，两台手机各一条；B 读了会话 → 每台手机收到
///    PUT /{topic}/{conv}/clear，且合并窗口被重置（读完后的下一条立即推）。
///    没推过的人（发送者 A）推进已读不发 clear。
#[tokio::test]
async fn p33_push_always_and_clear_on_read() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33b_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "b").await;
    let t1 = push_on(&base, &b, json!({"enabled": true})).await;
    let t2 = push_on(&base, &b, json!({})).await;
    let mut both = vec![t1.clone(), t2.clone()];
    both.sort();

    let mut sa = ws_of(&base, &a).await;
    let mut sb = ws_of(&base, &b).await;
    let m = ws_send(&mut sa, &conv, json!({"client_msg_id": "pm1", "content": "one"})).await;
    let hits = ntfy.wait_hits(2, 3000).await;
    let mut topics: Vec<String> = hits
        .iter()
        .map(|h| h["topic"].as_str().unwrap().to_string())
        .collect();
    topics.sort();
    assert_eq!(topics, both, "B 在线也推，两台手机各一条");

    // B 读到这条 → 两台手机清通知
    sb.send(WsMsg::Text(
        json!({"type":"read","conversation_id":conv,"seq":m["seq"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let hits = ntfy.wait_hits(4, 3000).await;
    let mut clears: Vec<String> = hits
        .iter()
        .filter(|h| h["_method"] == "PUT")
        .map(|h| h["_path"].as_str().unwrap().to_string())
        .collect();
    clears.sort();
    let want: Vec<String> = both.iter().map(|t| format!("/{t}/{conv}/clear")).collect();
    assert_eq!(clears, want);

    // 窗口已重置：下一条立即推
    ws_send(&mut sa, &conv, json!({"client_msg_id": "pm2", "content": "two"})).await;
    assert_eq!(ntfy.wait_hits(6, 3000).await.len(), 6, "读后下一条应立即推");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(ntfy.hit_count(), 6, "A 没被推过，发言推进已读不发 clear");
}

/// 3. B 静音群会话 → 普通消息不推；@B 的消息推（priority=4，标题=群名+未读数）。
#[tokio::test]
async fn p33_push_mute_and_mention() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33c_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, _dm) = p33_setup(&base, &ntfy, "c").await;
    push_on(&base, &b, json!({"enabled": true})).await;

    let gid = mk_group(&base, &a, std::slice::from_ref(&b.id)).await;
    // B 永久静音该群
    let r = put_pref(&base, &b, &gid, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);
    // mk_group 的 group_created 可能已在静音前推过；之后只看增量
    let baseline = ntfy.hit_count();

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "gm1", "content": "plain group msg"}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(ntfy.hit_count(), baseline, "静音会话普通消息不推");

    ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "gm2", "content": format!("@{} 看这里", b.username)}),
    )
    .await;
    let hits = ntfy.wait_hits(baseline + 1, 3000).await;
    assert_eq!(hits.len(), baseline + 1, "@我 应突破静音推送");
    let mention = &hits[hits.len() - 1];
    assert_eq!(mention["priority"], 4, "@我 推送 priority=high");
    let title = mention["title"].as_str().unwrap();
    assert!(title.starts_with("g ("), "群聊推送标题为群名 + 未读数: {title}");
    assert_eq!(mention["message"], "有人@了你");
}

/// 4. 合并：窗口内连发 3 条只推 1 条；窗口外照推，标题带累计未读数，sequence_id 不变。
#[tokio::test]
async fn p33_push_coalesce() {
    let (_guard, base, st) = spawn_named_state(&format!("chat_it_p33d_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "d").await;
    push_on(&base, &b, json!({"enabled": true})).await;

    let mut sa = ws_of(&base, &a).await;
    for i in 1..=3 {
        ws_send(
            &mut sa,
            &conv,
            json!({"client_msg_id": format!("cm{i}"), "content": format!("burst {i}")}),
        )
        .await;
    }
    let hits = ntfy.wait_hits(3, 4000).await;
    assert_eq!(hits.len(), 1, "窗口内同一 (用户,会话) 只推一条");

    // 关掉窗口：第 4 条照推，标题是累计未读
    st.push_coalesce_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);
    ws_send(&mut sa, &conv, json!({"client_msg_id": "cm4", "content": "burst 4"})).await;
    let hits = ntfy.wait_hits(2, 3000).await;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[1]["title"], format!("{} (4条)", a.username));
    assert_eq!(hits[1]["sequence_id"], conv, "同一会话原地更新同一条通知");
}

/// 5. preview 开 → 正文是「昵称: 内容」。
#[tokio::test]
async fn p33_push_preview() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33e_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "e").await;
    push_on(&base, &b, json!({"enabled": true, "preview": true})).await;

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "pv1", "content": "来看这个"}),
    )
    .await;

    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["message"], format!("{}: 来看这个", a.username));
}

/// 6. 服务器未配置：enabled=true / 登记设备 → 400；available=false、devices 空、
///    server-info.push_available=false。配置后登记了设备但未开启 → 发消息不产生请求。
#[tokio::test]
async fn p33_push_unconfigured() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33f_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let admin = register(&base, &format!("p33fadm_{}", uniq())).await;
    let a = register(&base, &format!("p33fa_{}", uniq())).await;
    let b = register(&base, &format!("p33fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    assert_eq!(push_put(&base, &b, json!({"enabled": true})).await.status(), 400);
    assert_eq!(add_device(&base, &b, "手机").await.status(), 400);
    let v = push_view(&base, &b).await;
    assert_eq!(v["available"], false);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["devices"], json!([]));
    let si: Value = http()
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(si["push_available"], false);

    // 配置后登记设备但仍未开启 → 不产生任何推送请求
    let r = admin_put_settings(&base, &admin, json!({"push_ntfy_base": ntfy.base})).await;
    assert_eq!(r.status(), 200);
    push_on(&base, &b, json!({"preview": true})).await;
    let v = push_view(&base, &b).await;
    assert_eq!(v["available"], true);
    assert_eq!(v["enabled"], false);
    assert!(v["devices"][0]["subscribe_url"].is_string());

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "uc1", "content": "no push"}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(ntfy.hit_count(), 0, "未开启推送的成员不产生请求");
}

/// 7. 管理员设置：非管理员 403；token 只写不读（GET 只有 push_ntfy_token_set）；
///    非法地址 400；配置令牌后推送带 Authorization；空串清除。
#[tokio::test]
async fn p33_admin_settings() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33g_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let admin = register(&base, &format!("p33gadm_{}", uniq())).await;
    let a = register(&base, &format!("p33ga_{}", uniq())).await;
    let b = register(&base, &format!("p33gb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    // 非管理员 → 403
    let r = admin_put_settings(&base, &b, json!({"push_ntfy_base": "https://ntfy.sh"})).await;
    assert_eq!(r.status(), 403);

    // 非法地址 → 400
    for bad in [
        json!({"push_ntfy_base": "ftp://ntfy.sh"}),
        json!({"push_ntfy_base": format!("http://{}", "x".repeat(210))}),
        json!({"public_url": "not-a-url"}),
    ] {
        let r = admin_put_settings(&base, &admin, bad).await;
        assert_eq!(r.status(), 400);
    }

    // 合法配置 + 令牌
    let r = admin_put_settings(
        &base,
        &admin,
        json!({
            "push_ntfy_base": ntfy.base,
            "push_ntfy_token": "tok_secret",
            "public_url": "https://chat.example.com/",
        }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let s = admin_get_settings(&base, &admin).await;
    assert_eq!(s["push_ntfy_base"], ntfy.base);
    assert_eq!(s["public_url"], "https://chat.example.com");
    assert_eq!(s["push_ntfy_token_set"], true);
    assert!(s["push_ntfy_token"].is_null(), "GET 不得回显令牌: {s}");

    // 推送请求携带 Bearer 令牌与 Click
    push_on(&base, &b, json!({"enabled": true})).await;
    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "tk1", "content": "with token"}),
    )
    .await;
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["_auth"], "Bearer tok_secret");
    assert_eq!(hits[0]["click"], "https://chat.example.com");

    // 空串清除令牌
    let r = admin_put_settings(&base, &admin, json!({"push_ntfy_token": ""})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        admin_get_settings(&base, &admin).await["push_ntfy_token_set"],
        false
    );
}

/// 8. 移除一台手机：只有剩下那台还收到推送；别人删不了；删不存在的 404。
#[tokio::test]
async fn p33_push_delete_device() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33h_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "h").await;
    let lost = push_on(&base, &b, json!({"enabled": true})).await;
    let kept = push_on(&base, &b, json!({})).await;
    let v = push_view(&base, &b).await;
    let lost_id = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| topic_of(d) == lost)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(delete_device(&base, &a, &lost_id).await.status(), 404, "别人的设备删不了");
    let r = delete_device(&base, &b, &lost_id).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);
    assert_eq!(delete_device(&base, &b, &lost_id).await.status(), 404);

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "dd1", "content": "after delete"}),
    )
    .await;
    let hits = ntfy.wait_hits(2, 2000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["topic"], kept);
}

/// 9. 心跳：超时 1s + ping 200ms。僵尸客户端（握手后不再读帧，也就不会回 Pong）
///    应在超时窗口内被服务端关闭，好友看到其离线；正常读帧的客户端不受影响。
#[tokio::test]
async fn p33_ws_idle_timeout() {
    let (_guard, base, st) = spawn_named_state(&format!("chat_it_p33i_{}", uniq())).await;
    st.ws_ping_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    st.ws_idle_timeout_ms
        .store(1000, std::sync::atomic::Ordering::Relaxed);

    let a = register(&base, &format!("p33ia_{}", uniq())).await;
    let z = register(&base, &format!("p33iz_{}", uniq())).await;
    befriend(&base, &a, &z).await;
    let conv = dm_conv(&base, &a).await;

    let mut sa = ws_of(&base, &a).await;

    // 僵尸连接：握手成功后保持 socket 但永不 poll（不会回 Pong）
    let _zombie = tokio_tungstenite::connect_async(ws_url(&base, &z).await)
        .await
        .unwrap();

    // A 先看到 Z 上线
    let zid = z.id.clone();
    let saw_online = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match sa.next().await {
                Some(Ok(WsMsg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "presence" && v["user_id"] == zid && v["online"] == true {
                        break true;
                    }
                }
                Some(Ok(_)) => {}
                _ => break false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_online, "A 应先看到 Z 上线");

    // 再在超时窗口内看到 Z 掉线（服务端主动断开 + 离线广播）
    let saw_offline = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        loop {
            match sa.next().await {
                Some(Ok(WsMsg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "presence" && v["user_id"] == zid && v["online"] == false {
                        break true;
                    }
                }
                Some(Ok(_)) => {}
                _ => break false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_offline, "僵尸连接应被心跳超时关闭并广播离线");
    assert!(!st.is_online(uuid::Uuid::parse_str(&z.id).unwrap()));

    // 正常客户端（poll 驱动自动回 Pong）未被断开：还能发消息拿 ack
    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "alive1", "content": "still here"}),
    )
    .await;
    assert_eq!(m["content"], "still here");
}

/// 10. 推送发送失败（连接拒绝）不影响消息正常送达；test 端点失败 503 带原因。
#[tokio::test]
async fn p33_push_failure_isolated() {
    let (_guard, base, _st) = spawn_named_state(&format!("chat_it_p33j_{}", uniq())).await;
    // 拿一个已释放的回环端口 → 连接拒绝
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}")
    };
    let admin = register(&base, &format!("p33jadm_{}", uniq())).await;
    let a = register(&base, &format!("p33ja_{}", uniq())).await;
    let b = register(&base, &format!("p33jb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let r = admin_put_settings(&base, &admin, json!({"push_ntfy_base": dead})).await;
    assert_eq!(r.status(), 200);
    push_on(&base, &b, json!({"enabled": true})).await;
    let dev = push_view(&base, &b).await["devices"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 离线推送失败只记日志：消息仍正常落库与回执
    let mut sa = ws_of(&base, &a).await;
    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "fj1", "content": "delivered"}),
    )
    .await;
    assert_eq!(m["content"], "delivered");
    let page: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);

    // test 端点：投递失败 → 503 带原因
    let r = test_device(&base, &b, &dev).await;
    assert_eq!(r.status(), 503);
    assert!(r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("推送"));
}

// ===== MF1 =====
// chat_record 推送摘要 = 「[聊天记录] {title}」，永不退化成多行兜底正文；
// separate 批量一目标一请求至多一条推送（摘取末条），WS 仍逐条一帧。

/// a 合并转发文本到与 b 的私聊：b 的推送正文 = 「{a}: [聊天记录] {a} 和 {b} 的聊天记录」。
#[tokio::test]
async fn mf1_push_chat_record_excerpt() {
    let (_guard, base, st) = spawn_named_state(&format!("chat_it_mf1p_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "cr").await;
    push_on(&base, &b, json!({"enabled": true, "preview": true})).await;
    // 关掉合并窗口：否则会话内第二条推送被压掉，没法断言摘要
    st.push_coalesce_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let mut sa = ws_of(&base, &a).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "cr1", "content": "收进记录"}),
    )
    .await;
    // 源消息本身产生一条普通推送，先等它到齐
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);

    let page = conv_messages(&base, &a, &conv).await;
    let src = page[0]["id"].as_str().unwrap().to_string();
    let r = forward_batch::post_batch(
        &base,
        &a,
        &conv,
        &[src],
        std::slice::from_ref(&conv),
        "merged",
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(r.status(), 200);

    let hits = ntfy.wait_hits(2, 3000).await;
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[1]["message"],
        format!(
            "{}: [聊天记录] {} 和 {} 的聊天记录",
            a.username, a.username, b.username
        ),
        "记录推送摘要 = [聊天记录] 标题，不得含兜底正文行: {}",
        hits[1]["message"]
    );
}

/// separate 批量 5 条：每条 WS 一帧，但一目标一请求只发一条推送（摘取末条）。
/// 把合并窗口关掉（coalesce=0）才能区分「发了几条」与「窗口压了几条」。
#[tokio::test]
async fn mf1_push_separate_batch_once() {
    let (_guard, base, st) = spawn_named_state(&format!("chat_it_mf1s_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "sb").await;
    push_on(&base, &b, json!({"enabled": true, "preview": true})).await;
    st.push_coalesce_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let mut sa = ws_of(&base, &a).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        let m = ws_send(
            &mut sa,
            &conv,
            json!({"client_msg_id": format!("s{i}"), "content": format!("第{i}条")}),
        )
        .await;
        ids.push(m["id"].as_str().unwrap().to_string());
    }
    let hits = ntfy.wait_hits(5, 3000).await;
    assert_eq!(hits.len(), 5, "五条源消息各一推");
    let _ = conv_messages(&base, &a, &conv).await;

    // b 连 WS 收帧；a 逐条转发 5 条回本会话
    let mut sb = ws_of(&base, &b).await;
    let r = forward_batch::post_batch(
        &base,
        &a,
        &conv,
        &ids,
        std::slice::from_ref(&conv),
        "separate",
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["results"][0]["forwarded"], 5);

    // WS：恰好 5 个 message 帧
    let mut kinds = Vec::new();
    for _ in 0..5 {
        kinds.push(next_of(&mut sb, "message").await["message"]["kind"].clone());
    }
    assert_eq!(kinds, vec![json!("text"); 5], "每条各一帧: {kinds:?}");

    // 推送：一目标一请求至多一条，摘要来自末条（"第4条"）
    let hits = ntfy.wait_hits(7, 2500).await;
    assert_eq!(
        hits.len(),
        6,
        "separate 5 条只追加一条推送（末条）: {}",
        hits.len()
    );
    assert_eq!(
        hits[5]["message"],
        format!("{}: 第4条", a.username),
        "推送摘要应取末条: {}",
        hits[5]["message"]
    );
}
