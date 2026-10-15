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
        recall_attachment_ttl_secs: 86400,
        recall_sweep_interval_secs: 3600,
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
        recall_attachment_ttl_secs: 86400,
        recall_sweep_interval_secs: 3600,
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

/// 撤回附件清扫（测试用）：把指定行拨过保留期后跑一轮 sweep，返回删除的行数。
/// 注意 sweep 会清掉全库所有过期行——共享库上并行测试若也拨过行，返回值可能 > ids.len()，
/// 故断言用 >= 并配合逐行存在性检查。
pub(crate) async fn sweep_recalled(pool: &sqlx::PgPool, files_dir: &str, ids: &[uuid::Uuid]) -> u64 {
    sqlx::query(
        "UPDATE attachments SET recalled_at = now() - interval '2 days' WHERE id = ANY($1)",
    )
    .bind(ids)
    .execute(pool)
    .await
    .unwrap();
    chat_server::domain::sweep_recalled_attachments(pool, files_dir, Dur::from_secs(86400))
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
