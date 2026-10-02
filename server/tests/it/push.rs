use super::*;

// ===== V2 P3-3 =====
// ntfy 离线推送 + WS 心跳超时。全部用 spawn_named_state 独立库（改的是全局设置）。

/// 假 ntfy 服务器：收到的 JSON body 记进 hits（authorization 头并进 "_auth" 键）。
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
    let app = axum::Router::new().fallback(axum::routing::post(
        move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let h = h.clone();
            async move {
                let mut v: Value =
                    serde_json::from_slice(&body).unwrap_or_else(|_| json!({"_raw": true}));
                if let Some(a) = headers.get("authorization").and_then(|x| x.to_str().ok()) {
                    v["_auth"] = json!(a);
                }
                h.lock().unwrap().push(v);
                axum::http::StatusCode::OK
            }
        },
    ));
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

async fn push_post(base: &str, u: &TestUser, action: &str) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/users/me/push/{action}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

/// subscribe_url 中的主题部分（{base}/{topic}）
fn topic_of(v: &Value) -> String {
    v["subscribe_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string()
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

/// 1. B 开推送且无 WS 连接 → A 私聊发消息 → 假 ntfy 收到一条：topic 正确、
///    标题 = A 的昵称、正文不含消息内容（preview 默认关）。test 端点直达一条测试推送。
#[tokio::test]
async fn p33_push_dm_basic() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33a_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "a").await;

    let r = push_put(&base, &b, json!({"enabled": true})).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["available"], true);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["preview"], false);
    let topic = topic_of(&v);
    assert!(topic.starts_with("chat-"), "topic: {topic}");
    assert_eq!(topic.len(), 5 + 24);
    assert_eq!(v["subscribe_url"], format!("{}/{}", ntfy.base, topic));

    // 测试推送：不受合并限制，立即到达
    let r = push_post(&base, &b, "test").await;
    assert_eq!(r.status(), 200);
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["topic"], topic);
    assert_eq!(hits[0]["title"], "Chat");

    // B 没有任何连接 → A 发消息触发离线推送
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
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
    assert_eq!(msg_hit["message"], "你收到一条新消息");
    assert_eq!(msg_hit["priority"], 3);
    assert_eq!(msg_hit["tags"], json!(["speech_balloon"]));
}

/// 2. B 有活跃连接不推；client_state{active:false} 后推；恢复 true 又不推。
#[tokio::test]
async fn p33_push_suppressed_by_active_conn() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33b_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "b").await;
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    // B 的连接默认 active=true（兼容不上报的旧客户端）
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "pm1", "content": "one"}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(ntfy.hit_count(), 0, "B 有活跃连接时不应推送");

    // B 上报后台/失焦 → 下一条消息触发推送
    sb.send(WsMsg::Text(
        json!({"type":"client_state","active":false})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "pm2", "content": "two"}),
    )
    .await;
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1, "B 无活跃连接时应推送");
    assert_eq!(hits[0]["message"], "你收到一条新消息");

    // B 回前台 → 又不推
    sb.send(WsMsg::Text(
        json!({"type":"client_state","active":true})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "pm3", "content": "three"}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(ntfy.hit_count(), 1, "B 回前台后不应再推送");
}

/// 3. B 静音群会话 → 普通消息不推；@B 的消息推（priority=4，标题=群名）。
#[tokio::test]
async fn p33_push_mute_and_mention() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33c_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, _dm) = p33_setup(&base, &ntfy, "c").await;
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );

    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    // B 永久静音该群
    let r = put_pref(&base, &b, &gid, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "gm1", "content": "plain group msg"}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(ntfy.hit_count(), 0, "静音会话普通消息不推");

    ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "gm2", "content": format!("@{} 看这里", b.username)}),
    )
    .await;
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1, "@我 应突破静音推送");
    assert_eq!(hits[0]["priority"], 4, "@我 推送 priority=high");
    assert_eq!(hits[0]["title"], "g", "群聊推送标题为群名");
    assert_eq!(hits[0]["message"], "有人@了你");
}

/// 4. 30s 合并：连发 3 条只推 1 条。
#[tokio::test]
async fn p33_push_coalesce() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33d_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "d").await;
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    for i in 1..=3 {
        ws_send(
            &mut sa,
            &conv,
            json!({"client_msg_id": format!("cm{i}"), "content": format!("burst {i}")}),
        )
        .await;
    }
    let hits = ntfy.wait_hits(3, 4000).await;
    assert_eq!(hits.len(), 1, "30s 窗口内同一 (用户,会话) 只推一条");
}

/// 5. preview 开 → 正文是「昵称: 内容」。
#[tokio::test]
async fn p33_push_preview() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33e_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "e").await;
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true, "preview": true}))
            .await
            .status(),
        200
    );

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
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

/// 6. 服务器未配置：enabled=true → 400；available=false、subscribe_url=null、
///    server-info.push_available=false。配置后但未开启 → 发消息不产生请求。
#[tokio::test]
async fn p33_push_unconfigured() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33f_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let admin = register(&base, &format!("p33fadm_{}", uniq())).await;
    let a = register(&base, &format!("p33fa_{}", uniq())).await;
    let b = register(&base, &format!("p33fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let r = push_put(&base, &b, json!({"enabled": true})).await;
    assert_eq!(r.status(), 400);
    let v = push_view(&base, &b).await;
    assert_eq!(v["available"], false);
    assert_eq!(v["enabled"], false);
    assert!(v["subscribe_url"].is_null());
    let si: Value = http()
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(si["push_available"], false);

    // 配置后仍未开启 → 不产生任何推送请求
    let r = admin_put_settings(&base, &admin, json!({"push_ntfy_base": ntfy.base})).await;
    assert_eq!(r.status(), 200);
    let v = push_view(&base, &b).await;
    assert_eq!(v["available"], true);
    assert_eq!(v["enabled"], false);
    assert!(v["subscribe_url"].is_string());

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
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
    let (base, _st) = spawn_named_state(&format!("chat_it_p33g_{}", uniq())).await;
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
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
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

/// 8. rotate 后主题变化，推送走新主题（旧主题不再收到）。
#[tokio::test]
async fn p33_push_rotate() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33h_{}", uniq())).await;
    let ntfy = fake_ntfy().await;
    let (_admin, a, b, conv) = p33_setup(&base, &ntfy, "h").await;
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );
    let old = push_view(&base, &b).await;
    let old_topic = topic_of(&old);

    let r = push_post(&base, &b, "rotate").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let new_topic = topic_of(&v);
    assert_ne!(new_topic, old_topic);
    assert_eq!(v["subscribe_url"], format!("{}/{}", ntfy.base, new_topic));

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "rt1", "content": "after rotate"}),
    )
    .await;
    let hits = ntfy.wait_hits(1, 3000).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["topic"], new_topic);
    assert_ne!(hits[0]["topic"], old_topic);
}

/// 9. 心跳：超时 1s + ping 200ms。僵尸客户端（握手后不再读帧，也就不会回 Pong）
///    应在超时窗口内被服务端关闭，好友看到其离线；正常读帧的客户端不受影响。
#[tokio::test]
async fn p33_ws_idle_timeout() {
    let (base, st) = spawn_named_state(&format!("chat_it_p33i_{}", uniq())).await;
    st.ws_ping_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    st.ws_idle_timeout_ms
        .store(1000, std::sync::atomic::Ordering::Relaxed);

    let a = register(&base, &format!("p33ia_{}", uniq())).await;
    let z = register(&base, &format!("p33iz_{}", uniq())).await;
    befriend(&base, &a, &z).await;
    let conv = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

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

/// 10. 推送发送失败（连接拒绝）不影响消息正常送达；test 端点失败 502 带原因。
#[tokio::test]
async fn p33_push_failure_isolated() {
    let (base, _st) = spawn_named_state(&format!("chat_it_p33j_{}", uniq())).await;
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
    assert_eq!(
        push_put(&base, &b, json!({"enabled": true})).await.status(),
        200
    );

    // 离线推送失败只记日志：消息仍正常落库与回执
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
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

    // test 端点：投递失败 → 502 带原因
    let r = push_post(&base, &b, "test").await;
    assert_eq!(r.status(), 502);
    assert!(r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("推送"));
}
