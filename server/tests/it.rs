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
