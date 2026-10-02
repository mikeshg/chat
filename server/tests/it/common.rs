use super::*;

pub(crate) fn base_db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:15432/postgres".into())
}

pub(crate) fn test_db_url() -> String {
    let base = base_db_url();
    // 把库名替换为 chat_test
    match base.rfind('/') {
        Some(i) => format!("{}/chat_test", &base[..i]),
        None => "postgres://chat:chat@localhost:15432/chat_test".into(),
    }
}

static DB_INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub(crate) async fn ensure_test_db() {
    DB_INIT
        .get_or_init(|| async {
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect(&base_db_url())
                .await
                .expect("connect admin db (docker compose up db)");
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = 'chat_test')",
            )
            .fetch_one(&admin)
            .await
            .unwrap();
            if !exists {
                // 并发下重复建库会报 23505，忽略即可
                let _ = sqlx::query("CREATE DATABASE chat_test")
                    .execute(&admin)
                    .await;
            }
            let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        })
        .await;
}

/// 启动一个测试实例，返回 base URL
pub(crate) async fn spawn() -> String {
    ensure_test_db().await;
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let cfg = Config {
        database_url: test_db_url(),
        jwt_secret: "test-secret".into(),
        bind: "127.0.0.1".into(),
        port: 0,
        app_dist: None,
        files_dir: std::env::temp_dir()
            .join("chat-it-files")
            .to_string_lossy()
            .into(),
        upload_max_bytes: 25 * 1024 * 1024,
        trust_proxy: false,
    };
    std::fs::create_dir_all(&cfg.files_dir).unwrap();
    let state = AppState::new(pool, cfg.clone());
    let app = build_router(state.clone(), &cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

pub(crate) fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

/// 本机回环请求必须绕开系统代理（否则被转发到 127.0.0.1:8888 之类的本机代理 → 502）
pub(crate) fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

pub(crate) struct TestUser {
    pub(crate) id: String,
    pub(crate) username: String,
    pub(crate) access: String,
    pub(crate) refresh: String,
}

pub(crate) async fn register(base: &str, name: &str) -> TestUser {
    let c = http();
    let resp = c
        .post(format!("{base}/v1/auth/register"))
        .json(&json!({"username": name, "password": "secret123", "display_name": name}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    assert_eq!(status, 200, "register failed (response body redacted)");
    let r: Value = resp.json().await.expect("invalid register response");
    TestUser {
        id: r["user"]["id"].as_str().unwrap().into(),
        username: name.into(),
        access: r["access_token"].as_str().unwrap().into(),
        refresh: r["refresh_token"].as_str().unwrap().into(),
    }
}

pub(crate) fn authz(t: &str) -> String {
    format!("Bearer {t}")
}

/// 建立 A↔B 好友关系，返回 ()
pub(crate) async fn befriend(base: &str, a: &TestUser, b: &TestUser) {
    let c = http();
    let r: Value = c
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&a.access))
        .json(&json!({"username": b.username, "message": "hi"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["status"], "pending");

    // B 收到 incoming
    let reqs: Value = c
        .get(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rid = reqs["incoming"][0]["id"].as_str().unwrap().to_string();

    let r: Value = c
        .post(format!("{base}/v1/friends/requests/{rid}/accept"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["status"], "accepted");
}

pub(crate) async fn dm_conv(base: &str, u: &TestUser) -> String {
    let c = http();
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "dm")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

pub(crate) async fn ws_url(base: &str, u: &TestUser) -> String {
    let t: Value = http()
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    format!(
        "{}/v1/ws?ticket={}",
        base.replacen("http://", "ws://", 1),
        t["ticket"].as_str().unwrap()
    )
}

/// 读帧直到 type == want（presence 等异步帧可能插队，客户端必须容忍乱序）
pub(crate) async fn next_of<S>(s: &mut S, want: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..10 {
        let m = s.next().await.unwrap().unwrap();
        if let WsMsg::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == want {
                return v;
            }
        }
    }
    panic!("did not receive frame type={want}");
}

/// 独立库：首个注册用户自动成为管理员，便于确定性测试
pub(crate) async fn spawn_named_db(db_name: &str) -> String {
    spawn_named_state(db_name).await.0
}

pub(crate) async fn spawn_named_state(db_name: &str) -> (String, chat_server::state::SharedState) {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_db_url())
        .await
        .unwrap();
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(db_name)
    .fetch_one(&admin)
    .await
    .unwrap();
    if !exists {
        let _ = sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&admin)
            .await;
    }
    let url = match base_db_url().rfind('/') {
        Some(i) => format!("{}/{db_name}", &base_db_url()[..i]),
        None => format!("postgres://chat:chat@localhost:15432/{db_name}"),
    };
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let cfg = Config {
        database_url: url,
        jwt_secret: "test-secret".into(),
        bind: "127.0.0.1".into(),
        port: 0,
        app_dist: None,
        files_dir: std::env::temp_dir()
            .join("chat-it-files-admin")
            .to_string_lossy()
            .into(),
        upload_max_bytes: 25 * 1024 * 1024,
        trust_proxy: false,
    };
    std::fs::create_dir_all(&cfg.files_dir).unwrap();
    let state = AppState::new(pool, cfg.clone());
    let app = build_router(state.clone(), &cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), state)
}

pub(crate) async fn me(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// REST 取某会话最新消息页
pub(crate) async fn history(base: &str, at: &str, conv: &str) -> Value {
    http()
        .get(format!("{base}/v1/conversations/{conv}/messages?limit=50"))
        .header("authorization", authz(at))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// WS 发一条消息并等 ack，返回服务端 MessageView
pub(crate) async fn ws_send<S>(s: &mut S, conv: &str, frame: Value) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<WsMsg, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let mut f = frame;
    f["type"] = json!("send_message");
    f["conversation_id"] = json!(conv);
    s.send(WsMsg::Text(f.to_string().into())).await.unwrap();
    next_of(s, "message_ack").await["message"].clone()
}

pub(crate) async fn conv_list(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(crate) async fn mk_group(base: &str, u: &TestUser, members: &[String]) -> String {
    let detail: Value = http()
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&u.access))
        .json(&json!({"name": "g", "member_ids": members}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    detail["id"].as_str().unwrap().to_string()
}

/// 带 device_label 的登录（同一账号多次登录 = 多台设备/多个 session）
pub(crate) async fn login_device(
    base: &str,
    username: &str,
    password: &str,
    device_label: &str,
) -> TestUser {
    let r: Value = http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": username, "password": password, "device_label": device_label}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        r["access_token"].as_str().is_some(),
        "login failed for {username}"
    );
    TestUser {
        id: r["user"]["id"].as_str().unwrap().into(),
        username: username.into(),
        access: r["access_token"].as_str().unwrap().into(),
        refresh: r["refresh_token"].as_str().unwrap().into(),
    }
}

pub(crate) async fn sessions_of(base: &str, access: &str) -> Vec<Value> {
    http()
        .get(format!("{base}/v1/users/me/sessions"))
        .header("authorization", authz(access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// 现场生成 PNG（与 server 共用 image 依赖）
pub(crate) fn png_fixture(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 253) as u8, ((x + y) % 241) as u8])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// 上传附件：返回 (status, json body)
pub(crate) async fn upload_att(
    base: &str,
    conv: &str,
    at: &str,
    name: &str,
    mime: &str,
    body: Vec<u8>,
) -> (u16, Value) {
    let part = reqwest::multipart::Part::bytes(body)
        .file_name(name.to_string())
        .mime_str(mime)
        .unwrap();
    let r = http()
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(at))
        .multipart(
            reqwest::multipart::Form::new()
                .text("conversation_id", conv.to_string())
                .part("file", part),
        )
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

/// 最小合法 PNG（1×1）。服务端只校验魔数，不解码。
pub(crate) fn png_bytes() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

pub(crate) async fn put_avatar(
    base: &str,
    token: &str,
    body: Vec<u8>,
    mime: &str,
) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(token))
        .multipart(avatar_part(body, mime))
        .send()
        .await
        .unwrap()
}

/// 本人的 self 会话列表项（kind='self'、name NULL、member_count=1、peer NULL）
pub(crate) async fn self_conv(base: &str, u: &TestUser) -> Value {
    let convs = conv_list(base, u).await;
    convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "self")
        .expect("self conversation missing")
        .clone()
}

pub(crate) async fn admin_put_settings(
    base: &str,
    admin: &TestUser,
    patch: Value,
) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&patch)
        .send()
        .await
        .unwrap()
}

/// 用账号 access token 换 WS 票的 TicketSource（宿主连接也走同一票口）
pub(crate) struct AgentTickets {
    pub(crate) base: String,
    pub(crate) access: String,
}

/// 假 ACP agent 的绝对路径（node 第一个参数）
pub(crate) fn fake_acp_args() -> Vec<String> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-host/tests/fake-acp-agent.mjs");
    assert!(p.exists(), "fake agent 脚本不存在: {p:?}");
    vec![p.to_string_lossy().to_string()]
}

/// 起一个真宿主连接，等到 host_ready。返回 (handle, host_id)。
pub(crate) async fn spawn_host(
    base: &str,
    u: &TestUser,
    install_id: uuid::Uuid,
) -> (agent_host::HostHandle, String) {
    let h = agent_host::spawn(
        agent_host::HostConfig {
            server_url: base.into(),
            install_id,
            label: "it-host".into(),
            platform: "windows".into(),
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

/// POST /v1/agents；返回 (status, body)
pub(crate) async fn create_agent(
    base: &str,
    u: &TestUser,
    host_id: &str,
    name: &str,
) -> (u16, Value) {
    create_agent_with(base, u, host_id, name, fake_acp_args()).await
}

pub(crate) async fn create_agent_with(
    base: &str,
    u: &TestUser,
    host_id: &str,
    name: &str,
    args: Vec<String>,
) -> (u16, Value) {
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .json(&json!({
            "name": name,
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
        }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

/// 收集帧直到命中 want 类型（返回含命中帧在内的全部帧）；超时 panic
pub(crate) async fn until_frame<S>(s: &mut S, want: &str, timeout: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 {want} 超时；已收到: {out:?}");
        match tokio::time::timeout(left, s.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let hit = v["type"] == want;
                    out.push(v);
                    if hit {
                        return out;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("等 {want} 时 ws 异常: {other:?}; 已收到: {out:?}"),
        }
    }
}

/// 静默收集 dur 内的全部文本帧（用于「不应出现某帧」的反向断言）
pub(crate) async fn frames_for<S>(s: &mut S, dur: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + dur;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, s.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    out.push(v);
                }
            }
            _ => break,
        }
    }
    out
}

pub(crate) async fn send_ws<S>(s: &mut S, conv: &str, cmid: &str, content: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    s.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":cmid,"content":content})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

pub(crate) async fn cancel_inv(base: &str, u: &TestUser, id: &str) -> (u16, Value) {
    let r = http()
        .post(format!("{base}/v1/agent-invocations/{id}/cancel"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

pub(crate) async fn list_inv(base: &str, u: &TestUser, conv: &str) -> Value {
    http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(crate) async fn conv_messages(base: &str, u: &TestUser, conv: &str) -> Vec<Value> {
    let page: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    page["messages"].as_array().cloned().unwrap_or_default()
}

pub(crate) fn agent_texts(msgs: &[Value]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m["agent"].is_object())
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

/// 读到满足 pred 的 agent_invocation 帧（超时 panic）
pub(crate) async fn wait_inv<S>(s: &mut S, pred: impl Fn(&Value) -> bool, timeout: A01Dur) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等调用帧超时");
        let frames = until_frame(s, "agent_invocation", left).await;
        if let Some(f) = frames.into_iter().rev().find(|f| pred(f)) {
            return f;
        }
    }
}

pub(crate) type RawHostWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 裸宿主连接：role=host + host_hello → host_ready。返回 (socket, host_id)。
pub(crate) async fn spawn_raw_host(base: &str, u: &TestUser) -> (RawHostWs, String) {
    let (mut s, _) =
        tokio_tungstenite::connect_async(format!("{}&role=host", ws_url(base, u).await))
            .await
            .unwrap();
    s.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),
               "label":"raw-host","platform":"test"})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = next_of(&mut s, "host_ready").await;
    (s, ready["host_id"].as_str().unwrap().to_string())
}

/// 宿主侧结束调用（invocation_finish ok:true）
pub(crate) async fn host_finish(sh: &mut RawHostWs, inv_id: &str) {
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv_id,"ok":true,"text":"done"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

pub(crate) async fn a11_group(base: &str, owner: &TestUser, members: &[&TestUser]) -> String {
    for m in members {
        befriend(base, owner, m).await;
    }
    let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
    let detail: Value = http()
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&owner.access))
        .json(&json!({"name": "a11", "member_ids": ids}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    detail["id"].as_str().unwrap().to_string()
}

pub(crate) async fn a11_unread(base: &str, u: &TestUser, conv: &str) -> i64 {
    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv)
        .unwrap()["unread_count"]
        .as_i64()
        .unwrap()
}

pub(crate) async fn decide_approval(
    base: &str,
    u: &TestUser,
    id: &str,
    option_id: &str,
) -> (u16, Value) {
    let r = http()
        .post(format!("{base}/v1/agent-approvals/{id}/decide"))
        .header("authorization", authz(&u.access))
        .json(&json!({"option_id": option_id}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}
