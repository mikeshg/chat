//! 集成测试：需要本地 Postgres（docker compose up db）。测试库 chat_test 自动创建。
//! 运行：cargo test --manifest-path server/Cargo.toml

use chat_server::build_router;
use chat_server::config::Config;
use chat_server::state::AppState;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;

fn base_db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:5432/postgres".into())
}

fn test_db_url() -> String {
    let base = base_db_url();
    // 把库名替换为 chat_test
    match base.rfind('/') {
        Some(i) => format!("{}/chat_test", &base[..i]),
        None => "postgres://chat:chat@localhost:5432/chat_test".into(),
    }
}

static DB_INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_test_db() {
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
async fn spawn() -> String {
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

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

/// 本机回环请求必须绕开系统代理（否则被转发到 127.0.0.1:8888 之类的本机代理 → 502）
fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

struct TestUser {
    id: String,
    username: String,
    access: String,
    refresh: String,
}

async fn register(base: &str, name: &str) -> TestUser {
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

fn authz(t: &str) -> String {
    format!("Bearer {t}")
}

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

/// 建立 A↔B 好友关系，返回 ()
async fn befriend(base: &str, a: &TestUser, b: &TestUser) {
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

async fn dm_conv(base: &str, u: &TestUser) -> String {
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

async fn ws_url(base: &str, u: &TestUser) -> String {
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
async fn next_of<S>(s: &mut S, want: &str) -> Value
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

// ---------- 管理员/账号角色 ----------

/// 独立库：首个注册用户自动成为管理员，便于确定性测试
async fn spawn_named_db(db_name: &str) -> String {
    spawn_named_state(db_name).await.0
}

async fn spawn_named_state(db_name: &str) -> (String, chat_server::state::SharedState) {
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
        None => format!("postgres://chat:chat@localhost:5432/{db_name}"),
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

async fn me(base: &str, u: &TestUser) -> Value {
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

#[tokio::test]
async fn admin_rules() {
    let base = spawn_named_db(&format!("chat_t_adm_{}", uniq())).await;
    let c = http();

    // 首个注册用户 = admin；其余为 user
    let a = register(&base, &format!("ad_{}", uniq())).await;
    let b = register(&base, &format!("bu_{}", uniq())).await;
    let c3 = register(&base, &format!("cu_{}", uniq())).await;
    assert_eq!(me(&base, &a).await["role"], "admin");
    assert_eq!(me(&base, &b).await["role"], "user");

    // 普通用户访问 admin API → 403
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // admin overview
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let st_code = r.status();
    let ov: Value = r.json().await.unwrap();
    eprintln!("overview -> {st_code} body={ov}");
    assert_eq!(st_code, 200);
    assert_eq!(ov["user_count"], 3);
    assert_eq!(ov["admin_count"], 1);

    // 任命 c3 为管理员 → 管理员账号不可禁用
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", c3.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", c3.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 禁用 b → 立即全线拒绝（REST/登录/搜索排除），数据保留
    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // ws ticket 也被拒
    let r = c
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 搜索排除
    let found: Value = c
        .get(format!("{base}/v1/users/search?q={}", b.username))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 0);

    // 启用 → 账号恢复，但旧 token 已在禁用时随 session_version 作废，必须重新登录
    let r = c
        .post(format!("{base}/v1/admin/users/{}/enable", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 撤销 a 的管理员（还有 c3）→ 成功；a 失去 admin 访问权
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", a.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 末位保护：c3 是最后一个管理员，撤销自己 → 400
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", c3.id))
        .header("authorization", authz(&c3.access))
        .json(&json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 重置密码：旧密码死、新密码活
    let r = c
        .post(format!("{base}/v1/admin/users/{}/password", b.id))
        .header("authorization", authz(&c3.access))
        .json(&json!({"password": "newpass999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "newpass999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

// ---------- review 回归 ----------

/// P1-1：附件下载安全头。HTML/SVG 一律 attachment + nosniff；图片/PDF 仍可 inline。
#[tokio::test]
async fn attachment_download_hardened() {
    let base = spawn().await;
    let a = register(&base, &format!("fa_{}", uniq())).await;
    let b = register(&base, &format!("fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    async fn upload(
        base: &str,
        conv: &str,
        at: &str,
        name: &str,
        mime: &str,
        body: &[u8],
    ) -> String {
        let part = reqwest::multipart::Part::bytes(body.to_vec())
            .file_name(name.to_string())
            .mime_str(mime)
            .unwrap();
        let form = reqwest::multipart::Form::new()
            .text("conversation_id", conv.to_string())
            .part("file", part);
        let r = http()
            .post(format!("{base}/v1/files"))
            .header("authorization", authz(at))
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    // text/html 上传 → 下载必须 attachment + nosniff（不允许同源 inline 执行）
    let id = upload(
        &base,
        &conv,
        &a.access,
        "evil.html",
        "text/html",
        b"<script>x</script>",
    )
    .await;
    let dl = c.get(format!("{base}/v1/files/{id}")).send().await.unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.headers()["x-content-type-options"], "nosniff");
    assert!(dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment"));

    // svg 带脚本 → 同样 attachment
    let id = upload(
        &base,
        &conv,
        &a.access,
        "i.svg",
        "image/svg+xml",
        b"<svg><script>x</script></svg>",
    )
    .await;
    let dl = c.get(format!("{base}/v1/files/{id}")).send().await.unwrap();
    assert!(dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment"));

    // png → inline 预览保留，nosniff 同样带上
    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let id = upload(&base, &conv, &a.access, "p.png", "image/png", png).await;
    let dl = c.get(format!("{base}/v1/files/{id}")).send().await.unwrap();
    assert!(dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("inline"));
    assert_eq!(dl.headers()["x-content-type-options"], "nosniff");

    // dl=1 时即便安全 MIME 也强制 attachment
    let dl = c
        .get(format!("{base}/v1/files/{id}?dl=1"))
        .send()
        .await
        .unwrap();
    assert!(dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment"));
}

/// P1-5：UPLOAD_MAX_MB(25MB) 与 Axum body limit 一致。26MB 走流式检查拒掉，30MB 被 body limit 拦下。
#[tokio::test]
async fn upload_body_limit_consistent() {
    let base = spawn().await;
    let a = register(&base, &format!("ua_{}", uniq())).await;
    let b = register(&base, &format!("ub_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let up = |bytes: usize| {
        let base = base.clone();
        let conv = conv.clone();
        let at = a.access.clone();
        async move {
            let part = reqwest::multipart::Part::bytes(vec![0u8; bytes])
                .file_name("big.bin".to_string())
                .mime_str("application/octet-stream")
                .unwrap();
            let form = reqwest::multipart::Form::new()
                .text("conversation_id", conv)
                .part("file", part);
            match http()
                .post(format!("{base}/v1/files"))
                .header("authorization", authz(&at))
                .multipart(form)
                .send()
                .await
            {
                Ok(r) => r.status().as_u16(),
                // 中途断连（body limit 直接切断）同样视为拒绝
                Err(_) => 0,
            }
        }
    };

    // 26MB：body limit(25MB+1MB 余量)放行，流式检查在 >25MB 处掐断，返回干净的 400
    assert_eq!(up(26 * 1024 * 1024).await, 400);
    // 30MB：超过 body limit 上限被拒（413/400/连接中止皆可，只要不落库）
    let st = up(30 * 1024 * 1024).await;
    assert!(st == 0 || st == 400 || st == 413, "unexpected status {st}");
}

/// P1-4：并发非法 refresh 不得拖垮连接池。全部 401，/users/me 仍即时响应。
#[tokio::test]
async fn refresh_flood_no_pool_starvation() {
    let base = spawn().await;
    let u = register(&base, &format!("rf_{}", uniq())).await;

    let mut hs = Vec::new();
    for i in 0..40 {
        let base = base.clone();
        hs.push(tokio::spawn(async move {
            http()
                .post(format!("{base}/v1/auth/refresh"))
                .json(&json!({ "refresh_token": format!("bad-{i}") }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for h in hs {
        assert_eq!(h.await.unwrap(), 401);
    }

    // 池没有被拖死：正常请求立刻成功
    let r = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

// Observe the lock barrier, not elapsed time, before advancing a concurrent operation.
async fn wait_for_blocker<T>(pool: &sqlx::PgPool, pid: i32, task: &tokio::task::JoinHandle<T>) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
            ).bind(pid).fetch_one(pool).await.unwrap();
            if waiting || task.is_finished() { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("operation did not reach lock barrier");
}

#[tokio::test]
async fn token_issuance_waits_for_user_authorization() {
    let (base, st) = spawn_named_state(&format!("chat_t_issue_{}", uniq())).await;
    let _admin = register(&base, &format!("ia_{}", uniq())).await;
    for refresh in [false, true] {
        let u = register(&base, &format!("iu_{}", uniq())).await;
        let uid = uuid::Uuid::parse_str(&u.id).unwrap();
        let mut reset = st.pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *reset)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE")
            .bind(uid)
            .execute(&mut *reset)
            .await
            .unwrap();
        let request = if refresh {
            http()
                .post(format!("{base}/v1/auth/refresh"))
                .json(&json!({"refresh_token":u.refresh}))
        } else {
            http()
                .post(format!("{base}/v1/auth/login"))
                .json(&json!({"username":u.username,"password":"secret123"}))
        };
        let task = tokio::spawn(async move { request.send().await.unwrap() });
        wait_for_blocker(&st.pool, pid, &task).await;
        // Commit the revocation while the issuance request waits for authorization.
        sqlx::query("UPDATE users SET password_hash = $2, session_version = session_version + 1 WHERE id = $1")
            .bind(uid).bind(chat_server::auth::hash_password("new-password").unwrap())
            .execute(&mut *reset).await.unwrap();
        sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE user_id = $1")
            .bind(uid)
            .execute(&mut *reset)
            .await
            .unwrap();
        reset.commit().await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r.status(),
            401,
            "stale login/refresh authorization must not mint tokens"
        );
    }
}

#[tokio::test]
async fn registration_is_not_visible_before_token_issuance_commits() {
    let (base, st) = spawn_named_state(&format!("chat_t_register_{}", uniq())).await;
    let admin = register(&base, &format!("rga_{}", uniq())).await;
    let name = format!("rgu_{}", uniq());
    let mut barrier = st.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE refresh_tokens IN SHARE MODE")
        .execute(&mut *barrier)
        .await
        .unwrap();
    let request = http()
        .post(format!("{base}/v1/auth/register"))
        .json(&json!({"username":name,"password":"secret123"}));
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_blocker(&st.pool, pid, &task).await;
    assert!(!task.is_finished());
    let visible: Value = http()
        .get(format!("{base}/v1/admin/users?q={name}"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        visible,
        json!([]),
        "an admin must not reset/disable a half-issued registration"
    );
    barrier.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn reset_and_disable_revoke_tokens_atomically() {
    let (base, st) = spawn_named_state(&format!("chat_t_atomic_{}", uniq())).await;
    let admin = register(&base, &format!("ata_{}", uniq())).await;
    for operation in ["password", "disable"] {
        let u = register(&base, &format!("atu_{}", uniq())).await;
        let uid = uuid::Uuid::parse_str(&u.id).unwrap();
        let mut barrier = st.pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM refresh_tokens WHERE user_id = $1 FOR UPDATE")
            .bind(uid)
            .execute(&mut *barrier)
            .await
            .unwrap();
        let request = http()
            .post(format!("{base}/v1/admin/users/{}/{operation}", u.id))
            .header("authorization", authz(&admin.access))
            .json(&json!({"password":"new-password"}));
        let task = tokio::spawn(async move { request.send().await.unwrap() });
        wait_for_blocker(&st.pool, pid, &task).await;
        assert!(
            !task.is_finished(),
            "revocation must wait for the token barrier"
        );
        let during = http()
            .get(format!("{base}/v1/users/me"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap();
        // Neither new credentials nor disabled/version state may commit before token revocation.
        assert_eq!(
            during.status(),
            200,
            "partially committed session revocation"
        );
        barrier.commit().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .status(),
            204
        );
        let r = http()
            .post(format!("{base}/v1/auth/refresh"))
            .json(&json!({"refresh_token":u.refresh}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
}

/// P1-3：管理员重置密码后，旧 access token 与 WS ticket 立即失效。
#[tokio::test]
async fn reset_password_revokes_access() {
    let base = spawn_named_db(&format!("chat_t_rp_{}", uniq())).await;
    let c = http();
    let a = register(&base, &format!("ra_{}", uniq())).await; // 首个 = admin
    let b = register(&base, &format!("rb_{}", uniq())).await;

    // 重置前 token 可用
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = c
        .post(format!("{base}/v1/admin/users/{}/password", b.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"password": "rotated999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 旧 access token → 401（session_version 已 +1）
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // WS ticket 同样失效
    let r = c
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 旧 refresh 已撤销
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 新密码可登录
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "rotated999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// P1-2：禁用用户时旧 WS 收到 force_logout 后被服务端主动关闭；旧连接无法再写。
#[tokio::test]
async fn force_logout_actually_closes_socket() {
    let base = spawn_named_db(&format!("chat_t_fk_{}", uniq())).await;
    let c = http();
    let a = register(&base, &format!("ka_{}", uniq())).await;
    let b = register(&base, &format!("kb_{}", uniq())).await;

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 先收 force_logout 帧，随后服务端主动发 Close
    let f = next_of(&mut sb, "force_logout").await;
    assert_eq!(f["reason"], "disabled");
    let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match sb.next().await {
                Some(Ok(WsMsg::Close(_))) | None | Some(Err(_)) => break true,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "server did not close the kicked socket");

    // 旧 access token 一并失效
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert!(r.status() == 401 || r.status() == 403);
}

async fn closed_without_application_frames<S>(socket: &mut S)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match socket.next().await {
                Some(Ok(WsMsg::Close(_))) | None => break,
                Some(Ok(WsMsg::Text(text))) => {
                    let f: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(
                        f["type"], "force_logout",
                        "revoked connection accepted application frames"
                    );
                }
                Some(Err(e)) => panic!("expected a clean server close: {e}"),
                _ => {}
            }
        }
    })
    .await
    .expect("revoked connection was not closed");
}

#[tokio::test]
async fn ws_reset_between_upgrade_and_registration_closes_without_presence() {
    let (base, st) = spawn_named_state(&format!("chat_t_upgrade_{}", uniq())).await;
    let admin = register(&base, &format!("uga_{}", uniq())).await;
    let u = register(&base, &format!("ugu_{}", uniq())).await;
    let grant = chat_server::auth::verify_access(&u.access, &st.cfg.jwt_secret).unwrap();
    let uid = grant.uid;
    let version = grant.session_version;
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let resume = release.clone();
    let session_state = st.clone();
    // Pause only the test's upgrade continuation; production needs no timing/debug hooks.
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(move |upgrade: axum::extract::WebSocketUpgrade| {
            let resume = resume.clone();
            let state = session_state.clone();
            async move {
                upgrade.on_upgrade(move |socket| async move {
                    resume.notified().await;
                    chat_server::ws::handle_session(
                        socket,
                        state,
                        chat_server::auth::AccessGrant {
                            uid,
                            session_version: version,
                            sid: None,
                        },
                    )
                    .await;
                })
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (mut delayed, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
        .await
        .unwrap();
    let reset = http()
        .post(format!("{base}/v1/admin/users/{}/password", u.id))
        .header("authorization", authz(&admin.access))
        .json(&json!({"password":"new-password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 204);
    assert_eq!(st.conn_count(uid), 0);
    release.notify_one();
    closed_without_application_frames(&mut delayed).await;
    assert_eq!(
        st.conn_count(uid),
        0,
        "late revoked registration leaked presence"
    );
    assert!(!st.is_online(uid));
    server.abort();
}

#[tokio::test]
async fn ws_rechecks_session_version_before_frames_and_preserves_other_connections() {
    let (base, st) = spawn_named_state(&format!("chat_t_frame_{}", uniq())).await;
    let u = register(&base, &format!("fvu_{}", uniq())).await;
    let peer = register(&base, &format!("fvp_{}", uniq())).await;
    befriend(&base, &u, &peer).await;
    let uid = uuid::Uuid::parse_str(&u.id).unwrap();
    let conv = mk_group(&base, &u, &[peer.id.clone()]).await;
    let (mut old, _) = tokio_tungstenite::connect_async(ws_url(&base, &u).await)
        .await
        .unwrap();
    next_of(&mut old, "presence_init").await;
    // Model the committed version change before the in-memory kick is delivered.
    sqlx::query("UPDATE users SET session_version = session_version + 1 WHERE id = $1")
        .bind(uid)
        .execute(&st.pool)
        .await
        .unwrap();
    let r = http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username":u.username,"password":"secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let tokens: Value = r.json().await.unwrap();
    let fresh = TestUser {
        id: u.id,
        username: u.username,
        access: tokens["access_token"].as_str().unwrap().into(),
        refresh: tokens["refresh_token"].as_str().unwrap().into(),
    };
    let (mut current, _) = tokio_tungstenite::connect_async(ws_url(&base, &fresh).await)
        .await
        .unwrap();
    next_of(&mut current, "presence_init").await;
    assert_eq!(st.conn_count(uid), 2);
    old.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    closed_without_application_frames(&mut old).await;
    // Fence the remaining connection and check that stale cleanup did not remove its epoch/bucket.
    current
        .send(WsMsg::Text(
            json!({"type":"sync","conversation_id":conv,"last_seq":0})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        next_of(&mut current, "sync_messages"),
    )
    .await
    .unwrap();
    assert_eq!(st.conn_count(uid), 1);
    assert!(st.is_online(uid));
}

/// P1-6：群主转让与退群并发——最终必须恰好剩一个群主，且仍是成员。
#[tokio::test]
async fn owner_transfer_leave_race() {
    let base = spawn().await;
    let a = register(&base, &format!("oa_{}", uniq())).await;
    let b = register(&base, &format!("ob_{}", uniq())).await;
    let cc = register(&base, &format!("oc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &cc).await;
    let c = http();
    let detail: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "race", "member_ids": [b.id, cc.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = detail["id"].as_str().unwrap().to_string();

    // 并发：a 转让群主给 b + a 退群
    let (r1, r2) = tokio::join!(
        c.post(format!("{base}/v1/conversations/{gid}/transfer"))
            .header("authorization", authz(&a.access))
            .json(&json!({"user_id": b.id}))
            .send(),
        c.delete(format!("{base}/v1/conversations/{gid}/members/me"))
            .header("authorization", authz(&a.access))
            .send(),
    );
    let transfer = r1.unwrap().status().as_u16();
    assert!(
        matches!(transfer, 204 | 403),
        "unexpected transfer status: {transfer}"
    );
    assert_eq!(r2.unwrap().status(), 204);

    // 终态不变量（不依赖竞态赢家）：恰好一个群主、群主属于剩余成员、a 已离群
    let d: Value = c
        .get(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let members = d["members"].as_array().unwrap();
    let owners: Vec<&Value> = members.iter().filter(|m| m["role"] == "owner").collect();
    assert_eq!(owners.len(), 1, "exactly one owner must remain");
    let owner_id = owners[0]["user"]["id"].as_str().unwrap();
    assert_eq!(
        d["owner_id"].as_str().unwrap(),
        owner_id,
        "conversations.owner_id must match the owner member"
    );
    assert!(members.iter().any(|m| m["user"]["id"] == b.id));
    assert!(members.iter().any(|m| m["user"]["id"] == cc.id));
    assert!(!members.iter().any(|m| m["user"]["id"] == a.id));
}

/// P1-7：解散与拉人并发——成员不得被插回已解散会话。
#[tokio::test]
async fn dissolve_invite_race() {
    let base = spawn().await;
    let a = register(&base, &format!("xa_{}", uniq())).await;
    let b = register(&base, &format!("xb_{}", uniq())).await;
    let cc = register(&base, &format!("xc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &b, &cc).await; // b 与 c 是好友，b 才能拉 c
    let c = http();
    let detail: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "race2", "member_ids": [b.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = detail["id"].as_str().unwrap().to_string();

    let (r1, r2) = tokio::join!(
        c.delete(format!("{base}/v1/conversations/{gid}"))
            .header("authorization", authz(&a.access))
            .send(),
        c.post(format!("{base}/v1/conversations/{gid}/members"))
            .header("authorization", authz(&b.access))
            .json(&json!({"user_ids": [cc.id]}))
            .send(),
    );
    assert_eq!(r1.unwrap().status(), 204);
    let invite = r2.unwrap().status().as_u16();
    assert!(
        matches!(invite, 204 | 400 | 403),
        "unexpected invite status: {invite}"
    );

    // 终态：会话已解散，且没有任何成员残留（含刚被拉进来的 c）
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let gid_uuid = uuid::Uuid::parse_str(&gid).unwrap();
    let dissolved: bool =
        sqlx::query_scalar("SELECT dissolved_at IS NOT NULL FROM conversations WHERE id = $1")
            .bind(gid_uuid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dissolved);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE conversation_id = $1")
        .bind(gid_uuid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// P1-8：禁用账号不能被任命为管理员；并发互相降权后系统仍剩可用管理员。
#[tokio::test]
async fn admin_role_safety() {
    let db_name = format!("chat_t_ar_{}", uniq());
    let base = spawn_named_db(&db_name).await;
    let c = http();
    let a = register(&base, &format!("aa_{}", uniq())).await; // admin
    let b = register(&base, &format!("bb_{}", uniq())).await;
    let cc = register(&base, &format!("cc_{}", uniq())).await;

    // 禁用 cc → 任命为 admin 必须被拒
    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", cc.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", cc.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = c
        .post(format!("{base}/v1/admin/users/{}/enable", cc.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 提升 b 为管理员，然后 a、b 并发互相降权：必须恰好一个成功
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", b.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let (r1, r2) = tokio::join!(
        c.post(format!("{base}/v1/admin/users/{}/role", a.id))
            .header("authorization", authz(&b.access))
            .json(&json!({"role": "user"}))
            .send(),
        c.post(format!("{base}/v1/admin/users/{}/role", b.id))
            .header("authorization", authz(&a.access))
            .json(&json!({"role": "user"}))
            .send(),
    );
    let codes = [r1.unwrap().status().as_u16(), r2.unwrap().status().as_u16()];
    assert!(
        codes.iter().all(|s| matches!(s, 204 | 400 | 403)),
        "unexpected demotion status: {codes:?}"
    );
    assert!(
        codes.iter().filter(|s| **s == 204).count() == 1,
        "double demote both succeeded: {codes:?}"
    );

    // 终态：恰好一名可用管理员（查该测试库，避免误数到共享库）
    let named_url = match base_db_url().rfind('/') {
        Some(i) => format!("{}/{db_name}", &base_db_url()[..i]),
        None => format!("postgres://chat:chat@localhost:5432/{db_name}"),
    };
    let pool = PgPoolOptions::new().connect(&named_url).await.unwrap();
    let admins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(admins, 1);
}

/// P2-1：中文验证消息按字符截断（>200 字 → 200 字），不能按字节 panic。
#[tokio::test]
async fn request_message_char_truncate() {
    let base = spawn().await;
    let a = register(&base, &format!("ta_{}", uniq())).await;
    let b = register(&base, &format!("tb_{}", uniq())).await;
    let c = http();

    let msg = "验证消息很长很长".repeat(30); // 210 个汉字
    let r = c
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&a.access))
        .json(&json!({"username": b.username, "message": msg}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let reqs: Value = c
        .get(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stored = reqs["incoming"][0]["message"].as_str().unwrap();
    assert_eq!(stored.chars().count(), 200);
}

/// P2-6：被拒后同方向不能再发申请；反方向申请仍然可以建立。
#[tokio::test]
async fn declined_request_blocks_same_direction() {
    let base = spawn().await;
    let a = register(&base, &format!("pa_{}", uniq())).await;
    let b = register(&base, &format!("pb_{}", uniq())).await;
    let c = http();

    let rid: String = c
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&a.access))
        .json(&json!({"username": b.username}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = c
        .post(format!("{base}/v1/friends/requests/{rid}/decline"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 同方向重申 → 409
    let r = c
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&a.access))
        .json(&json!({"username": b.username}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // 反方向（b → a）正常发起，a 接受 → 成为好友
    let r = c
        .post(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&b.access))
        .json(&json!({"username": a.username}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let reqs: Value = c
        .get(format!("{base}/v1/friends/requests"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rid2 = reqs["incoming"][0]["id"].as_str().unwrap().to_string();
    let r = c
        .post(format!("{base}/v1/friends/requests/{rid2}/accept"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// P2-5：删除好友后保留的只读 DM 不再暴露对方 last_seen_at。
#[tokio::test]
async fn unfriended_dm_hides_presence() {
    let base = spawn().await;
    let a = register(&base, &format!("ya_{}", uniq())).await;
    let b = register(&base, &format!("yb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let c = http();

    // b 先上线再下线，让 last_seen_at 有值
    {
        let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
            .await
            .unwrap();
        next_of(&mut sb, "presence_init").await;
        sb.close(None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // 好友期间：a 的会话列表里 b 的 last_seen_at 可见
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dm = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "dm")
        .unwrap();
    assert!(dm["peer"]["last_seen_at"].is_string() || dm["peer"]["last_seen_at"].is_null());

    // 删除好友 → 保留的 DM 里 last_seen_at 必须为 null
    let r = c
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dm = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "dm")
        .unwrap();
    assert!(dm["peer"]["last_seen_at"].is_null(), "last_seen leaked");
}

/// P2-3：WS sync 单页 500 条封顶，has_more=true 驱动客户端续页。
#[tokio::test]
async fn sync_paginates_beyond_500() {
    let base = spawn().await;
    let a = register(&base, &format!("sa_{}", uniq())).await;
    let b = register(&base, &format!("sb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    // 直接灌 505 条历史（绕过逐条 WS 发送的开销），保持 seq 连续
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let conv_uuid = uuid::Uuid::parse_str(&conv).unwrap();
    let a_uuid = uuid::Uuid::parse_str(&a.id).unwrap();
    sqlx::query(
        "INSERT INTO messages (conversation_id, seq, sender_id, content, kind)
         SELECT $1, g, $2, 'm' || g, 'text' FROM generate_series(1, 505) g",
    )
    .bind(conv_uuid)
    .bind(a_uuid)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET last_seq = 505 WHERE id = $1")
        .bind(conv_uuid)
        .execute(&pool)
        .await
        .unwrap();

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    sb.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let page1 = next_of(&mut sb, "sync_messages").await;
    assert_eq!(page1["messages"].as_array().unwrap().len(), 500);
    assert_eq!(page1["has_more"], true);

    // 续页：按上一页末尾 seq 继续
    sb.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":500})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let page2 = next_of(&mut sb, "sync_messages").await;
    assert_eq!(page2["messages"].as_array().unwrap().len(), 5);
    assert_eq!(page2["has_more"], false);
}

// ==================== 消息生命周期（Chat V1 / Devin A） ====================

/// REST 取某会话最新消息页
async fn history(base: &str, at: &str, conv: &str) -> Value {
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
async fn ws_send<S>(s: &mut S, conv: &str, frame: Value) -> Value
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

/// 回复：同会话校验 + 预览 + 目标删除后的墓碑预览 + 跨会话拒绝
#[tokio::test]
async fn lifecycle_reply() {
    let base = spawn().await;
    let a = register(&base, &format!("ra_{}", uniq())).await;
    let b = register(&base, &format!("rb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"t1","content":"原消息 abc"}),
    )
    .await;
    let m1_id = m1["id"].as_str().unwrap();

    // B 回复 m1：预览含 id/seq/sender/excerpt
    let r1 = ws_send(
        &mut sb,
        &conv,
        json!({"client_msg_id":"t2","content":"回复来了","reply_to_id":m1_id}),
    )
    .await;
    let pv = &r1["reply_to_preview"];
    assert_eq!(pv["message_id"], m1_id);
    assert_eq!(pv["seq"].as_i64().unwrap(), 1);
    assert_eq!(pv["sender_name"], a.username);
    assert_eq!(pv["excerpt"], "原消息 abc");
    assert_eq!(pv["deleted"], false);
    assert_eq!(r1["reply_to_id"], m1_id);

    // 跨会话回复必须拒绝：a 建群后在群里引用 DM 消息
    let g: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "g", "member_ids": [b.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = g["id"].as_str().unwrap();
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":gid,"client_msg_id":"t3",
               "content":"x","reply_to_id":m1_id})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sa, "error").await;
    assert_eq!(err["code"], "bad_message");
    assert_eq!(err["client_msg_id"], "t3");

    // 删除原消息 → 预览 deleted=true、excerpt 清空、id/seq 仍可定位
    let d = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(d.status(), 200);
    let page = history(&base, &b.access, &conv).await;
    let reply = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == r1["id"])
        .unwrap();
    assert_eq!(reply["reply_to_preview"]["deleted"], true);
    assert_eq!(reply["reply_to_preview"]["excerpt"], "");
    assert_eq!(reply["reply_to_preview"]["seq"].as_i64().unwrap(), 1);
}

/// 编辑：权限/类型/窗口/版本契约
#[tokio::test]
async fn lifecycle_edit() {
    let base = spawn().await;
    let a = register(&base, &format!("ea_{}", uniq())).await;
    let b = register(&base, &format!("eb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(&mut sa, &conv, json!({"client_msg_id":"e1","content":"v0"})).await;
    let mid = m1["id"].as_str().unwrap();
    assert_eq!(m1["edit_version"].as_i64().unwrap(), 0);
    assert_eq!(m1["seq"].as_i64().unwrap(), 1);

    // 他人编辑 → 403
    let r = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&b.access))
        .json(&json!({"content":"x","expected_version":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 版本落后 → 409
    let r = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"v1","expected_version":9}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // 正常编辑：edited_at 出现、edit_version+1、seq 不变
    let op_id = uuid::Uuid::new_v4();
    let r: Value = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"v1","expected_version":0,"client_op_id":op_id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["content"], "v1");
    assert_eq!(r["edit_version"].as_i64().unwrap(), 1);
    assert_eq!(r["seq"].as_i64().unwrap(), 1);
    assert!(r["edited_at"].is_string());

    // 同一 client_op_id 幂等回放：不二次递增版本，返回现状
    let r: Value = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"v1-again","expected_version":0,"client_op_id":op_id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["content"], "v1");
    assert_eq!(r["edit_version"].as_i64().unwrap(), 1);

    // 附件消息不可编辑：先上传再发附件消息
    let part = reqwest::multipart::Part::bytes(b"hello".to_vec())
        .file_name("f.txt".to_string())
        .mime_str("text/plain")
        .unwrap();
    let att: Value = c
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(&a.access))
        .multipart(
            reqwest::multipart::Form::new()
                .part(
                    "conversation_id",
                    reqwest::multipart::Part::text(conv.clone()),
                )
                .part("file", part),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m2 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"e2","content":"","attachment_id":att["id"]}),
    )
    .await;
    let r = c
        .patch(format!("{base}/v1/messages/{}", m2["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"x","expected_version":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 15 分钟窗口（回拨 created_at）→ 400
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    sqlx::query("UPDATE messages SET created_at = now() - interval '20 minutes' WHERE id = $1")
        .bind(uuid::Uuid::parse_str(mid).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let r = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"v2","expected_version":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    sqlx::query("UPDATE messages SET created_at = now() WHERE id = $1")
        .bind(uuid::Uuid::parse_str(mid).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    // 已删除 → 409
    c.delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let r = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"v3","expected_version":2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

/// 删除权限矩阵 + 幂等 + 内容掩蔽 + 附件失效
#[tokio::test]
async fn lifecycle_delete() {
    let base = spawn().await;
    let a = register(&base, &format!("xa_{}", uniq())).await;
    let b = register(&base, &format!("xb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    // DM：b 不能删 a 的消息
    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"d1","content":"mine"}),
    )
    .await;
    let m1_id = m1["id"].as_str().unwrap();
    let r = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 附件消息删除后文件 404（行删除即失效，不等物理清理）
    let part = reqwest::multipart::Part::bytes(b"img".to_vec())
        .file_name("p.png".to_string())
        .mime_str("image/png")
        .unwrap();
    let att: Value = c
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(&a.access))
        .multipart(
            reqwest::multipart::Form::new()
                .part(
                    "conversation_id",
                    reqwest::multipart::Part::text(conv.clone()),
                )
                .part("file", part),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let att_id = att["id"].as_str().unwrap().to_string();
    let dl = c
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    let m2 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"d2","content":"","attachment_id":att_id}),
    )
    .await;
    let m2_id = m2["id"].as_str().unwrap();
    let r = c
        .delete(format!("{base}/v1/messages/{m2_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let tomb: Value = r.json().await.unwrap();
    assert_eq!(tomb["id"], m2_id);
    assert_eq!(tomb["content"], "消息已删除");
    assert!(tomb["deleted_at"].is_string());
    assert!(tomb["edit_version"].as_i64().unwrap() >= 1);
    let dl = c
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 404);

    // 本人删除：幂等（第二次仍 200，版本不再递增）
    let d1: Value = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d2: Value = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d1["edit_version"], d2["edit_version"]);

    // 历史/搜索里正文被掩蔽
    let page = history(&base, &b.access, &conv).await;
    let t1 = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == Value::from(m1_id))
        .unwrap();
    assert_eq!(t1["content"], "消息已删除");
    assert!(t1["deleted_at"].is_string());
    assert_eq!(t1["seq"].as_i64().unwrap(), 1);
    let found: Value = c
        .get(format!(
            "{base}/v1/conversations/{conv}/messages/search?q=mine"
        ))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        found.as_array().unwrap().is_empty(),
        "deleted content leaked"
    );

    // 群聊：群主可删他人消息，普通成员不行
    let cx = register(&base, &format!("xc_{}", uniq())).await;
    befriend(&base, &a, &cx).await;
    let g: Value = c
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name": "gd", "member_ids": [b.id, cx.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = g["id"].as_str().unwrap().to_string();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;
    let gm = ws_send(&mut sb, &gid, json!({"client_msg_id":"gd1","content":"gm"})).await;
    let gm_id = gm["id"].as_str().unwrap();
    let r = c
        .delete(format!("{base}/v1/messages/{gm_id}"))
        .header("authorization", authz(&cx.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .delete(format!("{base}/v1/messages/{gm_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// reactions：幂等、个性化、上限、墓碑拒绝
#[tokio::test]
async fn lifecycle_reactions() {
    let base = spawn().await;
    let a = register(&base, &format!("xa2_{}", uniq())).await;
    let b = register(&base, &format!("xb2_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"rx1","content":"hi"}),
    )
    .await;
    let mid = m1["id"].as_str().unwrap();
    let url = |e: &str| format!("{base}/v1/messages/{mid}/reactions/{}", urlencoded(e));

    // a 添加 👍 → count 1 / reacted_by_me=true / version+1
    let r: Value = c
        .put(url("👍"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["reactions"][0]["count"].as_i64().unwrap(), 1);
    assert_eq!(r["reactions"][0]["reacted_by_me"], true);
    let v1 = r["edit_version"].as_i64().unwrap();
    assert_eq!(v1, 1);

    // 重复 add 幂等：count 不变、版本不涨
    let r: Value = c
        .put(url("👍"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["reactions"][0]["count"].as_i64().unwrap(), 1);
    assert_eq!(r["edit_version"].as_i64().unwrap(), v1);

    // b 也添加同一 emoji → count 2；b 视角 reacted_by_me=true
    let r: Value = c
        .put(url("👍"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["reactions"][0]["count"].as_i64().unwrap(), 2);
    assert_eq!(r["reactions"][0]["reacted_by_me"], true);
    assert_eq!(r["edit_version"].as_i64().unwrap(), v1 + 1);

    // a 视角历史：reaction_update 之后 reacted_by_me 仍 true；b 删除自己 → a 视角变回 1
    let r: Value = c
        .delete(url("👍"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["reactions"][0]["count"].as_i64().unwrap(), 1);
    // 删除不存在的 reaction 幂等（再删一次 → count 仍 1、版本不涨）
    let r2: Value = c
        .delete(url("👍"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r2["edit_version"], r["edit_version"]);

    // 每用户每消息最多 20 种 emoji（用 ASCII key 测边界，服务端不校验 emoji 字形）
    for i in 0..19 {
        let e = format!("e{i}");
        let r = c
            .put(url(&e))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "emoji {e} should succeed");
    }
    let r = c
        .put(url("e19"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "21st emoji kind must be rejected");

    // 墓碑不可再表态
    c.delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let r = c
        .put(url("😂"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
}

fn urlencoded(s: &str) -> String {
    s.bytes().map(|b| format!("%{b:02X}")).collect()
}

/// WS 生命周期帧：broadcast 个性化 + op_ack 与 client_msg_id 隔离
#[tokio::test]
async fn lifecycle_ws_frames() {
    let base = spawn().await;
    let a = register(&base, &format!("fa_{}", uniq())).await;
    let b = register(&base, &format!("fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"w1","content":"ws msg"}),
    )
    .await;
    let mid = m1["id"].as_str().unwrap();
    next_of(&mut sb, "message").await; // b 收到原始消息

    // b 编辑 a 的消息 → error 帧带 client_op_id、不带 client_msg_id（op/outbox 隔离）
    let bad_op = uuid::Uuid::new_v4();
    sb.send(WsMsg::Text(
        json!({"type":"edit_message","message_id":mid,"content":"x",
               "expected_version":0,"client_op_id":bad_op})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sb, "error").await;
    assert_eq!(err["code"], "forbidden");
    assert_eq!(err["client_op_id"].as_str().unwrap(), bad_op.to_string());
    assert!(err["client_msg_id"].is_null());

    // a 正常编辑 → 双端 message_updated + 发起连接 op_ack
    let op = uuid::Uuid::new_v4();
    sa.send(WsMsg::Text(
        json!({"type":"edit_message","message_id":mid,"content":"edited",
               "expected_version":0,"client_op_id":op})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let upd_a = next_of(&mut sa, "message_updated").await;
    let upd_b = next_of(&mut sb, "message_updated").await;
    assert_eq!(upd_a["message"]["content"], "edited");
    assert_eq!(upd_a["edit_version"].as_i64().unwrap(), 1);
    assert_eq!(upd_b["message"]["content"], "edited");
    let ack = next_of(&mut sa, "op_ack").await;
    assert_eq!(ack["client_op_id"].as_str().unwrap(), op.to_string());

    // reaction_upsert：双端 reaction_update，reacted_by_me 逐接收者个性化
    let op2 = uuid::Uuid::new_v4();
    sa.send(WsMsg::Text(
        json!({"type":"reaction_upsert","message_id":mid,"emoji":"👍","client_op_id":op2})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let rx_a = next_of(&mut sa, "reaction_update").await;
    let rx_b = next_of(&mut sb, "reaction_update").await;
    assert_eq!(rx_a["message"]["reactions"][0]["reacted_by_me"], true);
    assert_eq!(rx_b["message"]["reactions"][0]["reacted_by_me"], false);
    assert_eq!(
        rx_a["message"]["reactions"][0]["count"].as_i64().unwrap(),
        1
    );
    next_of(&mut sa, "op_ack").await;

    // delete_message：双端墓碑帧（完整视图，seq/id 保留、正文已掩蔽）
    let op3 = uuid::Uuid::new_v4();
    sa.send(WsMsg::Text(
        json!({"type":"delete_message","message_id":mid,"client_op_id":op3})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let del_a = next_of(&mut sa, "message_deleted").await;
    let del_b = next_of(&mut sb, "message_deleted").await;
    assert_eq!(del_a["message"]["id"], mid);
    assert_eq!(del_b["message"]["id"], mid);
    assert_eq!(del_a["message"]["content"], "消息已删除");
    assert_eq!(del_a["message"]["seq"].as_i64().unwrap(), 1);
    assert!(del_a["message"]["deleted_at"].is_string());
    assert!(del_a["edit_version"].as_i64().unwrap() >= 2);
    next_of(&mut sa, "op_ack").await;
}

/// §8 反序安全回归：每个 mutation 帧都携带完整视图——
/// reaction_update(v2) 已含编辑后的正文，即使客户端随后才收到 message_updated(v1)
/// 并按版本丢弃它，正文与 reaction 也不会丢；message_deleted(v3) 同理自带墓碑态。
#[tokio::test]
async fn lifecycle_frames_carry_full_view() {
    let base = spawn().await;
    let a = register(&base, &format!("fa2_{}", uniq())).await;
    let b = register(&base, &format!("fb2_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"fv1","content":"old"}),
    )
    .await;
    let mid = m1["id"].as_str().unwrap();
    next_of(&mut sb, "message").await;

    // edit(v1) → reaction(v2)：v2 帧必须同时含新正文与新 reaction
    c.patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"edited body","expected_version":0}))
        .send()
        .await
        .unwrap();
    c.put(format!(
        "{base}/v1/messages/{mid}/reactions/{}",
        urlencoded("👍")
    ))
    .header("authorization", authz(&b.access))
    .send()
    .await
    .unwrap();

    let upd = next_of(&mut sb, "message_updated").await;
    let rx = next_of(&mut sb, "reaction_update").await;
    assert_eq!(upd["edit_version"].as_i64().unwrap(), 1);
    // 关键断言：reaction_update(v2) 携带完整视图，含 v1 的编辑结果
    let m2 = &rx["message"];
    assert_eq!(m2["edit_version"].as_i64().unwrap(), 2);
    assert_eq!(m2["content"], "edited body");
    assert_eq!(m2["reactions"][0]["emoji"], "👍");
    assert_eq!(m2["reactions"][0]["count"].as_i64().unwrap(), 1);
    assert!(m2["deleted_at"].is_null());
    // b 视角 reacted_by_me=true（b 自己加的）
    assert_eq!(m2["reactions"][0]["reacted_by_me"], true);

    // delete(v3)：帧内完整墓碑视图（正文体掩蔽、reactions 清空、seq 保留）
    c.delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let del = next_of(&mut sb, "message_deleted").await;
    let m3 = &del["message"];
    assert_eq!(m3["edit_version"].as_i64().unwrap(), 3);
    assert_eq!(m3["id"], mid);
    assert_eq!(m3["seq"].as_i64().unwrap(), 1);
    assert_eq!(m3["content"], "消息已删除");
    assert!(m3["deleted_at"].is_string());
    assert_eq!(m3["reactions"].as_array().unwrap().len(), 0);
}

/// 重连批量刷新端点：按 id 回读、≤100、无权限会话 403
#[tokio::test]
async fn lifecycle_refresh() {
    let base = spawn().await;
    let a = register(&base, &format!("ra2_{}", uniq())).await;
    let b = register(&base, &format!("rb2_{}", uniq())).await;
    let outsider = register(&base, &format!("ro_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(&mut sa, &conv, json!({"client_msg_id":"rf1","content":"x"})).await;
    let mid = m1["id"].as_str().unwrap();

    // 编辑后刷新应回最新正文与版本
    c.patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content":"new","expected_version":0}))
        .send()
        .await
        .unwrap();
    let r: Value = c
        .post(format!("{base}/v1/conversations/{conv}/messages/refresh"))
        .header("authorization", authz(&b.access))
        .json(&json!({"ids":[mid]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = &r["messages"][0];
    assert_eq!(m["content"], "new");
    assert_eq!(m["edit_version"].as_i64().unwrap(), 1);
    assert!(m["edited_at"].is_string());

    // >100 ids → 400
    let ids: Vec<String> = (0..=100)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let r = c
        .post(format!("{base}/v1/conversations/{conv}/messages/refresh"))
        .header("authorization", authz(&b.access))
        .json(&json!({"ids":ids}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 非成员 → 403（客户端据此清空本地缓存）
    let r = c
        .post(format!("{base}/v1/conversations/{conv}/messages/refresh"))
        .header("authorization", authz(&outsider.access))
        .json(&json!({"ids":[mid]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// 只读 DM（好友解除后）：回复/编辑/删除/reaction 全部拒绝
#[tokio::test]
async fn lifecycle_readonly_dm() {
    let base = spawn().await;
    let a = register(&base, &format!("za_{}", uniq())).await;
    let b = register(&base, &format!("zb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id":"ro1","content":"keep"}),
    )
    .await;
    let mid = m1["id"].as_str().unwrap();

    // 解除好友 → DM 只读
    let r = c
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // 回复（send_message + reply_to_id）被拒
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":"ro2",
               "content":"reply","reply_to_id":mid})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sa, "error").await;
    assert_eq!(err["code"], "not_friends");
    assert_eq!(err["client_msg_id"], "ro2");

    // 编辑/删除/reaction 均 403
    for resp in [
        c.patch(format!("{base}/v1/messages/{mid}"))
            .header("authorization", authz(&a.access))
            .json(&json!({"content":"x","expected_version":0}))
            .send()
            .await
            .unwrap(),
        c.delete(format!("{base}/v1/messages/{mid}"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap(),
        c.put(format!(
            "{base}/v1/messages/{mid}/reactions/{}",
            urlencoded("👍")
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap(),
    ] {
        assert_eq!(resp.status(), 403);
    }
}

// ==================== 会话偏好（Chat V1 / Devin B，§8.3） ====================

async fn conv_list(base: &str, u: &TestUser) -> Value {
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

async fn put_pref(base: &str, u: &TestUser, conv: &str, body: Value) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/preferences"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn mk_group(base: &str, u: &TestUser, members: &[String]) -> String {
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

/// 在指定会话的列表项中取 preference 字段
fn pref_of(list: &Value, conv: &str) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv)
        .unwrap_or_else(|| panic!("conv {conv} missing"))["preference"]
        .clone()
}

/// 偏好读写：缺省行=默认值；部分更新不动其他字段；静音形状校验；置顶排序
#[tokio::test]
async fn prefs_crud_and_order() {
    let base = spawn().await;
    let a = register(&base, &format!("pa_{}", uniq())).await;
    let b = register(&base, &format!("pb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    // 默认：无偏好行 → 全默认字段
    let list = conv_list(&base, &a).await;
    let p = pref_of(&list, &dm);
    assert!(p["pinned_at"].is_null());
    assert!(p["archived_at"].is_null());
    assert!(p["muted_until"].is_null());
    assert_eq!(p["mute_forever"], false);

    // 群比 DM 新 → 初始群在前
    assert_eq!(list[0]["id"], gid);
    assert_eq!(list[1]["id"], dm);

    // 置顶 DM → DM 排前
    let r = put_pref(&base, &a, &dm, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_string());
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], dm, "pinned dm first");

    // 置顶群（pinned_at 更新）→ 群在置顶块内排前
    let r = put_pref(&base, &a, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], gid, "pinned block ordered by pinned_at desc");

    // 取消群置顶 → DM（仍置顶）在前
    let r = put_pref(&base, &a, &gid, json!({"pinned": false})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_null());
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], dm);

    // 静音：永久 / 定时互斥形状校验
    let r = put_pref(&base, &a, &dm, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert_eq!(p["mute_forever"], true);
    assert!(p["muted_until"].is_null());

    let until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let r = put_pref(
        &base,
        &a,
        &dm,
        json!({"mute": {"mode": "until", "until": until}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    // 切到定时静音会清掉永久静音
    assert_eq!(p["mute_forever"], false);
    assert!(p["muted_until"].is_string());

    // 非法静音形状全部被拒（serde 数据错 422 / 域校验 400，均属 4xx）
    for body in [
        json!({"mute": {"mode": "until"}}), // 缺 until
        json!({"mute": {"mode": "until", "until": "2000-01-01T00:00:00Z"}}), // 过去时刻
        json!({"mute": {"mode": "until", "until": "not-a-time"}}), // 非法时间
        json!({"mute": {"mode": "forever", "until": until}}), // 冗余 until
        json!({"mute": {"mode": "none", "until": until}}), // none 带 until
        json!({"mute": {"mode": "bogus"}}), // 未知 mode
    ] {
        let r = put_pref(&base, &a, &dm, body).await;
        assert!(r.status().is_client_error(), "body must be rejected: {r:?}");
    }

    // mode none 清空两个静音字段
    let r = put_pref(&base, &a, &dm, json!({"mute": {"mode": "none"}})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert_eq!(p["mute_forever"], false);
    assert!(p["muted_until"].is_null());

    // 部分更新：归档后只清 pinned，archived_at 保持
    let r = put_pref(&base, &a, &dm, json!({"archived": true})).await;
    assert_eq!(r.status(), 200);
    let r = put_pref(&base, &a, &dm, json!({"pinned": false})).await;
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_null());
    assert!(
        p["archived_at"].is_string(),
        "partial update must not clear archive"
    );

    // 幂等：同状态重复 PUT 返回一致字段（状态覆盖，非事件）
    let r1: Value = put_pref(&base, &a, &dm, json!({"pinned": true}))
        .await
        .json()
        .await
        .unwrap();
    let r2: Value = put_pref(&base, &a, &dm, json!({"pinned": true}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r1["pinned_at"].is_string(), true);
    assert_eq!(
        r2["pinned_at"], r1["pinned_at"],
        "repeated pin preserves ordering"
    );
    assert_eq!(r2["archived_at"], r1["archived_at"]);

    // 空 PUT：200，不改任何字段
    let r = put_pref(&base, &a, &dm, json!({})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_string());
    assert!(p["archived_at"].is_string());

    // GET /conversations/preferences：返回本人的偏好行
    let prefs: Value = http()
        .get(format!("{base}/v1/conversations/preferences"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = prefs
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["conversation_id"] == dm)
        .expect("pref entry for dm");
    assert!(entry["preference"]["pinned_at"].is_string());
    assert!(entry["preference"]["archived_at"].is_string());
}

#[tokio::test]
async fn preferences_advance_after_waited_conversation_lock() {
    let (base, st) = spawn_named_state(&format!("chat_t_prefclock_{}", uniq())).await;
    let a = register(&base, &format!("pca_{}", uniq())).await;
    let b = register(&base, &format!("pcb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = mk_group(&base, &a, &[b.id.clone()]).await;
    let cid = uuid::Uuid::parse_str(&conv).unwrap();
    let uid = uuid::Uuid::parse_str(&b.id).unwrap();
    for mention in [false, true] {
        let initial: Value = put_pref(&base, &b, &conv, json!({"pinned":true,"archived":true}))
            .await
            .json()
            .await
            .unwrap();
        let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
            .await
            .unwrap();
        next_of(&mut sa, "presence_init").await;
        let mut barrier = st.pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR UPDATE")
            .bind(cid)
            .execute(&mut *barrier)
            .await
            .unwrap();
        let task = if mention {
            let conv = conv.clone();
            let text = format!("@{} check this", b.username);
            tokio::spawn(async move { ws_send(&mut sa, &conv, json!({"content":text})).await })
        } else {
            let request = http()
                .put(format!("{base}/v1/conversations/{conv}/preferences"))
                .header("authorization", authz(&b.access))
                .json(&json!({"pinned":true,"archived":false}));
            tokio::spawn(async move {
                let r = request.send().await.unwrap();
                assert_eq!(r.status(), 200);
                r.json::<Value>().await.unwrap()
            })
        };
        wait_for_blocker(&st.pool, pid, &task).await;
        assert!(!task.is_finished());
        let previous: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "UPDATE conversation_preferences SET updated_at = clock_timestamp()
             WHERE conversation_id = $1 AND user_id = $2 RETURNING updated_at",
        )
        .bind(cid)
        .bind(uid)
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
        barrier.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let current = pref_of(&conv_list(&base, &b).await, &conv);
        let updated =
            chrono::DateTime::parse_from_rfc3339(current["updated_at"].as_str().unwrap()).unwrap();
        assert!(
            updated > previous,
            "a waited write must advance freshness, mention={mention}"
        );
        assert!(current["archived_at"].is_null());
        assert_eq!(current["pinned_at"], initial["pinned_at"]);
    }
}

/// 权限与同步：非成员/解散/被移出 403；preference_updated 只发本人全部连接
#[tokio::test]
async fn prefs_permissions_and_fanout() {
    let base = spawn().await;
    let a = register(&base, &format!("qa_{}", uniq())).await;
    let b = register(&base, &format!("qb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let c = http();

    // b 开两条连接（模拟多设备）
    let (mut sb1, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb1, "presence_init").await;
    let (mut sb2, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb2, "presence_init").await;
    // a 也在线，确认不会收到别人的 preference_updated
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let r = put_pref(&base, &b, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    // b 的两条连接都收到
    for s in [&mut sb1, &mut sb2] {
        let f = next_of(s, "preference_updated").await;
        assert_eq!(f["conversation_id"], gid);
        assert!(f["preference"]["pinned_at"].is_string());
    }
    // a 只能收到消息类帧，不能收到 b 的偏好帧：a 发一条消息，读到 message 为止，
    // 途中不得出现 preference_updated
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":gid,"client_msg_id":"x1","content":"hi"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    for _ in 0..8 {
        let m = sa.next().await.unwrap().unwrap();
        if let WsMsg::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_ne!(v["type"], "preference_updated", "pref leaked to a");
            if v["type"] == "message" {
                break;
            }
        }
    }

    // 非成员 → 403；且 GET 列表不暴露他人偏好
    let outsider = register(&base, &format!("qc_{}", uniq())).await;
    let r = put_pref(&base, &outsider, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 403);
    let prefs: Value = c
        .get(format!("{base}/v1/conversations/preferences"))
        .header("authorization", authz(&outsider.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(prefs
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["conversation_id"] != gid));

    // 被移出群 → 403
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = put_pref(&base, &b, &gid, json!({"pinned": false})).await;
    assert_eq!(r.status(), 403, "kicked member must not write prefs");

    // 群主解散 → 任何人写偏好 403
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = put_pref(&base, &a, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 403, "dissolved conv must not accept prefs");
}

/// 归档规则：普通消息不解除归档；@本人消息解除归档并向被提及者推 preference_updated；
/// 静音不抑制未读数/mention 标记；重连后列表仍携带偏好（目录刷新即可恢复）
#[tokio::test]
async fn prefs_archive_and_mute_semantics() {
    let base = spawn().await;
    let a = register(&base, &format!("ma_{}", uniq())).await;
    let b = register(&base, &format!("mb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    // b 归档群
    let r = put_pref(&base, &b, &gid, json!({"archived": true})).await;
    assert_eq!(r.status(), 200);
    next_of(&mut sb, "preference_updated").await;

    // 普通消息不解除归档
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n1","content":"plain"}),
    )
    .await;
    next_of(&mut sb, "message").await;
    let list = conv_list(&base, &b).await;
    assert!(
        pref_of(&list, &gid)["archived_at"].is_string(),
        "plain message must not unarchive"
    );
    // 未读数仍照常累计
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == gid)
            .unwrap()["unread_count"]
            .as_i64()
            .unwrap(),
        1
    );

    // @本人 → 解除归档，b 收到 preference_updated（archived_at=null）
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n2","content":format!("@{} ping", b.username)}),
    )
    .await;
    let f = next_of(&mut sb, "preference_updated").await;
    assert_eq!(f["conversation_id"], gid);
    assert!(f["preference"]["archived_at"].is_null());
    let list = conv_list(&base, &b).await;
    assert!(
        pref_of(&list, &gid)["archived_at"].is_null(),
        "mention must unarchive"
    );

    // 静音：未读数与 mention 标记照常更新（静音只影响应用内提示，属客户端）
    let r = put_pref(&base, &b, &gid, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);
    next_of(&mut sb, "preference_updated").await;
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n3","content":format!("@{} again", b.username)}),
    )
    .await;
    next_of(&mut sb, "message").await;
    let list = conv_list(&base, &b).await;
    let item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == gid)
        .unwrap();
    assert!(item["unread_count"].as_i64().unwrap() >= 1);
    assert_eq!(
        item["has_mention"], true,
        "mute must not suppress mention flag"
    );

    // 重连/目录刷新：偏好仍在
    drop(sb);
    let (mut sb2, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb2, "presence_init").await;
    let list = conv_list(&base, &b).await;
    assert_eq!(pref_of(&list, &gid)["mute_forever"], true);
}

// ==================== 轻量投票（Chat V1 / Devin C，§8.4） ====================

async fn post_poll(base: &str, u: &TestUser, conv: &str, body: Value) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/conversations/{conv}/polls"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn vote(base: &str, u: &TestUser, mid: &str, ids: &[String]) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/messages/{mid}/poll/votes"))
        .header("authorization", authz(&u.access))
        .json(&json!({"option_ids": ids}))
        .send()
        .await
        .unwrap()
}

async fn close_poll(base: &str, u: &TestUser, mid: &str) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/messages/{mid}/poll/close"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

fn opt_ids(m: &Value) -> Vec<String> {
    m["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

fn opt_by_label<'a>(m: &'a Value, label: &str) -> &'a Value {
    m["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["label"] == label)
        .unwrap()
}

/// 创建：仅群聊成员、形状校验、kind/content 占位、client_msg_id 幂等、不可伪造
#[tokio::test]
async fn poll_create_validate() {
    let base = spawn().await;
    let a = register(&base, &format!("qa_{}", uniq())).await;
    let b = register(&base, &format!("qb_{}", uniq())).await;
    let out = register(&base, &format!("qx_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    // 正常创建：kind='poll'，content='[投票] question'，seq 照常分配
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":" 午饭去哪 ","options":[" A ","B","C"],"client_msg_id":"pv1",
               "expires_at":"2999-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(m["kind"], "poll");
    assert_eq!(m["content"], "[投票] 午饭去哪"); // 问题 trim 后入占位文本
    assert_eq!(m["seq"].as_i64().unwrap(), 1);
    let p = &m["poll"];
    assert_eq!(p["question"], "午饭去哪");
    assert_eq!(p["allow_multiple"], false);
    assert_eq!(p["anonymous"], false);
    assert_eq!(p["expired"], false);
    assert_eq!(p["version"].as_i64().unwrap(), 0);
    assert_eq!(p["total_voters"].as_i64().unwrap(), 0);
    assert_eq!(p["options"].as_array().unwrap().len(), 3);
    assert_eq!(p["options"][0]["index"].as_i64().unwrap(), 0);
    assert_eq!(p["options"][0]["label"], "A"); // 选项 trim 落库
    assert!(p["expires_at"].is_string());
    assert_eq!(p["my_option_ids"].as_array().unwrap().len(), 0);

    // client_msg_id 幂等：同键重试返回同一条消息，不重复落库
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"午饭去哪","options":["A","B","C"],"client_msg_id":"pv1"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m2: Value = r.json().await.unwrap();
    assert_eq!(m2["id"], m["id"]);
    let page = history(&base, &a.access, &gid).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert!(page["messages"][0]["poll"].is_object()); // 历史读模型含 poll

    // DM 拒绝（poll 仅群聊）
    let dm = dm_conv(&base, &a).await;
    let r = post_poll(
        &base,
        &a,
        &dm,
        json!({"question":"q","options":["x","y"],"client_msg_id":"pv-dm"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 非成员拒绝
    let r = post_poll(
        &base,
        &out,
        &gid,
        json!({"question":"q","options":["x","y"],"client_msg_id":"pv-out"}),
    )
    .await;
    assert_eq!(r.status(), 403);

    // 形状校验：问题空/超长、选项数、选项空/超长/去重后不足
    for (i, body) in [
        json!({"question":"","options":["x","y"]}),
        json!({"question":"q","options":["x"]}),
        json!({"question":"q","options":["x","x"]}), // 去重后不足 2
        json!({"question":"q","options":["x","  "]}), // trim 后空
        json!({"question":"q","options":vec!["x"; 11]}),
        json!({"question":"q","options":["x","y"],"expires_at":"2000-01-01T00:00:00Z"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut b = body;
        b["client_msg_id"] = json!(format!("bad{i}"));
        let r = post_poll(&base, &a, &gid, b).await;
        assert_eq!(r.status(), 400, "case {i} should be 400");
    }
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"x".repeat(201),"options":["a","b"],"client_msg_id":"badq"}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":["a","x".repeat(101)],"client_msg_id":"bado"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // send_message 不能伪造 kind='poll'（kind 由服务端推导，poll 负载不经 WS 进入）
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let forged = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"forge","content":"[投票] fake","kind":"poll"}),
    )
    .await;
    assert_eq!(forged["kind"], "text");
    assert!(forged["poll"].is_null());
}

/// 平面字符边界：按 Unicode 字符数（非字节）校验——𠀀 为 BMP 外 4 字节字符，
/// 问题 200/201、选项 100/101、消息 4000/4001 恰好跨线各一对
#[tokio::test]
async fn boundary_char_counts() {
    let base = spawn().await;
    let a = register(&base, &format!("ba_{}", uniq())).await;
    let b = register(&base, &format!("bb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;
    let wide = "𠀀";

    // 投票问题：200 字符通过、201 拒绝（𠀀 UTF-8 为 4 字节，按字节算会误判）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":wide.repeat(200),"options":["x","y"],"client_msg_id":"bnd-q200"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(m["poll"]["question"].as_str().unwrap().chars().count(), 200);
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":wide.repeat(201),"options":["x","y"],"client_msg_id":"bnd-q201"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 投票选项：100 字符通过、101 拒绝
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":[wide.repeat(100),"y"],"client_msg_id":"bnd-o100"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(
        m["poll"]["options"][0]["label"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        100
    );
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":[wide.repeat(101),"y"],"client_msg_id":"bnd-o101"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 消息正文：4000 字符通过、4001 拒绝（WS 发送路径；拒绝走 error 帧）
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"bnd-m4000","content":wide.repeat(4000)}),
    )
    .await;
    assert_eq!(m["content"].as_str().unwrap().chars().count(), 4000);
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":dm,
               "client_msg_id":"bnd-m4001","content":wide.repeat(4001)})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sa, "error").await;
    assert_eq!(err["code"], "bad_message");
}

/// 投票：替换语义、相同集合幂等、单/多选约束、跨 poll 选项归属、撤票、seq 不变
#[tokio::test]
async fn poll_vote_replace() {
    let base = spawn().await;
    let a = register(&base, &format!("va_{}", uniq())).await;
    let b = register(&base, &format!("vb_{}", uniq())).await;
    let c = register(&base, &format!("vc_{}", uniq())).await;
    let out = register(&base, &format!("vx_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &c).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), c.id.clone()]).await;

    // 多选投票
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"团建","options":["爬山","桌游","烧烤"],"allow_multiple":true,
               "client_msg_id":"v1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);
    let seq = m["seq"].as_i64().unwrap();

    // b 投 [A] → count/total/my/version 全部正确
    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);
    assert_eq!(opt_by_label(&v, "爬山")["count"].as_i64().unwrap(), 1);
    assert_eq!(v["poll"]["my_option_ids"].as_array().unwrap()[0], ids[0]);
    assert_eq!(v["seq"].as_i64().unwrap(), seq); // 消息 seq 不动

    // 相同集合幂等：不升版本
    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);

    // 替换 [A]→[A,B]：多选累加本人集合
    let r = vote(&base, &b, mid, &[ids[0].clone(), ids[1].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 2);
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);
    assert_eq!(opt_by_label(&v, "桌游")["count"].as_i64().unwrap(), 1);

    // 替换 [A,B]→[C]：旧选项计数回落（替换而非累加）
    let r = vote(&base, &b, mid, &[ids[2].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(opt_by_label(&v, "爬山")["count"].as_i64().unwrap(), 0);
    assert_eq!(opt_by_label(&v, "桌游")["count"].as_i64().unwrap(), 0);
    assert_eq!(opt_by_label(&v, "烧烤")["count"].as_i64().unwrap(), 1);

    // 撤票：空集合
    let r = vote(&base, &b, mid, &[]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 0);
    assert_eq!(v["poll"]["my_option_ids"].as_array().unwrap().len(), 0);

    // c 全选 3 项（多选上限=选项数）
    let r = vote(&base, &c, mid, &ids).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);

    // 非成员投票 → 403
    let r = vote(&base, &out, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 403);

    // 他 poll 的选项 id → 400（应用层校验 + 复合外键兜底）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"p2","options":["x","y"],"client_msg_id":"v2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let foreign = opt_ids(&m2)[0].clone();
    let r = vote(&base, &b, mid, &[foreign]).await;
    assert_eq!(r.status(), 400);
    let r = vote(&base, &b, mid, &[uuid::Uuid::new_v4().to_string()]).await;
    assert_eq!(r.status(), 400);

    // 单选约束
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"单选","options":["是","否"],"client_msg_id":"v3"}),
    )
    .await;
    let sm: Value = r.json().await.unwrap();
    let smid = sm["id"].as_str().unwrap();
    let sids = opt_ids(&sm);
    let r = vote(&base, &b, smid, &sids).await;
    assert_eq!(r.status(), 400); // 单选至多 1 项
    let r = vote(&base, &b, smid, &[sids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    // 单选换票：直接替换
    let r = vote(&base, &b, smid, &[sids[1].clone()]).await;
    let v2: Value = r.json().await.unwrap();
    assert_eq!(v2["poll"]["my_option_ids"].as_array().unwrap()[0], sids[1]);
    assert_eq!(
        v2["poll"]["version"].as_i64().unwrap(),
        v["poll"]["version"].as_i64().unwrap() + 1
    );

    // 非 poll 消息投票 → 404
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let t = ws_send(&mut sa, &gid, json!({"client_msg_id":"t","content":"hi"})).await;
    let tid = t["id"].as_str().unwrap();
    let r = vote(&base, &b, tid, &[]).await;
    assert_eq!(r.status(), 404);

    // 会话 last_seq 不被投票推进（不产生新消息）；未读不受 mutation 影响
    let list = conv_list(&base, &c).await;
    let item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == gid)
        .unwrap();
    assert_eq!(item["last_seq"].as_i64().unwrap(), 4); // 3 poll + 1 text
}

/// 关闭：创建者/群主权限、幂等、关后拒投；投票行与消息版本同步
#[tokio::test]
async fn poll_close_perm() {
    let base = spawn().await;
    let a = register(&base, &format!("ca_{}", uniq())).await;
    let b = register(&base, &format!("cb_{}", uniq())).await;
    let c = register(&base, &format!("cc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &c).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), c.id.clone()]).await;

    // b（普通成员）发起；c（非创建者非群主）关闭 → 403
    let r = post_poll(
        &base,
        &b,
        &gid,
        json!({"question":"q1","options":["x","y"],"client_msg_id":"c1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let r = close_poll(&base, &c, mid).await;
    assert_eq!(r.status(), 403);

    // 创建者 b 关闭 → 200，closed_at 落定、版本 +1
    let r = close_poll(&base, &b, mid).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["poll"]["closed_at"].is_string());
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);

    // 重复关闭幂等：版本不再升
    let r = close_poll(&base, &b, mid).await;
    assert_eq!(r.status(), 200);
    let v2: Value = r.json().await.unwrap();
    assert_eq!(v2["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v2["edit_version"].as_i64().unwrap(), 1);

    // 关闭后投票 → 409，结果仍可读
    let ids = opt_ids(&m);
    let r = vote(&base, &a, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &a.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert!(pm["poll"]["closed_at"].is_string());

    // 群主（非创建者）也可关闭
    let r = post_poll(
        &base,
        &b,
        &gid,
        json!({"question":"q2","options":["x","y"],"client_msg_id":"c2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let r = close_poll(&base, &a, m2["id"].as_str().unwrap()).await;
    assert_eq!(r.status(), 200);

    // 非 poll 消息关闭 → 404
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let t = ws_send(&mut sa, &gid, json!({"client_msg_id":"t2","content":"hi"})).await;
    let r = close_poll(&base, &a, t["id"].as_str().unwrap()).await;
    assert_eq!(r.status(), 404);
}

/// 过期按 DB now() 判定；软删除墓碑掩去 poll 且拒绝后续投票/关闭
#[tokio::test]
async fn poll_expire_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("xa_{}", uniq())).await;
    let b = register(&base, &format!("xb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"限时","options":["x","y"],"client_msg_id":"e1",
               "expires_at":"2999-01-01T00:00:00Z"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);

    // 回拨 expires_at 到过去 → DB now() 判过期
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    sqlx::query("UPDATE polls SET expires_at = now() - interval '1 second' WHERE message_id = $1")
        .bind(uuid::Uuid::parse_str(mid).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["expired"], true);
    // 过期后创建者仍可关闭（展示终态），关闭后依旧拒投
    let r = close_poll(&base, &a, mid).await;
    assert_eq!(r.status(), 200);

    // 软删除：墓碑掩去 poll；投票/关闭一律拒绝
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"tomb","options":["x","y"],"client_msg_id":"e2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let mid2 = m2["id"].as_str().unwrap();
    let ids2 = opt_ids(&m2);
    let r = http()
        .delete(format!("{base}/v1/messages/{mid2}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = vote(&base, &b, mid2, &[ids2[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let r = close_poll(&base, &a, mid2).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &a.access, &gid).await;
    let tm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid2)
        .unwrap();
    assert_eq!(tm["content"], "消息已删除");
    assert!(tm["poll"].is_null()); // 墓碑掩去 poll（与正文/附件同口径）
}

/// 匿名隐私：voters 永不出库（REST/WS/广播全路径掩蔽）；my_option_ids 逐人个性化；
/// WS 同步复用 message / message_updated 帧
#[tokio::test]
async fn poll_anonymous_and_ws() {
    let base = spawn().await;
    let a = register(&base, &format!("aa_{}", uniq())).await;
    let b = register(&base, &format!("ab_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    // REST 创建匿名投票 → 双端收 message 帧（kind='poll'，视图含 poll）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"匿","options":["x","y"],"anonymous":true,"client_msg_id":"an1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap().to_string();
    let ids = opt_ids(&m);
    let fa = next_of(&mut sa, "message").await;
    let fb = next_of(&mut sb, "message").await;
    assert_eq!(fa["message"]["kind"], "poll");
    assert_eq!(fb["message"]["poll"]["anonymous"], true);

    // b 投票 → 双端 message_updated：匿名掩蔽 voters，my_option_ids 个性化
    let r = vote(&base, &b, &mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let resp: Value = r.json().await.unwrap();
    for o in resp["poll"]["options"].as_array().unwrap() {
        assert_eq!(o["voters"].as_array().unwrap().len(), 0, "anonymous voters");
    }
    assert_eq!(resp["poll"]["my_option_ids"].as_array().unwrap()[0], ids[0]);
    assert_eq!(resp["poll"]["total_voters"].as_i64().unwrap(), 1);

    let ua = next_of(&mut sa, "message_updated").await;
    let ub = next_of(&mut sb, "message_updated").await;
    // a（非投票者）视角：无 voter 泄漏、my 为空
    assert_eq!(
        ua["message"]["poll"]["options"][0]["voters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ua["message"]["poll"]["my_option_ids"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ua["message"]["poll"]["options"][0]["count"]
            .as_i64()
            .unwrap(),
        1
    );
    // b 视角：本人选择可见，voters 仍掩蔽
    assert_eq!(
        ub["message"]["poll"]["my_option_ids"].as_array().unwrap()[0],
        ids[0]
    );
    assert_eq!(
        ub["message"]["poll"]["options"][0]["voters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // 非匿名投票：voters 按读模型可见
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"公开","options":["x","y"],"client_msg_id":"an2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let mid2 = m2["id"].as_str().unwrap().to_string();
    let ids2 = opt_ids(&m2);
    next_of(&mut sa, "message").await;
    next_of(&mut sb, "message").await;
    let r = vote(&base, &b, &mid2, &[ids2[0].clone()]).await;
    let v: Value = r.json().await.unwrap();
    let voters = opt_by_label(&v, "x")["voters"].as_array().unwrap();
    assert_eq!(voters[0], b.id);
    let ua2 = next_of(&mut sa, "message_updated").await;
    assert_eq!(ua2["message"]["poll"]["options"][0]["voters"][0], b.id);

    // 版本合并契约：message_updated 携带完整视图（poll + content + reactions 同帧）
    assert_eq!(ua2["message"]["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(ua2["message"]["content"], "[投票] 公开");
}

#[tokio::test]
async fn poll_creation_replay_is_private_over_rest_and_ws() {
    let base = spawn().await;
    let a = register(&base, &format!("pra_{}", uniq())).await;
    let b = register(&base, &format!("prb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = mk_group(&base, &a, &[b.id.clone()]).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    next_of(&mut sb, "presence_init").await;
    let body = json!({"question":"private choices","options":["x","y"],
        "anonymous":true,"client_msg_id":"private-replay"});
    let r = post_poll(&base, &a, &conv, body.clone()).await;
    assert_eq!(r.status(), 200);
    let original: Value = r.json().await.unwrap();
    let mid = original["id"].as_str().unwrap();
    let ids = opt_ids(&original);
    assert_eq!(next_of(&mut sa, "message").await["message"]["id"], mid);
    assert_eq!(next_of(&mut sb, "message").await["message"]["id"], mid);
    for (user, choice) in [(&a, &ids[0]), (&b, &ids[1])] {
        assert_eq!(
            vote(&base, user, mid, &[choice.clone()]).await.status(),
            200
        );
        next_of(&mut sa, "message_updated").await;
        next_of(&mut sb, "message_updated").await;
    }
    let r = http()
        .put(format!(
            "{base}/v1/messages/{mid}/reactions/{}",
            urlencoded("\u{1f44d}")
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let a_view = next_of(&mut sa, "reaction_update").await["message"].clone();
    let b_view = next_of(&mut sb, "reaction_update").await["message"].clone();
    assert_eq!(a_view["poll"]["my_option_ids"], json!([ids[0]]));
    assert_eq!(b_view["poll"]["my_option_ids"], json!([ids[1]]));
    assert_eq!(a_view["reactions"][0]["reacted_by_me"], true);
    assert_eq!(b_view["reactions"][0]["reacted_by_me"], false);

    for websocket in [false, true] {
        let replay = if websocket {
            ws_send(
                &mut sa,
                &conv,
                json!({"client_msg_id":"private-replay","content":"retry"}),
            )
            .await
        } else {
            let r = post_poll(&base, &a, &conv, body.clone()).await;
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        };
        assert_eq!(replay["id"], original["id"]);
        assert_eq!(replay["seq"], original["seq"]);
        assert_eq!(replay, a_view);
        // Sync is a queue fence: any replay broadcast would precede this response.
        sb.send(WsMsg::Text(
            json!({"type":"sync","conversation_id":conv,"last_seq":0})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let sync = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let WsMsg::Text(t) = sb.next().await.unwrap().unwrap() {
                    let f: Value = serde_json::from_str(&t).unwrap();
                    assert_ne!(
                        f["type"], "message",
                        "replay must not broadcast a creation view"
                    );
                    assert_ne!(f["type"], "preference_updated", "replay must not re-notify");
                    if f["type"] == "sync_messages" {
                        break f;
                    }
                }
            }
        })
        .await
        .expect("sync fence timed out");
        assert_eq!(sync["messages"], json!([b_view]));
        for option in sync["messages"][0]["poll"]["options"].as_array().unwrap() {
            assert_eq!(option["voters"], json!([]));
        }
    }
}

/// 并发替换：消息行锁串行化，后提交者覆盖先提交者，终态收敛且版本按真实变更计数
#[tokio::test]
async fn poll_concurrent_votes() {
    let base = spawn().await;
    let a = register(&base, &format!("za_{}", uniq())).await;
    let b = register(&base, &format!("zb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"cc","options":["x","y","z"],"client_msg_id":"cc1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);

    // 同一用户并发两个不同选择 → 串行化，两次都是真实变更
    let pick0 = [ids[0].clone()];
    let pick1 = [ids[1].clone()];
    let (r1, r2) = tokio::join!(vote(&base, &b, mid, &pick0), vote(&base, &b, mid, &pick1),);
    assert_eq!(r1.status(), 200);
    assert_eq!(r2.status(), 200);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["version"].as_i64().unwrap(), 2);
    assert_eq!(pm["poll"]["total_voters"].as_i64().unwrap(), 1);
    let counts: i64 = pm["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["count"].as_i64().unwrap())
        .sum();
    assert_eq!(counts, 1, "最终只剩一个选项被投（替换语义收敛）");
    assert_eq!(pm["poll"]["my_option_ids"].as_array().unwrap().len(), 1);

    // 并发相同选择 → 只有一次真实变更
    let pick2 = [ids[2].clone()];
    let (r3, r4) = tokio::join!(vote(&base, &b, mid, &pick2), vote(&base, &b, mid, &pick2),);
    assert_eq!(r3.status(), 200);
    assert_eq!(r4.status(), 200);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["version"].as_i64().unwrap(), 3); // 2 + 1 次真实变更
    assert_eq!(opt_by_label(pm, "z")["count"].as_i64().unwrap(), 1);
}

// ===== V2 P1-5：自助改密码 + 登录设备管理 =====

/// 带 device_label 的登录（同一账号多次登录 = 多台设备/多个 session）
async fn login_device(base: &str, username: &str, password: &str, device_label: &str) -> TestUser {
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

async fn sessions_of(base: &str, access: &str) -> Vec<Value> {
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

async fn rest_status(base: &str, method: &str, path: &str, access: &str) -> u16 {
    let c = http();
    let req = match method {
        "GET" => c.get(format!("{base}/v1{path}")),
        "POST" => c.post(format!("{base}/v1{path}")),
        "DELETE" => c.delete(format!("{base}/v1{path}")),
        _ => unreachable!(),
    };
    req.header("authorization", authz(access))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn login_status(base: &str, username: &str, password: &str) -> u16 {
    http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 读帧直到见到 want；中途收到 force_logout 直接判失败（未被踢的连接不该收到踢人帧）
async fn next_of_no_kick<S>(s: &mut S, want: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..10 {
        if let WsMsg::Text(t) = s.next().await.unwrap().unwrap() {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_ne!(v["type"], "force_logout", "unexpected force_logout: {v}");
            if v["type"] == want {
                return v;
            }
        }
    }
    panic!("did not receive frame type={want}");
}

/// 设备列表能看到多次登录，current 只标发起请求的 session，online 反映 WS 连接
#[tokio::test]
async fn sessions_list_marks_current_and_online() {
    let base = spawn().await;
    let u = register(&base, &format!("sa_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "设备甲").await;
    let _dev_b = login_device(&base, &u.username, "secret123", "设备乙").await;

    // 注册 + 两次登录 = 3 个 session
    let list = sessions_of(&base, &dev_a.access).await;
    assert_eq!(list.len(), 3);
    let a = list.iter().find(|s| s["device_label"] == "设备甲").unwrap();
    let b = list.iter().find(|s| s["device_label"] == "设备乙").unwrap();
    assert_eq!(a["current"], true);
    assert_eq!(b["current"], false);
    assert_eq!(a["online"], false);
    assert_eq!(b["online"], false);
    assert!(list.iter().all(|s| s["user_agent"].is_string()));

    // A 建立 WS 后仅 A 的 session 在线
    let (mut wa, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_a).await)
        .await
        .unwrap();
    next_of(&mut wa, "presence_init").await;
    let list = sessions_of(&base, &dev_a.access).await;
    let a = list.iter().find(|s| s["device_label"] == "设备甲").unwrap();
    let b = list.iter().find(|s| s["device_label"] == "设备乙").unwrap();
    assert_eq!(a["online"], true);
    assert_eq!(b["online"], false);
}

/// refresh 轮换沿用同一 session，且推进 last_used_at
#[tokio::test]
async fn refresh_rotates_token_within_same_session() {
    let base = spawn().await;
    let u = register(&base, &format!("sb_{}", uniq())).await;
    let dev = login_device(&base, &u.username, "secret123", "本机").await;
    let before = sessions_of(&base, &dev.access).await;
    let s0 = before.iter().find(|s| s["device_label"] == "本机").unwrap();
    let sid = s0["id"].as_str().unwrap().to_string();
    let used0 = s0["last_used_at"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    let new_access = res["access_token"].as_str().unwrap();

    let after = sessions_of(&base, new_access).await;
    let s1 = after
        .iter()
        .find(|s| s["id"] == sid)
        .expect("session id changed");
    assert!(
        s1["last_used_at"].as_str().unwrap() > used0.as_str(),
        "last_used_at 应前进"
    );
    // refresh 不产生新 session
    assert_eq!(after.len(), before.len());
}

/// A 移出设备 B：B 的 WS 收 force_logout(session_revoked) 并被关闭、凭据全失效；
/// A 的连接不受影响；U 仍在线 → 好友收不到离线广播
#[tokio::test]
async fn revoke_session_kicks_only_that_device() {
    let base = spawn().await;
    let u = register(&base, &format!("sc_{}", uniq())).await;
    let f = register(&base, &format!("sf_{}", uniq())).await;
    befriend(&base, &u, &f).await;
    let conv = dm_conv(&base, &u).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let (mut wa, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_a).await)
        .await
        .unwrap();
    let (mut wb, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_b).await)
        .await
        .unwrap();
    let (mut wf, _) = tokio_tungstenite::connect_async(ws_url(&base, &f).await)
        .await
        .unwrap();
    next_of(&mut wa, "presence_init").await;
    next_of(&mut wb, "presence_init").await;
    next_of(&mut wf, "presence_init").await;

    // A 移出 B 的 session
    let list = sessions_of(&base, &dev_a.access).await;
    let b_sid = list.iter().find(|s| s["device_label"] == "B").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );

    // B：先收 force_logout(session_revoked)，随后服务端关闭 socket
    let kicked = next_of(&mut wb, "force_logout").await;
    assert_eq!(kicked["reason"], "session_revoked");
    closed_without_application_frames(&mut wb).await;

    // B 的 access / refresh 全部失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // A 的连接仍可受理帧，且全程没收到 force_logout
    wa.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    next_of_no_kick(&mut wa, "sync_messages").await;

    // U 仍有 A 在线 → F（好友观察者）不得收到 U 的离线广播
    let stray = tokio::time::timeout(std::time::Duration::from_millis(800), wf.next()).await;
    if let Ok(Some(Ok(WsMsg::Text(t)))) = stray {
        let v: Value = serde_json::from_str(&t).unwrap();
        assert!(
            !(v["type"] == "presence" && v["user_id"] == u.id && v["online"] == false),
            "仍在在线的用户不该被广播离线: {v}"
        );
    }
}

/// 撤销当前 session → 400；撤销他人/不存在的 session → 404；重复撤销幂等 204
#[tokio::test]
async fn revoke_session_guards_and_idempotent() {
    let base = spawn().await;
    let u = register(&base, &format!("sd_{}", uniq())).await;
    let other = register(&base, &format!("se_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let _dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let list = sessions_of(&base, &dev_a.access).await;
    let cur = list.iter().find(|s| s["current"] == true).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_sid = list.iter().find(|s| s["device_label"] == "B").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 当前设备 → 400
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{cur}"),
            &dev_a.access
        )
        .await,
        400
    );
    // 别人的 session → 404；不存在的 id → 404
    let other_sid = sessions_of(&base, &other.access).await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{other_sid}"),
            &dev_a.access
        )
        .await,
        404
    );
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{}", uuid::Uuid::new_v4()),
            &dev_a.access
        )
        .await,
        404
    );
    // 正常撤销 → 204；再撤一次仍 204（幂等）
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );
}

/// 自助改密：旧密码错 403；成功后签发新凭据、当前 session 不变；
/// 其他 session 的 token/refresh/WS 全失效；新密码可登录旧密码不能
#[tokio::test]
async fn change_password_kicks_other_sessions() {
    let base = spawn().await;
    let u = register(&base, &format!("sg_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;
    let (mut wb, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_b).await)
        .await
        .unwrap();
    next_of(&mut wb, "presence_init").await;
    let c = http();
    let sid_before = sessions_of(&base, &dev_a.access)
        .await
        .iter()
        .find(|s| s["current"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 旧密码错误 → 403（不能是 401，否则客户端会走刷新流程）
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"nope-nope","new_password":"newpass456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 新密码长度不合规 → 400（与注册一致 6-72）
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 成功 → 返回为当前 session 新签发的凭据
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"newpass456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    let new_access = res["access_token"].as_str().unwrap().to_string();
    let new_refresh = res["refresh_token"].as_str().unwrap().to_string();

    // 新凭据可用；session 列表只剩当前一条且 id 不变
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &new_access).await,
        200
    );
    let list = sessions_of(&base, &new_access).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"].as_str().unwrap(), sid_before);
    assert_eq!(list[0]["current"], true);

    // A 的旧 access → 401（session_version 已 +1）；B 的 access/refresh → 401
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_a.access).await,
        401
    );
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 同一 session 的新 refresh 仍有效（改密不撤销本设备的登录）
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": new_refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // B 的 WS：force_logout(password_changed) → 服务端关闭
    let kicked = next_of(&mut wb, "force_logout").await;
    assert_eq!(kicked["reason"], "password_changed");
    closed_without_application_frames(&mut wb).await;

    // 新密码可登录，旧密码不能
    assert_eq!(login_status(&base, &u.username, "secret123").await, 401);
    assert_eq!(login_status(&base, &u.username, "newpass456").await, 200);
}

/// 一键退出其他所有设备：只剩当前 session
#[tokio::test]
async fn revoke_other_sessions_keeps_current() {
    let base = spawn().await;
    let u = register(&base, &format!("sh_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;
    let dev_c = login_device(&base, &u.username, "secret123", "C").await;

    assert_eq!(
        rest_status(
            &base,
            "POST",
            "/users/me/sessions/revoke-others",
            &dev_a.access
        )
        .await,
        204
    );
    let list = sessions_of(&base, &dev_a.access).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["device_label"], "A");
    assert_eq!(list[0]["current"], true);

    // 其他设备的凭据全部失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_c.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 幂等：再调一次仍 204
    assert_eq!(
        rest_status(
            &base,
            "POST",
            "/users/me/sessions/revoke-others",
            &dev_a.access
        )
        .await,
        204
    );
}

/// 老 access token（无 sid claim）仍可通过 require_user：session_version 对就行
#[tokio::test]
async fn legacy_access_token_without_sid_still_accepted() {
    let base = spawn().await;
    let u = register(&base, &format!("si_{}", uniq())).await;
    let uid = uuid::Uuid::parse_str(&u.id).unwrap();
    // 模拟升级前签发的 token：session_version=0、不带 sid
    let (at, _) = chat_server::auth::mint_access(uid, 0, None, "test-secret").unwrap();
    assert_eq!(rest_status(&base, "GET", "/users/me", &at).await, 200);
    assert_eq!(rest_status(&base, "GET", "/ws/ticket", &at).await, 200);
}

/// 登出撤销所属 session：设备列表里消失、该设备的 access/refresh 全失效
#[tokio::test]
async fn logout_revokes_whole_session() {
    let base = spawn().await;
    let u = register(&base, &format!("sj_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let r = http()
        .post(format!("{base}/v1/auth/logout"))
        .json(&json!({"refresh_token": dev_a.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // B 视角：A 的 session 从列表消失（剩注册时的 session + B）
    let list = sessions_of(&base, &dev_b.access).await;
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s["device_label"] != "A"));

    // A 的凭据随 session 一起失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_a.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_a.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

// ===== V2 P1-2 =====
// 注册开关 + 邀请码。全部用 spawn_named_db 独立库：改的是全局设置，不能干扰并发测试。

/// 带可选邀请码的注册请求（返回原始响应；不像 register() 断言 200）
async fn register_invite(base: &str, name: &str, invite: Option<&str>) -> reqwest::Response {
    let mut body = json!({"username": name, "password": "secret123", "display_name": name});
    if let Some(c) = invite {
        body["invite_code"] = json!(c);
    }
    http()
        .post(format!("{base}/v1/auth/register"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// 直接 SQL 写 server_settings（覆盖「空库 + closed」等 API 无法到达的时序场景）
async fn set_mode_sql(pool: &sqlx::PgPool, mode: &str) {
    sqlx::query(
        "INSERT INTO server_settings (key, value) VALUES ('registration_mode', to_jsonb($1::text))
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(mode)
    .execute(pool)
    .await
    .unwrap();
}

async fn put_mode(base: &str, admin: &TestUser, mode: &str) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"registration_mode": mode}))
        .send()
        .await
        .unwrap()
}

async fn create_invite(base: &str, admin: &TestUser, body: Value) -> Value {
    let r = http()
        .post(format!("{base}/v1/admin/invites"))
        .header("authorization", authz(&admin.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "create invite failed");
    r.json().await.unwrap()
}

/// server-info 无需登录可访问，默认 open
#[tokio::test]
async fn p12_server_info_public() {
    let base = spawn_named_db(&format!("chat_t_p12a_{}", uniq())).await;
    let c = http();
    let r: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["registration_mode"], "open");

    // 管理员改模式后公共端点立即反映
    let admin = register(&base, &format!("p12a_{}", uniq())).await;
    let r = put_mode(&base, &admin, "invite").await;
    assert_eq!(r.status(), 200);
    let r: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["registration_mode"], "invite");
}

/// 非管理员访问 settings / invites → 403；未登录 → 401
#[tokio::test]
async fn p12_admin_endpoints_require_admin() {
    let base = spawn_named_db(&format!("chat_t_p12b_{}", uniq())).await;
    let c = http();
    let _admin = register(&base, &format!("p12b_{}", uniq())).await;
    let u = register(&base, &format!("p12c_{}", uniq())).await;
    let inv_id = uuid::Uuid::new_v4();

    for (method, path) in [
        ("GET", "/v1/admin/settings".to_string()),
        ("PUT", "/v1/admin/settings".to_string()),
        ("GET", "/v1/admin/invites".to_string()),
        ("POST", "/v1/admin/invites".to_string()),
        ("DELETE", format!("/v1/admin/invites/{inv_id}")),
    ] {
        let req = c
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .header("authorization", authz(&u.access))
            .json(&json!({}));
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 403, "{method} {path} 应拒绝普通用户");
        let r = c
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{method} {path} 应拒绝未登录");
    }
}

/// closed：空库时首个用户仍放行（引导例外），之后一律 403
#[tokio::test]
async fn p12_closed_mode_bootstrap() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12d_{}", uniq())).await;
    // 空库直接 SQL 置 closed——等价于部署后没注册过人就关了注册
    set_mode_sql(&st.pool, "closed").await;

    let r = register_invite(&base, &format!("p12d_{}", uniq()), None).await;
    assert_eq!(
        r.status(),
        200,
        "空库引导例外：closed 模式下首个用户仍应注册成功"
    );

    let r = register_invite(&base, &format!("p12e_{}", uniq()), None).await;
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "管理员已关闭注册");
}

/// invite 模式：无码/错码 403；有效码 200 且 uses+1、users.invite_id 写入；撤销/过期 403
#[tokio::test]
async fn p12_invite_mode() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12f_{}", uniq())).await;
    let c = http();
    let admin = register(&base, &format!("p12f_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);

    // 无码 → 403
    let r = register_invite(&base, &format!("p12g_{}", uniq()), None).await;
    assert_eq!(r.status(), 403);
    // 错码 → 403
    let r = register_invite(&base, &format!("p12g_{}", uniq()), Some("NOTEXIST99")).await;
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "邀请码无效或已失效");

    // 有效码（故意传小写+首尾空白，服务端规范化）→ 200
    let inv = create_invite(&base, &admin, json!({"note": "test"})).await;
    let code = inv["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 10);
    let fuzzy = format!("  {} ", code.to_lowercase());
    let invited_name = format!("p12g_{}", uniq());
    let r = register_invite(&base, &invited_name, Some(&fuzzy)).await;
    assert_eq!(r.status(), 200, "有效码（大小写/空白规范化后）应注册成功");

    // uses +1、users.invite_id 写入
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    let invite_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT invite_id FROM users WHERE username = $1")
            .bind(&invited_name)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(
        invite_id,
        Some(uuid::Uuid::parse_str(inv["id"].as_str().unwrap()).unwrap())
    );

    // 列表视图字段完整：usable=true
    let list: Value = c
        .get(format!("{base}/v1/admin/invites"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &list.as_array().unwrap()[0];
    assert_eq!(row["code"], code);
    assert_eq!(row["uses"], 1);
    assert_eq!(row["usable"], true);
    assert!(!row["created_by_name"].as_str().unwrap().is_empty());

    // 撤销 → 幂等 204，之后码失效
    let r = c
        .delete(format!(
            "{base}/v1/admin/invites/{}",
            inv["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .delete(format!(
            "{base}/v1/admin/invites/{}",
            inv["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "撤销应幂等");
    let r = register_invite(&base, &format!("p12h_{}", uniq()), Some(&code)).await;
    assert_eq!(r.status(), 403);

    // 过期码 → 403（SQL 直接把 expires_at 拨到过去）
    let inv2 = create_invite(&base, &admin, json!({"expires_in_hours": 24})).await;
    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 hour' WHERE id = $1")
        .bind(uuid::Uuid::parse_str(inv2["id"].as_str().unwrap()).unwrap())
        .execute(&st.pool)
        .await
        .unwrap();
    let r = register_invite(
        &base,
        &format!("p12i_{}", uniq()),
        Some(inv2["code"].as_str().unwrap()),
    )
    .await;
    assert_eq!(r.status(), 403);
}

/// max_uses=1：两个请求并发用同一码注册，恰一个 200、一个 403
#[tokio::test]
async fn p12_invite_max_uses_race() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12j_{}", uniq())).await;
    let admin = register(&base, &format!("p12j_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);
    let inv = create_invite(&base, &admin, json!({"max_uses": 1})).await;
    let code = inv["code"].as_str().unwrap().to_string();

    let (n1, n2) = (format!("p12k_{}", uniq()), format!("p12l_{}", uniq()));
    let (r1, r2) = tokio::join!(
        register_invite(&base, &n1, Some(&code)),
        register_invite(&base, &n2, Some(&code)),
    );
    let mut codes = [r1.status().as_u16(), r2.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 403], "恰好一个成功一个拒绝");

    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
}

/// 注册因用户名冲突失败时事务回滚，邀请次数不消耗
#[tokio::test]
async fn p12_invite_rollback_on_conflict() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12m_{}", uniq())).await;
    let admin = register(&base, &format!("p12m_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);
    let inv = create_invite(&base, &admin, json!({})).await;
    let code = inv["code"].as_str().unwrap().to_string();

    let name = format!("p12n_{}", uniq());
    assert_eq!(
        register_invite(&base, &name, Some(&code)).await.status(),
        200
    );
    // 同名再注册 → 409；uses 必须保持 1
    assert_eq!(
        register_invite(&base, &name, Some(&code)).await.status(),
        409
    );
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1, "冲突回滚后邀请次数不应增加");
}

/// open（默认）忽略 invite_code：带错误码也能注册
#[tokio::test]
async fn p12_open_ignores_invite() {
    let base = spawn_named_db(&format!("chat_t_p12o_{}", uniq())).await;
    let _admin = register(&base, &format!("p12o_{}", uniq())).await;
    let r = register_invite(&base, &format!("p12p_{}", uniq()), Some("BADCODE123")).await;
    assert_eq!(r.status(), 200);
}

/// PUT /admin/settings 未知模式 → 400；合法值返回完整设置
#[tokio::test]
async fn p12_settings_validation() {
    let base = spawn_named_db(&format!("chat_t_p12q_{}", uniq())).await;
    let admin = register(&base, &format!("p12q_{}", uniq())).await;

    let r = put_mode(&base, &admin, "bogus").await;
    assert_eq!(r.status(), 400);

    let r = put_mode(&base, &admin, "invite").await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["registration_mode"], "invite");

    let body: Value = http()
        .get(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["registration_mode"], "invite");

    // 创建参数边界：max_uses / expires_in_hours 越界 → 400
    for body in [
        json!({"max_uses": 0}),
        json!({"max_uses": 1001}),
        json!({"expires_in_hours": 0}),
        json!({"expires_in_hours": 8761}),
    ] {
        let r = http()
            .post(format!("{base}/v1/admin/invites"))
            .header("authorization", authz(&admin.access))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{body} 应 400");
    }
}

// ===== V2 P2-1 =====
// 图片尺寸/缩略图 + 每用户存储配额

/// 现场生成 PNG（与 server 共用 image 依赖）
fn png_fixture(w: u32, h: u32) -> Vec<u8> {
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
async fn upload_att(
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

/// spawn() 共用的 files_dir（与 spawn() 中的路径一致）
fn it_files_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("chat-it-files")
}

/// 物理清理由 tokio::spawn 异步执行：轮询文件存在性直到达到期望状态
async fn wait_file(path: &std::path::Path, exists: bool) {
    for _ in 0..50 {
        if path.exists() == exists {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("file state not reached: {}", path.display());
}

/// 复制一行 attachments（新 id，共享 storage_path/thumb_path），模拟 P2-4 转发副本
async fn copy_attachment(pool: &sqlx::PgPool, src_id: &str) -> String {
    let copy_id = uuid::Uuid::new_v4();
    let n = sqlx::query(
        "INSERT INTO attachments
             (id, uploader_id, conv_id, file_name, mime, size, storage_path, width, height, thumb_path)
         SELECT $1, uploader_id, conv_id, file_name, mime, size, storage_path, width, height, thumb_path
           FROM attachments WHERE id = $2",
    )
    .bind(copy_id)
    .bind(uuid::Uuid::parse_str(src_id).unwrap())
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1);
    copy_id.to_string()
}

/// 大图：上传记录宽高 + 生成缩略图；?thumb=1 拿 JPEG；消息视图字段一致
#[tokio::test]
async fn p2_1_image_dims_thumb() {
    let base = spawn().await;
    let a = register(&base, &format!("p2a_{}", uniq())).await;
    let b = register(&base, &format!("p2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (st, att) = upload_att(
        &base,
        &conv,
        &a.access,
        "big.png",
        "image/png",
        png_fixture(1200, 800),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap();
    assert_eq!(att["width"], 1200);
    assert_eq!(att["height"], 800);
    assert_eq!(att["has_thumb"], true);

    // ?thumb=1 → JPEG，最长边 480
    let dl = c
        .get(format!("{base}/v1/files/{att_id}?thumb=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.headers()["content-type"], "image/jpeg");
    let t = image::load_from_memory(&dl.bytes().await.unwrap()).unwrap();
    assert_eq!((t.width(), t.height()), (480, 320));

    // dl=1 忽略 thumb：返回原图
    let dl = c
        .get(format!("{base}/v1/files/{att_id}?thumb=1&dl=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.headers()["content-type"], "image/png");

    // 消息视图字段与上传响应一致
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "i1", "content": "", "attachment_id": att_id}),
    )
    .await;
    assert_eq!(m["kind"], "image");
    assert_eq!(m["attachment"]["width"], 1200);
    assert_eq!(m["attachment"]["height"], 800);
    assert_eq!(m["attachment"]["has_thumb"], true);
}

/// 小图有宽高无缩略图（?thumb=1 回退原图）；坏图照常入库、字段为 null
#[tokio::test]
async fn p2_1_small_and_broken_image() {
    let base = spawn().await;
    let a = register(&base, &format!("s2a_{}", uniq())).await;
    let b = register(&base, &format!("s2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let png = png_fixture(300, 200);
    let (st, att) = upload_att(&base, &conv, &a.access, "s.png", "image/png", png.clone()).await;
    assert_eq!(st, 200);
    assert_eq!(att["width"], 300);
    assert_eq!(att["height"], 200);
    assert_eq!(att["has_thumb"], false);
    let dl = c
        .get(format!(
            "{base}/v1/files/{}?thumb=1",
            att["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.headers()["content-type"], "image/png");
    assert_eq!(dl.bytes().await.unwrap().as_ref(), png.as_slice());

    // 坏图：mime 声明 image/png 但内容不可解码 → 照常入库，尺寸/缩略图缺省
    let junk: Vec<u8> = (0..8192).map(|i| (i * 31 % 256) as u8).collect();
    let (st, att) = upload_att(&base, &conv, &a.access, "b.png", "image/png", junk).await;
    assert_eq!(st, 200);
    assert!(att["width"].is_null());
    assert!(att["height"].is_null());
    assert_eq!(att["has_thumb"], false);
}

/// 删除消息：原文件与缩略图一起清理；仍有副本共享 storage_path 时两者都保留
#[tokio::test]
async fn p2_1_delete_cleans_thumb() {
    let base = spawn().await;
    let a = register(&base, &format!("d2a_{}", uniq())).await;
    let b = register(&base, &format!("d2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let (_, att) = upload_att(
        &base,
        &conv,
        &a.access,
        "d.png",
        "image/png",
        png_fixture(900, 700),
    )
    .await;
    let id = att["id"].as_str().unwrap();
    let file = it_files_dir().join(id);
    let thumb = it_files_dir().join(format!("{id}.thumb.jpg"));
    assert!(file.exists());
    assert!(thumb.exists());

    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d1", "content": "", "attachment_id": id}),
    )
    .await;
    let r = c
        .delete(format!("{base}/v1/messages/{}", m["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_file(&file, false).await;
    wait_file(&thumb, false).await;

    // 共享 storage_path 的副本（模拟转发）存在时，删除一条不清理物理文件
    let (_, att2) = upload_att(
        &base,
        &conv,
        &a.access,
        "d2.png",
        "image/png",
        png_fixture(900, 700),
    )
    .await;
    let id2 = att2["id"].as_str().unwrap();
    let file2 = it_files_dir().join(id2);
    let thumb2 = it_files_dir().join(format!("{id2}.thumb.jpg"));
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let copy = copy_attachment(&pool, id2).await;
    let m2 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d2", "content": "", "attachment_id": id2}),
    )
    .await;
    let m3 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d3", "content": "", "attachment_id": copy}),
    )
    .await;

    let r = c
        .delete(format!("{base}/v1/messages/{}", m2["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 清理是异步的；等一个足够窗口后两文件都应仍在（副本引用计数兜底）
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(file2.exists());
    assert!(thumb2.exists());

    let r = c
        .delete(format!("{base}/v1/messages/{}", m3["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_file(&file2, false).await;
    wait_file(&thumb2, false).await;
}

/// 配额：1MB 上限 → 600KB 成功、再 600KB 拒绝；共享行不重复计；0 恢复不限
#[tokio::test]
async fn p2_1_storage_quota() {
    let (base, state) = spawn_named_state(&format!("chat_t_p21q_{}", uniq())).await;
    let admin = register(&base, &format!("qad_{}", uniq())).await;
    let u = register(&base, &format!("qu_{}", uniq())).await;
    befriend(&base, &admin, &u).await;
    let conv = dm_conv(&base, &u).await;
    let c = http();

    // 缺省不限额
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 0);
    assert!(s["quota_bytes"].is_null());

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["storage_quota_mb"], 1);

    // 600KB 成功；再 600KB → 413 带中文提示
    let blob = vec![7u8; 600 * 1024];
    let (st, att) = upload_att(
        &base,
        &conv,
        &u.access,
        "a.bin",
        "application/octet-stream",
        blob.clone(),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    let (st, err) = upload_att(
        &base,
        &conv,
        &u.access,
        "b.bin",
        "application/octet-stream",
        blob,
    )
    .await;
    assert_eq!(st, 413);
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("存储空间已满"),
        "unexpected error body: {err}"
    );

    // me/storage：用量与上限
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 614400);
    assert_eq!(s["quota_bytes"], 1048576);

    // 共享 storage_path 的副本不重复计
    copy_attachment(&state.pool, &att_id).await;
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 614400);

    // admin/users 行带 storage_bytes
    let users: Value = c
        .get(format!("{base}/v1/admin/users"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == u.id)
        .unwrap();
    assert_eq!(row["storage_bytes"], 614400);

    // 调 0 → 恢复不限
    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (st, _) = upload_att(
        &base,
        &conv,
        &u.access,
        "c.bin",
        "application/octet-stream",
        vec![9u8; 600 * 1024],
    )
    .await;
    assert_eq!(st, 200);
}

/// 非管理员设配额 403；非法值 400；server-info 不暴露配额
#[tokio::test]
async fn p2_1_quota_admin_and_validation() {
    let base = spawn_named_db(&format!("chat_t_p21v_{}", uniq())).await;
    let admin = register(&base, &format!("vad_{}", uniq())).await;
    let u = register(&base, &format!("vu_{}", uniq())).await;
    let c = http();

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&u.access))
        .json(&json!({"storage_quota_mb": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    for v in [json!(-1), json!(1048577), json!("5"), json!(1.5)] {
        let r = c
            .put(format!("{base}/v1/admin/settings"))
            .header("authorization", authz(&admin.access))
            .json(&json!({"storage_quota_mb": v}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{v} 应 400");
    }

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.get("storage_quota_mb").is_none());
}

// ===== V2 P2-2 =====

/// 最小合法 PNG（1×1）。服务端只校验魔数，不解码。
fn png_bytes() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

/// 最小合法 JPEG（文件头魔数 + SOI/APP0）。
fn jpeg_bytes() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

fn avatar_part(body: Vec<u8>, mime: &str) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(body)
        .file_name("avatar.bin")
        .mime_str(mime)
        .unwrap();
    reqwest::multipart::Form::new().part("file", part)
}

async fn put_avatar(base: &str, token: &str, body: Vec<u8>, mime: &str) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(token))
        .multipart(avatar_part(body, mime))
        .send()
        .await
        .unwrap()
}

/// 头像用例用独立 FILES_DIR，避免并行测试互相删文件。
async fn spawn_avatar() -> (String, std::path::PathBuf) {
    ensure_test_db().await;
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let files_dir = std::env::temp_dir().join(format!("chat-it-avatar-{}", uniq()));
    let cfg = Config {
        database_url: test_db_url(),
        jwt_secret: "test-secret".into(),
        bind: "127.0.0.1".into(),
        port: 0,
        app_dist: None,
        files_dir: files_dir.to_string_lossy().into(),
        upload_max_bytes: 25 * 1024 * 1024,
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
    (format!("http://{addr}"), files_dir)
}

/// 上传 PNG：版本 0→1、has_avatar；当前版本 immutable，旧版本号 no-cache，字节一致。
#[tokio::test]
async fn p22_upload_png_and_cache_headers() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("av_{}", uniq())).await;
    let png = png_bytes();

    let me: Value = put_avatar(&base, &u.access, png.clone(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 1);
    assert_eq!(me["has_avatar"], true);
    assert_eq!(me["id"], u.id);

    let cur = http()
        .get(format!("{base}/v1/users/{}/avatar?v=1", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(cur.status(), 200);
    assert_eq!(
        cur.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        cur.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(cur.headers().get("content-type").unwrap(), "image/png");
    assert_eq!(cur.bytes().await.unwrap().as_ref(), png.as_slice());

    let stale = http()
        .get(format!("{base}/v1/users/{}/avatar?v=0", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 200);
    assert_eq!(
        stale
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    assert_eq!(stale.bytes().await.unwrap().as_ref(), png.as_slice());
}

/// 再次上传升到版本 2，旧文件最终被删除。
#[tokio::test]
async fn p22_replace_deletes_old_file() {
    let (base, files_dir) = spawn_avatar().await;
    let u = register(&base, &format!("avr_{}", uniq())).await;
    let png = png_bytes();
    let jpeg = jpeg_bytes();

    let first: Value = put_avatar(&base, &u.access, png.clone(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first["avatar_version"], 1);

    let second: Value = put_avatar(&base, &u.access, jpeg.clone(), "image/jpeg")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(second["avatar_version"], 2);
    assert_eq!(second["has_avatar"], true);

    let cur = http()
        .get(format!("{base}/v1/users/{}/avatar?v=2", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(cur.status(), 200);
    assert_eq!(cur.headers().get("content-type").unwrap(), "image/jpeg");
    assert_eq!(cur.bytes().await.unwrap().as_ref(), jpeg.as_slice());

    // 提交后异步删旧文件。旧版本请求仍回当前图，但磁盘上不应再有 v1 文件。
    let dir = files_dir.join("avatars");
    let old = dir.join(format!("{}-1.png", u.id));
    let mut gone = false;
    for _ in 0..50 {
        if !old.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(gone, "旧头像文件应在更换后被删除");
    assert!(dir.join(format!("{}-2.jpg", u.id)).exists());
}

/// 声称 image/png 但内容是 HTML，以及超过 1MB，都是 400。
#[tokio::test]
async fn p22_rejects_spoofed_type_and_oversize() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("avb_{}", uniq())).await;

    let html = put_avatar(
        &base,
        &u.access,
        b"<html><script>alert(1)</script></html>".to_vec(),
        "image/png",
    )
    .await;
    assert_eq!(html.status(), 400);

    let big = put_avatar(
        &base,
        &u.access,
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
            .into_iter()
            .chain(vec![0u8; 1_048_577 - 8])
            .collect(),
        "image/png",
    )
    .await;
    assert_eq!(big.status(), 400);

    let me: Value = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 0);
    assert_eq!(me["has_avatar"], false);
}

/// 删除后 has_avatar=false，公开 GET 404，版本仍递增。
#[tokio::test]
async fn p22_delete_clears_avatar() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("avd_{}", uniq())).await;
    let uploaded: Value = put_avatar(&base, &u.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(uploaded["avatar_version"], 1);

    let me: Value = http()
        .delete(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["has_avatar"], false);
    assert_eq!(me["avatar_version"], 2);

    let r = http()
        .get(format!("{base}/v1/users/{}/avatar?v=2", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // 已经没有头像再删：版本不再递增，避免空操作刷失效
    let again: Value = http()
        .delete(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["has_avatar"], false);
    assert_eq!(again["avatar_version"], 2);
}

/// 未登录不能上传；接口只作用于自己。
#[tokio::test]
async fn p22_upload_requires_self() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("ava_{}", uniq())).await;
    let b = register(&base, &format!("avb2_{}", uniq())).await;

    let anon = http()
        .put(format!("{base}/v1/users/me/avatar"))
        .multipart(avatar_part(png_bytes(), "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["id"], a.id);
    assert_eq!(me["avatar_version"], 1);

    let b_me: Value = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(b_me["has_avatar"], false);
    assert_eq!(b_me["avatar_version"], 0);
}

/// 好友列表、会话 peer、消息 sender_avatar_version 都反映新版本。
#[tokio::test]
async fn p22_avatar_visible_on_friends_peer_and_messages() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("avs_{}", uniq())).await;
    let b = register(&base, &format!("avr2_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 1);

    let friends: Value = http()
        .get(format!("{base}/v1/friends"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let friend = friends
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["user"]["id"] == a.id)
        .unwrap();
    assert_eq!(friend["user"]["avatar_version"], 1);
    assert_eq!(friend["user"]["has_avatar"], true);

    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let peer = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv)
        .unwrap();
    assert_eq!(peer["peer"]["avatar_version"], 1);
    assert_eq!(peer["peer"]["has_avatar"], true);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let sent = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "av1", "content": "hi"}),
    )
    .await;
    assert_eq!(sent["sender_avatar_version"], 1);
    assert_eq!(sent["sender_has_avatar"], true);

    let page = history(&base, &b.access, &conv).await;
    let listed = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sender_id"] == a.id)
        .unwrap();
    assert_eq!(listed["sender_avatar_version"], 1);
    assert_eq!(listed["sender_has_avatar"], true);
}

/// 头像变更后好友的 WS 收到 invalidate { what: conversations }。
#[tokio::test]
async fn p22_avatar_change_invalidates_watchers() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("avw_{}", uniq())).await;
    let b = register(&base, &format!("avw2_{}", uniq())).await;
    befriend(&base, &a, &b).await;

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["has_avatar"], true);

    let frame = next_of(&mut sb, "invalidate").await;
    assert_eq!(frame["what"], "conversations");
}

// ===== V2 P2-3 =====

/// 上传音频附件。`duration` 为 None 时不带字段；Some 时写入 multipart 文本。
async fn upload_audio(
    base: &str,
    conv: &str,
    at: &str,
    name: &str,
    mime: &str,
    body: &[u8],
    duration: Option<&str>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(body.to_vec())
        .file_name(name.to_string())
        .mime_str(mime)
        .unwrap();
    let mut form = reqwest::multipart::Form::new()
        .text("conversation_id", conv.to_string())
        .part("file", part);
    if let Some(d) = duration {
        form = form.text("duration_ms", d.to_string());
    }
    http()
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(at))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

fn assert_voice(view: &Value, duration: i64) {
    assert_eq!(view["kind"], "audio");
    assert_eq!(view["content"], "[语音]");
    assert_eq!(view["attachment"]["duration_ms"].as_i64(), Some(duration));
    assert_eq!(view["attachment"]["mime"], "audio/webm");
}

/// 语音消息：上传带时长 → 发送后 kind/占位/时长在历史、广播、sync 中一致。
#[tokio::test]
async fn p23_voice_upload_send_sync() {
    let base = spawn().await;
    let a = register(&base, &format!("v3a_{}", uniq())).await;
    let b = register(&base, &format!("v3b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "voice.webm",
        "audio/webm",
        b"webm-bytes",
        Some("5300"),
    )
    .await;
    assert_eq!(up.status(), 200);
    let att: Value = up.json().await.unwrap();
    assert_eq!(att["duration_ms"].as_i64(), Some(5300));
    let att_id = att["id"].as_str().unwrap();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let ack = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "v1", "content": "", "attachment_id": att_id}),
    )
    .await;
    assert_voice(&ack, 5300);

    let broadcast = next_of(&mut sb, "message").await;
    assert_voice(&broadcast["message"], 5300);

    let page = history(&base, &b.access, &conv).await;
    let hist = &page["messages"][0];
    assert_voice(hist, 5300);

    sb.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let synced = next_of(&mut sb, "sync_messages").await;
    assert_voice(&synced["messages"][0], 5300);
}

/// duration_ms 只接受音频，且必须是 1–300000 的整数。
#[tokio::test]
async fn p23_duration_rejected() {
    let base = spawn().await;
    let a = register(&base, &format!("v3c_{}", uniq())).await;
    let b = register(&base, &format!("v3d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47];

    let cases = [
        ("audio/webm", "voice.webm", b"a".as_slice(), Some("0")),
        ("audio/webm", "voice.webm", b"a", Some("300001")),
        ("audio/webm", "voice.webm", b"a", Some("nope")),
        ("image/png", "pic.png", png, Some("5300")),
    ];
    for (mime, name, body, dur) in cases {
        let r = upload_audio(&base, &conv, &a.access, name, mime, body, dur).await;
        assert_eq!(r.status(), 400, "{name} duration={dur:?} 应 400");
    }
}

/// 普通音频文件不带 duration_ms，仍是 file，时长为空。
#[tokio::test]
async fn p23_plain_audio_stays_file() {
    let base = spawn().await;
    let a = register(&base, &format!("v3e_{}", uniq())).await;
    let b = register(&base, &format!("v3f_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "song.mp3",
        "audio/mpeg",
        b"mp3",
        None,
    )
    .await;
    assert_eq!(up.status(), 200);
    let att: Value = up.json().await.unwrap();
    assert!(att.get("duration_ms").unwrap().is_null());

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let ack = ws_send(
        &mut sa,
        &conv,
        json!({
            "client_msg_id": "f1",
            "content": "",
            "attachment_id": att["id"].as_str().unwrap()
        }),
    )
    .await;
    assert_eq!(ack["kind"], "file");
    assert_eq!(ack["content"], "[文件] song.mp3");
    assert!(ack["attachment"]["duration_ms"].is_null());
}

/// 语音不可编辑；删除后墓碑不再带出 duration_ms。
#[tokio::test]
async fn p23_voice_edit_denied_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("v3g_{}", uniq())).await;
    let b = register(&base, &format!("v3h_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "voice.webm",
        "audio/webm",
        b"webm",
        Some("1500"),
    )
    .await;
    let att: Value = up.json().await.unwrap();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let sent = ws_send(
        &mut sa,
        &conv,
        json!({
            "client_msg_id": "v2",
            "content": "",
            "attachment_id": att["id"].as_str().unwrap()
        }),
    )
    .await;
    let mid = sent["id"].as_str().unwrap();

    let edit = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content": "改一下", "expected_version": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), 400);

    let del = c
        .delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let tomb: Value = del.json().await.unwrap();
    assert!(tomb["deleted_at"].as_str().is_some());
    assert!(tomb["attachment"].is_null());
    assert!(tomb
        .get("attachment")
        .and_then(|a| a.get("duration_ms"))
        .is_none());

    let page = history(&base, &b.access, &conv).await;
    let hist = &page["messages"][0];
    assert!(hist["attachment"].is_null());
    assert!(hist["content"] != "[语音]");
}

// ===== V2 P2-4 =====
// 文件传输助手（type='self' 单人会话）+ 消息转发/收藏。

/// 本人的 self 会话列表项（kind='self'、name NULL、member_count=1、peer NULL）
async fn self_conv(base: &str, u: &TestUser) -> Value {
    let convs = conv_list(base, u).await;
    convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "self")
        .expect("self conversation missing")
        .clone()
}

/// POST /v1/messages/{id}/forward
async fn post_forward(
    base: &str,
    u: &TestUser,
    msg_id: &str,
    targets: &[String],
    op: &str,
) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/messages/{msg_id}/forward"))
        .header("authorization", authz(&u.access))
        .json(&json!({"conversation_ids": targets, "client_op_id": op}))
        .send()
        .await
        .unwrap()
}

/// 新注册：会话列表恰有一个 self 会话；后台统计不把 self 计入 dm/group。
/// 独立库：首个注册者即管理员，顺手校验 admin_overview 口径。
#[tokio::test]
async fn p24_self_conv_on_register() {
    let base = spawn_named_db(&format!("chat_it_p24a_{}", uniq())).await;
    let a = register(&base, &format!("p24a_{}", uniq())).await;
    let convs = conv_list(&base, &a).await;
    let selves: Vec<_> = convs
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "self")
        .collect();
    assert_eq!(selves.len(), 1, "每用户恰一个 self 会话");
    let sc = selves[0];
    assert!(sc["name"].is_null(), "self 会话 name 为 NULL");
    assert_eq!(sc["member_count"], 1);
    assert!(sc["peer"].is_null());

    // 后台统计：self 不计入 dm_count / group_count
    let ov: Value = http()
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ov["dm_count"], 0);
    assert_eq!(ov["group_count"], 0);
}

/// self 会话发消息：本人两条 WS 连接都实时收到（多设备同步）；
/// 他人无法访问该会话历史（403）。
#[tokio::test]
async fn p24_self_conv_sync_devices() {
    let base = spawn().await;
    let a = register(&base, &format!("p24b_{}", uniq())).await;
    let b = register(&base, &format!("p24c_{}", uniq())).await;
    let conv = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut s1, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut s2, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut s1, "presence_init").await;
    next_of(&mut s2, "presence_init").await;

    ws_send(
        &mut s1,
        &conv,
        json!({"client_msg_id": "self1", "content": "memo to self"}),
    )
    .await;
    // 本人第二条连接实时收到同一消息
    let m2 = next_of(&mut s2, "message").await;
    assert_eq!(m2["message"]["conversation_id"], conv);
    assert_eq!(m2["message"]["content"], "memo to self");

    // 非成员访问历史 → 403
    let r = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// self 会话不可做：拉人/踢人/退出/改名/转让/解散/建投票 → 400
#[tokio::test]
async fn p24_self_conv_restrictions() {
    let base = spawn().await;
    let a = register(&base, &format!("p24d_{}", uniq())).await;
    let b = register(&base, &format!("p24e_{}", uniq())).await;
    // 先成为好友：拉人入口的好友校验通过后才能命中类型守卫 400
    befriend(&base, &a, &b).await;
    let conv = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c = http();
    let az = authz(&a.access);

    let cases: Vec<(reqwest::Response, &str)> = vec![
        (
            c.post(format!("{base}/v1/conversations/{conv}/members"))
                .header("authorization", az.clone())
                .json(&json!({"user_ids": [b.id]}))
                .send()
                .await
                .unwrap(),
            "拉人",
        ),
        (
            c.delete(format!("{base}/v1/conversations/{conv}/members/{}", b.id))
                .header("authorization", az.clone())
                .send()
                .await
                .unwrap(),
            "踢人",
        ),
        (
            c.delete(format!("{base}/v1/conversations/{conv}/members/me"))
                .header("authorization", az.clone())
                .send()
                .await
                .unwrap(),
            "退出",
        ),
        (
            c.patch(format!("{base}/v1/conversations/{conv}"))
                .header("authorization", az.clone())
                .json(&json!({"name": "改名"}))
                .send()
                .await
                .unwrap(),
            "改名",
        ),
        (
            c.post(format!("{base}/v1/conversations/{conv}/transfer"))
                .header("authorization", az.clone())
                .json(&json!({"user_id": b.id}))
                .send()
                .await
                .unwrap(),
            "转让",
        ),
        (
            c.delete(format!("{base}/v1/conversations/{conv}"))
                .header("authorization", az.clone())
                .send()
                .await
                .unwrap(),
            "解散",
        ),
        (
            c.post(format!("{base}/v1/conversations/{conv}/polls"))
                .header("authorization", az.clone())
                .json(&json!({"question": "q?", "options": ["a", "b"]}))
                .send()
                .await
                .unwrap(),
            "建投票",
        ),
    ];
    for (r, name) in cases {
        assert_eq!(r.status(), 400, "self 会话{name}应 400");
    }
}

/// 转发文字到一个 DM 和一个群：forwarded_from 正确、目标 seq 各自递增、
/// 目标成员实时收到 message 帧。
#[tokio::test]
async fn p24_forward_text_to_dm_and_group() {
    let base = spawn().await;
    let a = register(&base, &format!("p24f_{}", uniq())).await;
    let b = register(&base, &format!("p24g_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    // b 在 DM 发源消息（a 是成员可转发）。
    // 注意：ws_send 等待 message_ack 时会把先到的 message 广播帧一并消费，
    // 因此此处 sb 帧流是干净的，后续 next_of 只可能收到转发帧。
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;
    let view = ws_send(
        &mut sb,
        &dm,
        json!({"client_msg_id": "src1", "content": "link https://x"}),
    )
    .await;
    let src = view["id"].as_str().unwrap().to_string();
    let src_seq = view["seq"].as_i64().unwrap();

    // a 转发到 [群, DM]
    let r = post_forward(
        &base,
        &a,
        &src,
        &[gid.clone(), dm.clone()],
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let g_res = results
        .iter()
        .find(|x| x["conversation_id"] == gid)
        .unwrap();
    let dm_res = results.iter().find(|x| x["conversation_id"] == dm).unwrap();
    assert_eq!(g_res["ok"], true, "{g_res}");
    assert_eq!(dm_res["ok"], true, "{dm_res}");
    // 转发来源 = 源发送者展示名
    assert_eq!(g_res["message"]["forwarded_from"], b.username);
    assert_eq!(dm_res["message"]["forwarded_from"], b.username);
    assert_eq!(g_res["message"]["content"], "link https://x");
    // seq 各自递增：DM 已有源消息，群是首条
    assert_eq!(dm_res["message"]["seq"], src_seq + 1);
    assert_eq!(g_res["message"]["seq"], 1);

    // 目标成员 b 实时收到两条广播
    let f1 = next_of(&mut sb, "message").await;
    let f2 = next_of(&mut sb, "message").await;
    let got: std::collections::HashSet<String> = [f1, f2]
        .iter()
        .map(|f| {
            f["message"]["conversation_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(got.contains(&dm) && got.contains(&gid), "{got:?}");
}

/// 转发附件：目标会话克隆 attachment 行（新 id、共享物理文件）。
/// 源删除后目标仍可下载；副本删除后物理文件被清理（异步，轮询等待）。
#[tokio::test]
async fn p24_forward_attachment_lifecycle() {
    let base = spawn().await;
    let a = register(&base, &format!("p24h_{}", uniq())).await;
    let b = register(&base, &format!("p24i_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c = http();

    // a 传图到 self 会话并发送
    let part = reqwest::multipart::Part::bytes(b"png-bytes-p24".to_vec())
        .file_name("pic.png".to_string())
        .mime_str("image/png")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("conversation_id", selfc.clone())
        .part("file", part);
    let r = c
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(&a.access))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let att = r.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let view = ws_send(
        &mut sa,
        &selfc,
        json!({"client_msg_id": "img1", "content": "", "attachment_id": att}),
    )
    .await;
    let src = view["id"].as_str().unwrap().to_string();

    // 转发到 DM：新 attachment id，kind=image，uploader 保持原上传者
    let r = post_forward(
        &base,
        &a,
        &src,
        &[dm.clone()],
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let res = &body["results"][0];
    assert_eq!(res["ok"], true, "{res}");
    assert_eq!(res["message"]["kind"], "image");
    let fwd_id = res["message"]["id"].as_str().unwrap().to_string();
    let fwd_att = res["message"]["attachment"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(fwd_att, att, "克隆附件行得到新 id");

    // 目标附件可下载且内容一致
    let dl = c
        .get(format!("{base}/v1/files/{fwd_att}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.bytes().await.unwrap().as_ref(), b"png-bytes-p24");

    // 删除源消息 → 目标副本仍被引用，下载不受影响
    let r = c
        .delete(format!("{base}/v1/messages/{src}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let dl = c
        .get(format!("{base}/v1/files/{fwd_att}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);

    // 删除目标副本 → 最后一个 storage_path 引用消失，物理文件清理（异步轮询）
    let r = c
        .delete(format!("{base}/v1/messages/{fwd_id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let path = std::env::temp_dir().join("chat-it-files").join(&att);
    for _ in 0..50 {
        if !path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!path.exists(), "孤儿物理文件必须被清理");
}

/// 逐目标独立：已删好友的 DM 返回 ok=false/error="not_friends"，其他目标照常成功。
#[tokio::test]
async fn p24_forward_partial_failure() {
    let base = spawn().await;
    let a = register(&base, &format!("p24j_{}", uniq())).await;
    let b = register(&base, &format!("p24k_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c = http();

    // 删好友 → DM 只读；a 在 self 发源消息
    let r = c
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let view = ws_send(
        &mut sa,
        &selfc,
        json!({"client_msg_id": "src2", "content": "partial test"}),
    )
    .await;
    let src = view["id"].as_str().unwrap().to_string();

    // 转发到 [只读 DM, 自己的 self]：一败一成，互不影响
    let r = post_forward(
        &base,
        &a,
        &src,
        &[dm.clone(), selfc.clone()],
        &uuid::Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let dm_res = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["conversation_id"] == dm)
        .unwrap();
    let self_res = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["conversation_id"] == selfc)
        .unwrap();
    assert_eq!(dm_res["ok"], false);
    assert_eq!(dm_res["error"], "not_friends");
    assert_eq!(self_res["ok"], true, "{self_res}");
}

/// 同一 client_op_id 重试幂等：不产生重复消息，返回同一条；目标列表去重。
#[tokio::test]
async fn p24_forward_idempotent_retry() {
    let base = spawn().await;
    let a = register(&base, &format!("p24l_{}", uniq())).await;
    let b = register(&base, &format!("p24m_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c = http();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let view = ws_send(
        &mut sa,
        &selfc,
        json!({"client_msg_id": "src3", "content": "idem body"}),
    )
    .await;
    let src = view["id"].as_str().unwrap().to_string();

    let op = uuid::Uuid::new_v4().to_string();
    // 目标列表带重复 id：服务端去重
    let r = post_forward(&base, &a, &src, &[dm.clone(), dm.clone()], &op).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "重复目标去重");
    let first_id = results[0]["message"]["id"].as_str().unwrap().to_string();

    // 同一 op 重试：返回同一条消息，不重复发送
    let r = post_forward(&base, &a, &src, &[dm.clone()], &op).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][0]["message"]["id"], first_id);

    let page: Value = c
        .get(format!("{base}/v1/conversations/{dm}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fwd: Vec<_> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["forwarded_from"].is_string())
        .collect();
    assert_eq!(fwd.len(), 1, "幂等重试不产生重复消息");
}

/// 拒绝路径：投票 400 / 已删除 400 / 非成员源消息 403 / 目标数量越界 400；
/// 普通 WS send_message 仍不能借用他人 attachment_id。
#[tokio::test]
async fn p24_forward_rejections() {
    let base = spawn().await;
    let a = register(&base, &format!("p24n_{}", uniq())).await;
    let b = register(&base, &format!("p24o_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let a_self = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_self = self_conv(&base, &b).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c = http();
    let op = || uuid::Uuid::new_v4().to_string();

    // 1) 投票消息 → 400 投票不能转发
    let poll: Value = c
        .post(format!("{base}/v1/conversations/{gid}/polls"))
        .header("authorization", authz(&a.access))
        .json(&json!({"question": "q?", "options": ["x", "y"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = post_forward(
        &base,
        &a,
        poll["id"].as_str().unwrap(),
        &[dm.clone()],
        &op(),
    )
    .await;
    assert_eq!(r.status(), 400);
    let err = r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(err.contains("投票"), "{err}");

    // 2) 已删除消息 → 400 消息已删除
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let view = ws_send(
        &mut sa,
        &a_self,
        json!({"client_msg_id": "del1", "content": "soon deleted"}),
    )
    .await;
    let gone = view["id"].as_str().unwrap().to_string();
    let r = c
        .delete(format!("{base}/v1/messages/{gone}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = post_forward(&base, &a, &gone, &[dm.clone()], &op()).await;
    assert_eq!(r.status(), 400);
    let err = r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(err.contains("已删除"), "{err}");

    // 3) 转发他人 self 会话里的消息（a 非成员）→ 403
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;
    let view = ws_send(
        &mut sb,
        &b_self,
        json!({"client_msg_id": "bs1", "content": "private"}),
    )
    .await;
    let foreign = view["id"].as_str().unwrap().to_string();
    let r = post_forward(&base, &a, &foreign, &[dm.clone()], &op()).await;
    assert_eq!(r.status(), 403);

    // 4) 目标数量边界：空数组与 >10 都 400
    let r = post_forward(&base, &a, &foreign, &[], &op()).await;
    assert_eq!(r.status(), 400);
    let many: Vec<String> = (0..11).map(|_| dm.clone()).collect();
    let r = post_forward(&base, &a, &foreign, &many, &op()).await;
    assert_eq!(r.status(), 400);

    // 5) 普通 WS send_message 仍不能借用他人 attachment_id：
    //    b 传附件到自己的 self，a 拿去发到自己 self → not_found
    let part = reqwest::multipart::Part::bytes(b"other-bytes".to_vec())
        .file_name("other.txt".to_string())
        .mime_str("text/plain")
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("conversation_id", b_self.clone())
        .part("file", part);
    let r = c
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(&b.access))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let foreign_att = r.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":a_self,"client_msg_id":"steal1",
               "content":"","attachment_id":foreign_att})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sa, "error").await;
    assert_eq!(err["code"], "not_found");
}

// ===== V2 P3-1 =====
// 全局消息搜索（GET /v1/search）+ 会话内搜索墓碑修复

/// GET /v1/search：params 交给 reqwest 编码（含中文/百分号/RFC3339），返回 (status, body)
async fn gsearch(base: &str, at: &str, params: &[(&str, String)]) -> (u16, Value) {
    let r = http()
        .get(format!("{base}/v1/search"))
        .header("authorization", authz(at))
        .query(params)
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

fn gids(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

/// 全局搜索：跨会话命中按 created_at 倒序；非成员会话不出现；墓碑不命中；
/// 会话内搜索同步修好（回归）；退群后立即搜不到该会话。
#[tokio::test]
async fn p31_search_global_scope_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("sa_{}", uniq())).await;
    let b = register(&base, &format!("sb_{}", uniq())).await;
    let cx = register(&base, &format!("sc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &cx).await;
    befriend(&base, &b, &cx).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), cx.id.clone()]).await;

    let c = http();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let m1 = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"k1","content":"周报 alpha"}),
    )
    .await;
    let m2 = ws_send(
        &mut sb,
        &gid,
        json!({"client_msg_id":"k2","content":"周报 beta"}),
    )
    .await;
    // 将被删除的命中消息；另一个 a 非成员的群里含同关键词的消息
    let gone = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"k3","content":"周报 gone"}),
    )
    .await;
    let g2 = mk_group(&base, &b, &[cx.id.clone()]).await;
    ws_send(
        &mut sb,
        &g2,
        json!({"client_msg_id":"k4","content":"周报 secret"}),
    )
    .await;

    let r = c
        .delete(format!(
            "{base}/v1/messages/{}",
            gone["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 1) 两个会话都命中，按 created_at DESC（beta 后发 → 在前）；墓碑与外人会话不进结果
    let (st, page) = gsearch(&base, &a.access, &[("q", "周报".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            m2["id"].as_str().unwrap().to_string(),
            m1["id"].as_str().unwrap().to_string()
        ]
    );
    assert_eq!(page["has_more"], false);
    // 客户端按 conversation_id 对照本地会话目录显示会话名
    assert_eq!(
        page["messages"][0]["conversation_id"].as_str().unwrap(),
        gid
    );

    // 2) 搜墓碑占位文字不命中已删除消息
    let (st, page) = gsearch(&base, &a.access, &[("q", "消息已删除".into())]).await;
    assert_eq!(st, 200);
    assert!(gids(&page).is_empty(), "tombstone must not match");

    // 3) 会话内搜索回归：同样不返回墓碑、占位文字不命中
    let found: Value = c
        .get(format!(
            "{base}/v1/conversations/{gid}/messages/search?q=周报"
        ))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = found.as_array().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"].as_str().unwrap(), m2["id"].as_str().unwrap());
    let found: Value = c
        .get(format!(
            "{base}/v1/conversations/{gid}/messages/search?q=已删除"
        ))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(found.as_array().unwrap().is_empty());

    // 4) 非成员会话不可定向搜索；退群后历史立即搜不到
    let (st, _) = gsearch(&base, &a.access, &[("conversation_id", g2.clone())]).await;
    assert_eq!(st, 403);
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let (st, page) = gsearch(&base, &a.access, &[("q", "周报".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![m1["id"].as_str().unwrap().to_string()]);
    let (st, _) = gsearch(&base, &a.access, &[("conversation_id", gid.clone())]).await;
    assert_eq!(st, 403);
}

/// 筛选：conversation_id+kind、sender_id、from/to（含 from、不含 to）
#[tokio::test]
async fn p31_search_filters() {
    let base = spawn().await;
    let a = register(&base, &format!("fa_{}", uniq())).await;
    let b = register(&base, &format!("fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let t1 = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"f1","content":"找找这段文字"}),
    )
    .await;
    let (st, att) = upload_att(
        &base,
        &dm,
        &a.access,
        "p.png",
        "image/png",
        png_fixture(60, 40),
    )
    .await;
    assert_eq!(st, 200);
    let img = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"f2","content":"","attachment_id":att["id"].as_str().unwrap()}),
    )
    .await;
    let t2 = ws_send(
        &mut sb,
        &dm,
        json!({"client_msg_id":"f3","content":"对方的话"}),
    )
    .await;

    // conversation_id + kind=image → 只有该会话的图片消息
    let (st, page) = gsearch(
        &base,
        &a.access,
        &[("conversation_id", dm.clone()), ("kind", "image".into())],
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![img["id"].as_str().unwrap().to_string()]);
    assert_eq!(page["messages"][0]["kind"], "image");

    // sender_id → 只看 b 发的
    let (st, page) = gsearch(&base, &a.access, &[("sender_id", b.id.clone())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![t2["id"].as_str().unwrap().to_string()]);

    // kind=text → 两条文本（图片不算）
    let (st, page) = gsearch(&base, &a.access, &[("kind", "text".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            t2["id"].as_str().unwrap().to_string(),
            t1["id"].as_str().unwrap().to_string()
        ]
    );

    // from/to：以图片消息的 created_at 为界——含 from、不含 to
    let pivot = img["created_at"].as_str().unwrap().to_string();
    let (st, page) = gsearch(&base, &a.access, &[("from", pivot.clone())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            t2["id"].as_str().unwrap().to_string(),
            img["id"].as_str().unwrap().to_string()
        ]
    );
    let (st, page) = gsearch(&base, &a.access, &[("to", pivot)]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![t1["id"].as_str().unwrap().to_string()]);
}

/// 翻页：5 条命中、limit=2 分三页取完，不重不漏，has_more 正确
#[tokio::test]
async fn p31_search_pagination() {
    let base = spawn().await;
    let a = register(&base, &format!("pa_{}", uniq())).await;
    let b = register(&base, &format!("pb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let mut sent = Vec::new();
    for i in 0..5 {
        let m = ws_send(
            &mut sa,
            &dm,
            json!({"client_msg_id":format!("pg{i}"),"content":format!("分页 关键词 第{i}条")}),
        )
        .await;
        sent.push(m["id"].as_str().unwrap().to_string());
    }

    let mut got: Vec<String> = Vec::new();
    let mut cursor: Option<(String, String)> = None;
    for page_no in 0..3 {
        let mut params = vec![("q", "关键词".into()), ("limit", "2".into())];
        if let Some((before, before_id)) = &cursor {
            params.push(("before", before.clone()));
            params.push(("before_id", before_id.clone()));
        }
        let (st, page) = gsearch(&base, &a.access, &params).await;
        assert_eq!(st, 200);
        let msgs = page["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        cursor = Some((
            last["created_at"].as_str().unwrap().to_string(),
            last["id"].as_str().unwrap().to_string(),
        ));
        assert_eq!(
            page["has_more"].as_bool().unwrap(),
            page_no < 2,
            "page {page_no} has_more"
        );
        got.extend(gids(&page));
    }
    // 全部取到、无重复、按 created_at DESC = 发送顺序倒序
    let mut expect = sent.clone();
    expect.reverse();
    assert_eq!(got, expect);
}

/// 参数校验与通配符转义：全空 400、q 超长 400、非法 kind 400、非成员 403、%/_ 按字面量
#[tokio::test]
async fn p31_search_validation_and_escape() {
    let base = spawn().await;
    let a = register(&base, &format!("va_{}", uniq())).await;
    let b = register(&base, &format!("vb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let pct = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e1","content":"进度 100% 完成"}),
    )
    .await;
    let us = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e2","content":"变量 a_b 命名"}),
    )
    .await;
    ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e3","content":"变量 axb 不命中"}),
    )
    .await;

    // % 与 _ 按字面量，不当通配符
    let (st, page) = gsearch(&base, &a.access, &[("q", "100%".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![pct["id"].as_str().unwrap().to_string()]);
    let (st, page) = gsearch(&base, &a.access, &[("q", "a_b".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![us["id"].as_str().unwrap().to_string()]);

    // 参数全空 → 400；q 全空白且无筛选 → 400；q 101 字 → 400；非法 kind → 400
    let (st, _) = gsearch(&base, &a.access, &[]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "   ".into())]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "x".repeat(101))]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "x".repeat(100))]).await;
    assert_eq!(st, 200);
    let (st, _) = gsearch(&base, &a.access, &[("kind", "bogus".into())]).await;
    assert_eq!(st, 400);
    // 不存在的会话 / 非成员会话 → 403
    let (st, _) = gsearch(
        &base,
        &a.access,
        &[("conversation_id", uuid::Uuid::new_v4().to_string())],
    )
    .await;
    assert_eq!(st, 403);
}

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

async fn admin_get_settings(base: &str, admin: &TestUser) -> Value {
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

async fn admin_put_settings(base: &str, admin: &TestUser, patch: Value) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&patch)
        .send()
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

// ===== V2 P3-2 =====
// 群公告、置顶消息、会议链接。改全局设置的用例用独立库。

fn conv_of(list: &Value, id: &str) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("conv {id} missing"))
        .clone()
}

async fn put_announcement(
    base: &str,
    u: &TestUser,
    conv: &str,
    text: Option<&str>,
) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/announcement"))
        .header("authorization", authz(&u.access))
        .json(&json!({"text": text}))
        .send()
        .await
        .unwrap()
}

async fn list_pins(base: &str, u: &TestUser, conv: &str) -> reqwest::Response {
    http()
        .get(format!("{base}/v1/conversations/{conv}/pins"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

async fn put_pin(base: &str, u: &TestUser, conv: &str, mid: &str) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/pins/{mid}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

async fn del_pin(base: &str, u: &TestUser, conv: &str, mid: &str) -> reqwest::Response {
    http()
        .delete(format!("{base}/v1/conversations/{conv}/pins/{mid}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

/// 群主设公告 → 成员目录可见且收到 invalidate；成员 403；私聊 400；清空为 null；超长 400。
#[tokio::test]
async fn p32_announcement_owner_only_and_invalidate() {
    let base = spawn().await;
    let a = register(&base, &format!("p32a_{}", uniq())).await;
    let b = register(&base, &format!("p32b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let r = put_announcement(&base, &a, &gid, Some("值班表\n周一 A")).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["text"], "值班表\n周一 A");
    assert_eq!(body["updated_by_name"], a.username);
    assert!(body["updated_at"].is_string());

    let frame = next_of(&mut sb, "invalidate").await;
    assert_eq!(frame["what"], "conversations");

    let listed = conv_of(&conv_list(&base, &b).await, &gid);
    assert_eq!(listed["announcement"]["text"], "值班表\n周一 A");
    assert_eq!(listed["announcement"]["updated_by_name"], a.username);

    let r = put_announcement(&base, &b, &gid, Some("成员改公告")).await;
    assert_eq!(r.status(), 403);

    let r = put_announcement(&base, &a, &dm, Some("私聊没有公告")).await;
    assert_eq!(r.status(), 400);

    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = put_announcement(&base, &a, &selfc, Some("助手没有公告")).await;
    assert_eq!(r.status(), 400);

    let r = put_announcement(&base, &a, &gid, None).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["text"].is_null());
    assert!(body["updated_by_name"].is_null());
    assert!(body["updated_at"].is_null());
    let listed = conv_of(&conv_list(&base, &b).await, &gid);
    assert!(listed["announcement"].is_null());

    let too_long: String = "字".repeat(2001);
    let r = put_announcement(&base, &a, &gid, Some(&too_long)).await;
    assert_eq!(r.status(), 400);
}

/// 成员置顶 → 其他成员收到 pins_updated 且列表按时间倒序；重复置顶幂等；取消后消失。
#[tokio::test]
async fn p32_pin_notifies_members_and_is_idempotent() {
    let base = spawn().await;
    let a = register(&base, &format!("p32c_{}", uniq())).await;
    let b = register(&base, &format!("p32d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let first = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "p1", "content": "第一条"}),
    )
    .await;
    let _ = next_of(&mut sb, "message").await;
    let second = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "p2", "content": "第二条"}),
    )
    .await;
    let _ = next_of(&mut sb, "message").await;
    let mid1 = first["id"].as_str().unwrap();
    let mid2 = second["id"].as_str().unwrap();

    let r = put_pin(&base, &b, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let frame = next_of(&mut sa, "pins_updated").await;
    assert_eq!(frame["conversation_id"], gid);
    let _ = next_of(&mut sb, "pins_updated").await;

    let r = put_pin(&base, &a, &gid, mid2).await;
    assert_eq!(r.status(), 204);
    let _ = next_of(&mut sb, "pins_updated").await;

    let r = list_pins(&base, &a, &gid).await;
    assert_eq!(r.status(), 200);
    let pins: Value = r.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 2);
    assert_eq!(pins[0]["message"]["id"], mid2);
    assert_eq!(pins[0]["pinned_by_name"], a.username);
    assert_eq!(pins[1]["message"]["id"], mid1);
    assert_eq!(pins[1]["pinned_by_name"], b.username);

    let r = put_pin(&base, &b, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let pins: Value = list_pins(&base, &a, &gid).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 2, "重复置顶不新增行");

    let r = del_pin(&base, &a, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let pins: Value = list_pins(&base, &b, &gid).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 1);
    assert_eq!(pins[0]["message"]["id"], mid2);

    let r = del_pin(&base, &a, &gid, mid1).await;
    assert_eq!(r.status(), 204, "取消不存在的置顶也是 204");
}

/// 不能置顶别的会话的消息；非成员不能读置顶列表。
#[tokio::test]
async fn p32_pin_rejects_foreign_message_and_non_member() {
    let base = spawn().await;
    let a = register(&base, &format!("p32e_{}", uniq())).await;
    let b = register(&base, &format!("p32f_{}", uniq())).await;
    let cuser = register(&base, &format!("p32g_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let foreign = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id": "fx", "content": "私聊内容"}),
    )
    .await;
    let mid = foreign["id"].as_str().unwrap();

    let r = put_pin(&base, &a, &gid, mid).await;
    assert!(
        r.status() == 400 || r.status() == 404,
        "跨会话置顶应 400 或 404"
    );

    let r = list_pins(&base, &cuser, &gid).await;
    assert_eq!(r.status(), 403);
    let r = put_pin(&base, &cuser, &gid, mid).await;
    assert_eq!(r.status(), 403);
}

/// 第 21 条置顶被拒绝。
#[tokio::test]
async fn p32_pin_cap_is_twenty() {
    let base = spawn().await;
    let a = register(&base, &format!("p32h_{}", uniq())).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let mut ids = Vec::new();
    for i in 0..21 {
        let m = ws_send(
            &mut sa,
            &selfc,
            json!({"client_msg_id": format!("cap{i}"), "content": format!("m{i}")}),
        )
        .await;
        ids.push(m["id"].as_str().unwrap().to_string());
    }
    for id in &ids[..20] {
        assert_eq!(put_pin(&base, &a, &selfc, id).await.status(), 204);
    }
    let r = put_pin(&base, &a, &selfc, &ids[20]).await;
    assert_eq!(r.status(), 400);
    let err = r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(err.contains("20"), "{err}");
    let pins: Value = list_pins(&base, &a, &selfc).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 20);
}

/// 置顶消息被软删除后，列表不再返回它。
#[tokio::test]
async fn p32_deleted_message_drops_from_pins() {
    let base = spawn().await;
    let a = register(&base, &format!("p32i_{}", uniq())).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &selfc,
        json!({"client_msg_id": "gone", "content": "将被删除"}),
    )
    .await;
    let mid = m["id"].as_str().unwrap();
    assert_eq!(put_pin(&base, &a, &selfc, mid).await.status(), 204);
    let pinned = next_of(&mut sa, "pins_updated").await;
    assert_eq!(pinned["conversation_id"], selfc);

    let r = http()
        .delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let dropped = next_of(&mut sa, "pins_updated").await;
    assert_eq!(dropped["conversation_id"], selfc);
    let pins: Value = list_pins(&base, &a, &selfc).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 0);
}

/// 解除好友后的私聊只读，置顶与取消置顶都被拒绝。
#[tokio::test]
async fn p32_unfriended_dm_rejects_pin() {
    let base = spawn().await;
    let a = register(&base, &format!("p32j_{}", uniq())).await;
    let b = register(&base, &format!("p32k_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id": "dm1", "content": "还是好友时"}),
    )
    .await;
    let mid = m["id"].as_str().unwrap();

    let r = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let r = put_pin(&base, &a, &dm, mid).await;
    assert_eq!(r.status(), 403);
    let r = del_pin(&base, &b, &dm, mid).await;
    assert_eq!(r.status(), 403);
}

/// meeting_base_url：默认出现在 server-info；管理员可改；非法值 400；非管理员 403；空串关闭。
#[tokio::test]
async fn p32_meeting_base_url_settings() {
    let base = spawn_named_db(&format!("chat_t_p32_{}", uniq())).await;
    let c = http();
    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["meeting_base_url"], "https://meet.jit.si");

    let admin = register(&base, &format!("p32m_{}", uniq())).await;
    let user = register(&base, &format!("p32n_{}", uniq())).await;

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&user.access))
        .json(&json!({"meeting_base_url": "https://meet.example"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    for bad in [
        "ftp://x",
        "https://meet.example/room id",
        &format!("https://{}", "a".repeat(200)),
    ] {
        let r = c
            .put(format!("{base}/v1/admin/settings"))
            .header("authorization", authz(&admin.access))
            .json(&json!({"meeting_base_url": bad}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{bad}");
    }

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": "https://meet.example/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let saved: Value = r.json().await.unwrap();
    assert_eq!(saved["meeting_base_url"], "https://meet.example");
    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["meeting_base_url"], "https://meet.example");

    // scheme 本身的 // 不能被当成路径尾斜杠剥掉
    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": "https://"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let saved: Value = r.json().await.unwrap();
    assert_eq!(saved["meeting_base_url"], "https://");

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["meeting_base_url"], "");
}

// ===== V2 P4-1 =====
// 管理后台备份/恢复。恢复会清空整库、搬空 FILES_DIR：
// 全部测试用 spawn_backup_env（独立库 + 独立文件目录），不得用共享的 spawn/spawn_named_db。

/// spawn_named_state 的备份版：独立库 + 每次测试独立的 FILES_DIR。
async fn spawn_backup_env(
    tag: &str,
) -> (String, chat_server::state::SharedState, std::path::PathBuf) {
    let db_name = format!("chat_t_p41{tag}_{}", uniq());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_db_url())
        .await
        .unwrap();
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(&db_name)
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
        None => format!("postgres://chat:chat@localhost:5432/{db_name}"),
    };
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let files_dir = std::env::temp_dir().join(format!("chat-it-p41-{tag}-{}", uniq()));
    let cfg = Config {
        database_url: url,
        jwt_secret: "test-secret".into(),
        bind: "127.0.0.1".into(),
        port: 0,
        app_dist: None,
        files_dir: files_dir.to_string_lossy().into(),
        upload_max_bytes: 25 * 1024 * 1024,
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
    (format!("http://{addr}"), state, files_dir)
}

/// 申请下载 ticket 并把备份文件完整拉回来
async fn download_backup(base: &str, token: &str, name: &str) -> Vec<u8> {
    let c = http();
    let r = c
        .post(format!("{base}/v1/admin/backups/{name}/download-ticket"))
        .header("authorization", authz(token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let url = r.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let dl = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl.status(), 200);
    let dispo = dl
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(dispo.starts_with("attachment"), "{dispo}");
    dl.bytes().await.unwrap().to_vec()
}

/// POST /admin/backups 并轮询到成品出现在列表里，返回备份名
async fn create_and_wait_backup(base: &str, token: &str) -> String {
    let c = http();
    let r = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let name = r.json::<Value>().await.unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(name.starts_with("chat-backup-") && name.ends_with(".tar.gz"));
    wait_n_backups(base, token, &name, 1).await;
    name
}

/// 轮询备份列表，直到含 name 的成品数量 >= want 或该 name 出现在列表里
async fn wait_n_backups(base: &str, token: &str, name: &str, want: usize) -> Value {
    let c = http();
    for _ in 0..120 {
        let list: Value = c
            .get(format!("{base}/v1/admin/backups"))
            .header("authorization", authz(token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let items = list["items"].as_array().unwrap();
        if items.len() >= want || items.iter().any(|i| i["name"] == name) {
            return list;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("backup {name} did not finish in time");
}

/// multipart 上传备份文件恢复；confirm=None 不带 confirm 字段
async fn restore_upload(
    base: &str,
    token: &str,
    bytes: Vec<u8>,
    confirm: Option<&str>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("backup.tar.gz".to_string())
        .mime_str("application/gzip")
        .unwrap();
    let mut form = reqwest::multipart::Form::new();
    if let Some(c) = confirm {
        form = form.text("confirm", c.to_string());
    }
    form = form.part("file", part);
    http()
        .post(format!("{base}/v1/admin/restore"))
        .header("authorization", authz(token))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// 解开 tar.gz → {条目名: 内容}（只留普通文件条目）
fn read_backup(bytes: &[u8]) -> std::collections::HashMap<String, Vec<u8>> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz);
    let mut out = std::collections::HashMap::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        if e.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let name = e.path().unwrap().to_string_lossy().replace('\\', "/");
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).unwrap();
        out.insert(name, buf);
    }
    out
}

/// 由 {条目名: 内容} 重打一个 tar.gz（测试篡改用）。
/// 名字直接写进 header 字节走 append：要的就是能塞 ../ 这类恶意条目名。
fn write_backup(entries: &std::collections::HashMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, data) in entries {
            assert!(name.len() < 100, "test entry name too long");
            let mut h = tar::Header::new_gnu();
            h.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }
    out
}

/// 原密码重新登录，返回新凭据
async fn login_again(base: &str, username: &str) -> TestUser {
    let r: Value = http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": username, "password": "secret123"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["access_token"].as_str().is_some(), "{r}");
    TestUser {
        id: r["user"]["id"].as_str().unwrap().into(),
        username: username.into(),
        access: r["access_token"].as_str().unwrap().into(),
        refresh: r["refresh_token"].as_str().unwrap().into(),
    }
}

/// 备份：造数据 → 生成 → 列表 → ticket 下载 → manifest 与文件条目齐全；
/// 生成中再次请求 409。
#[tokio::test]
async fn p41_backup_list_download_manifest() {
    let (base, _st, files_dir) = spawn_backup_env("bk").await;
    let c = http();
    let admin = register(&base, &format!("p41a_{}", uniq())).await; // 首个 = 管理员
    let bob = register(&base, &format!("p41b_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "m1", "content": "backup me"}),
    )
    .await;
    let att_body = b"attachment-bytes-p41".to_vec();
    let (st, att) = upload_att(
        &base,
        &conv,
        &admin.access,
        "doc.bin",
        "application/octet-stream",
        att_body.clone(),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    put_avatar(&base, &admin.access, png_bytes(), "image/png").await;

    // 备份 1：正常完成
    let name = create_and_wait_backup(&base, &admin.access).await;

    // 备份 2：开始后立刻再次请求 → 409
    let r = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let r2 = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 409, "生成中再次请求应 409");
    wait_n_backups(&base, &admin.access, &name, 2).await;

    // 下载回来解包校验
    let bytes = download_backup(&base, &admin.access, &name).await;
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "必须是 gzip");
    let entries = read_backup(&bytes);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["format"], 1);
    assert!(manifest["migration_version"].as_i64().unwrap() >= 1);
    assert!(manifest["files"].as_i64().unwrap() >= 2);
    assert!(manifest["files_bytes"].as_i64().unwrap() >= att_body.len() as i64);
    let tables = manifest["tables"].as_array().unwrap();
    let users = tables.iter().find(|t| t["name"] == "users").unwrap();
    assert_eq!(users["rows"], 2, "users 行数应为 2");
    assert!(tables.iter().any(|t| t["name"] == "messages"));
    // tar 里有附件文件与头像文件
    let att_key = format!("files/{att_id}");
    assert_eq!(
        entries.get(&att_key).map(|v| v.as_slice()),
        Some(att_body.as_slice()),
        "附件字节必须原样进备份"
    );
    assert!(
        entries.keys().any(|k| k.starts_with("files/avatars/")),
        "头像文件必须进备份"
    );
    assert!(entries.contains_key("db/users.csv"));
    // 物理备份文件存在
    assert!(files_dir.join(".backups").join(&name).exists());
}

/// 恢复：备份后改数据（新消息/删消息/改昵称/删附件文件）→ 上传恢复 →
/// 数据回到备份时刻、附件字节一致（PG sha256 复核）、头像可访问；
/// 恢复前的 access/refresh 全部失效，原密码可重新登录。
#[tokio::test]
async fn p41_restore_rolls_back_data_and_credentials() {
    let (base, st, files_dir) = spawn_backup_env("rs").await;
    let c = http();
    let admin = register(&base, &format!("p41c_{}", uniq())).await;
    let bob = register(&base, &format!("p41d_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "keep", "content": "keep me"}),
    )
    .await;
    let m1_id = m1["id"].as_str().unwrap().to_string();
    let att_body = b"restore-bytes-12345".to_vec();
    let (st_code, att) = upload_att(
        &base,
        &conv,
        &admin.access,
        "photo.bin",
        "application/octet-stream",
        att_body.clone(),
    )
    .await;
    assert_eq!(st_code, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    put_avatar(&base, &admin.access, png_bytes(), "image/png").await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let backup_bytes = download_backup(&base, &admin.access, &name).await;

    // 备份之后的改动：新消息 / 删旧消息 / 改昵称 / 删附件物理文件
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "after", "content": "after backup"}),
    )
    .await;
    let r = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .patch(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"display_name": "Changed Name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    std::fs::remove_file(files_dir.join(&att_id)).unwrap();
    assert!(!files_dir.join(&att_id).exists());

    // 恢复
    let r = restore_upload(&base, &admin.access, backup_bytes, Some("RESTORE")).await;
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    assert!(res["tables"].as_array().unwrap().len() >= 10, "{res}");
    assert!(res["files"].as_i64().unwrap() >= 2);

    // 恢复前的凭据全部失效
    for token in [&admin.access, &bob.access] {
        let r = c
            .get(format!("{base}/v1/users/me"))
            .header("authorization", authz(token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "恢复前 access token 应失效");
    }
    for rt in [&admin.refresh, &bob.refresh] {
        let r = c
            .post(format!("{base}/v1/auth/refresh"))
            .json(&json!({"refresh_token": rt}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "恢复前 refresh token 应失效");
    }

    // 原密码能重新登录
    let admin2 = login_again(&base, &admin.username).await;
    let _bob2 = login_again(&base, &bob.username).await;

    // 数据回到备份时刻：旧消息回来了、新消息不见了、昵称回滚
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin2.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let contents: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(contents.contains(&"keep me"), "{contents:?}");
    assert!(!contents.contains(&"after backup"), "{contents:?}");
    let me2 = me(&base, &admin2).await;
    assert_eq!(me2["display_name"], admin.username);

    // 附件：字节一致；PG sha256 复核「恢复文件 == 原始字节」
    let dl = c
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    let got = dl.bytes().await.unwrap().to_vec();
    assert_eq!(got, att_body);
    let (sha_got, sha_orig): (String, String) =
        sqlx::query_as("SELECT encode(sha256($1),'hex'), encode(sha256($2),'hex')")
            .bind(&got)
            .bind(&att_body)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(sha_got, sha_orig);

    // 头像可访问
    let av = c
        .get(format!("{base}/v1/users/{}/avatar", admin.id))
        .send()
        .await
        .unwrap();
    assert_eq!(av.status(), 200);
    assert_eq!(av.bytes().await.unwrap().as_ref(), png_bytes().as_slice());

    // 维护模式已退出：公共接口正常
    let info = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), 200);
}

/// confirm 缺失或不是 RESTORE → 400，数据不变。
#[tokio::test]
async fn p41_restore_requires_confirm() {
    let (base, _st, _files_dir) = spawn_backup_env("cf").await;
    let c = http();
    let admin = register(&base, &format!("p41e_{}", uniq())).await;
    let bob = register(&base, &format!("p41f_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "x", "content": "still here"}),
    )
    .await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    for confirm in [None, Some(""), Some("restore"), Some("RESTORE ")] {
        let r = restore_upload(&base, &admin.access, bytes.clone(), confirm).await;
        assert_eq!(r.status(), 400, "confirm={confirm:?} 应 400");
    }

    // 数据不变：消息仍在、旧 token 仍有效
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": admin.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "旧 refresh 应仍可用（未发生恢复）");
}

/// 篡改 manifest 的 migration_version → 400「版本不一致」，数据不变。
#[tokio::test]
async fn p41_restore_rejects_version_mismatch() {
    let (base, _st, _files_dir) = spawn_backup_env("vm").await;
    let c = http();
    let admin = register(&base, &format!("p41g_{}", uniq())).await;
    let bob = register(&base, &format!("p41h_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "x", "content": "still here"}),
    )
    .await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    let mut entries = read_backup(&bytes);
    let mut manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    manifest["migration_version"] = json!(manifest["migration_version"].as_i64().unwrap() + 999);
    entries.insert(
        "manifest.json".into(),
        serde_json::to_vec(&manifest).unwrap(),
    );
    let bad = write_backup(&entries);
    let r = restore_upload(&base, &admin.access, bad, Some("RESTORE")).await;
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("版本") || err.contains("不一致"), "{body}");

    // 数据不变
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
}

/// tar 里放 ../evil 或绝对路径条目 → 400，FILES_DIR 外不出现新文件。
#[tokio::test]
async fn p41_restore_rejects_unsafe_paths() {
    let (base, _st, files_dir) = spawn_backup_env("ev").await;
    let c = http();
    let admin = register(&base, &format!("p41i_{}", uniq())).await;
    let _bob = register(&base, &format!("p41j_{}", uniq())).await;
    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    for evil in ["../evil-p41.txt", "/abs-p41.txt"] {
        let mut entries = read_backup(&bytes);
        entries.insert(evil.into(), b"pwned".to_vec());
        let bad = write_backup(&entries);
        let r = restore_upload(&base, &admin.access, bad, Some("RESTORE")).await;
        assert_eq!(r.status(), 400, "evil entry {evil} 应 400");
    }
    // FILES_DIR 之外（父目录 = 系统临时目录）没有出现新文件
    assert!(!files_dir.parent().unwrap().join("evil-p41.txt").exists());
    assert!(!files_dir.join("abs-p41.txt").exists());
    // 普通接口不受影响（旧 token 仍有效）
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// 非管理员调用各接口 403；ticket 重复使用 401、过期 401；非法备份名 400。
#[tokio::test]
async fn p41_backup_admin_gate_and_ticket_rules() {
    let (base, st, _files_dir) = spawn_backup_env("gt").await;
    let c = http();
    let admin = register(&base, &format!("p41k_{}", uniq())).await;
    let user = register(&base, &format!("p41l_{}", uniq())).await;
    let name = create_and_wait_backup(&base, &admin.access).await;

    // 非管理员 → 403（restore 得带合法 multipart 才能越过 body 解析到达鉴权）
    for (method, path) in [
        ("GET", "/admin/backups".to_string()),
        ("POST", "/admin/backups".to_string()),
        ("POST", format!("/admin/backups/{name}/download-ticket")),
        ("DELETE", format!("/admin/backups/{name}")),
    ] {
        let req = match method {
            "GET" => c.get(format!("{base}/v1{path}")),
            "DELETE" => c.delete(format!("{base}/v1{path}")),
            _ => c.post(format!("{base}/v1{path}")),
        };
        let r = req
            .header("authorization", authz(&user.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{method} {path}");
    }
    let r = restore_upload(&base, &user.access, b"x".to_vec(), Some("RESTORE")).await;
    assert_eq!(r.status(), 403, "POST /admin/restore");

    // 非法备份名 → 400（防路径穿越）
    for bad in [
        "x",
        "chat-backup-2026.tar.gz",
        "foo",
        "chat-backup-20260101T000000.tar.gz",
        "chat-backup-20260101T000000Z.tar.gx",
    ] {
        let r = c
            .post(format!("{base}/v1/admin/backups/{bad}/download-ticket"))
            .header("authorization", authz(&admin.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "ticket {bad}");
        let r = c
            .delete(format!("{base}/v1/admin/backups/{bad}"))
            .header("authorization", authz(&admin.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "DELETE {bad}");
    }

    // ticket 一次性：第二次复用 → 401
    let r = c
        .post(format!("{base}/v1/admin/backups/{name}/download-ticket"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    let url = r.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let dl = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl.status(), 200);
    let dl2 = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl2.status(), 401, "ticket 复用应 401");

    // 过期 ticket → 401（直接插入一张已超期的授权）
    let admin_uid = uuid::Uuid::parse_str(&admin.id).unwrap();
    st.tickets.insert(
        "deadbeef".into(),
        chat_server::state::TicketGrant {
            uid: admin_uid,
            session_version: 0,
            sid: None,
            issued: std::time::Instant::now() - std::time::Duration::from_secs(120),
        },
    );
    let dl3 = c
        .get(format!(
            "{base}/v1/admin/backups/{name}/download?t=deadbeef"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl3.status(), 401, "过期 ticket 应 401");

    // 普通用户的 WS ticket 不能用来下载备份（消费时仍查管理员身份）
    let t: Value = c
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&user.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ws_ticket = t["ticket"].as_str().unwrap();
    let dl4 = c
        .get(format!(
            "{base}/v1/admin/backups/{name}/download?t={ws_ticket}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl4.status(), 401, "非管理员 ticket 应 401");
}

/// 维护模式：maintenance=true 时所有 /v1 请求 503（含 WS 升级）；复位后恢复。
#[tokio::test]
async fn p41_maintenance_blocks_api() {
    let (base, st, _files_dir) = spawn_backup_env("mt").await;
    let c = http();
    let u = register(&base, &format!("p41m_{}", uniq())).await;

    st.maintenance
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for path in ["/users/me", "/conversations", "/ws/ticket"] {
        let r = c
            .get(format!("{base}/v1{path}"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503, "{path} 应 503");
    }
    // WS 升级同样被拒
    let r = c
        .get(format!("{base}/v1/ws?ticket=x"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);

    st.maintenance
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}
