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
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:15432/postgres".into())
}

fn test_db_url() -> String {
    let base = base_db_url();
    // 把库名替换为 chat_test
    match base.rfind('/') {
        Some(i) => format!("{}/chat_test", &base[..i]),
        None => "postgres://chat:chat@localhost:15432/chat_test".into(),
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
        None => format!("postgres://chat:chat@localhost:15432/{db_name}"),
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
    // 文件名 {uid}-{版本}-{随机段}.{扩展名}
    let dir = files_dir.join("avatars");
    let has = |prefix: String, ext: &str| {
        std::fs::read_dir(&dir).unwrap().flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&prefix) && n.ends_with(ext)
        })
    };
    let mut gone = false;
    for _ in 0..50 {
        if !has(format!("{}-1-", u.id), ".png") {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(gone, "旧头像文件应在更换后被删除");
    assert!(has(format!("{}-2-", u.id), ".jpg"));
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
        None => format!("postgres://chat:chat@localhost:15432/{db_name}"),
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

// ===== V2 审查修复（gpt-6-astar） =====

/// 1. 文件替换失败（新档里有 .backups 条目，挪不到位）：数据库不提交、原文件全部挪回，
///    .restore-old / .restore-new 不留；.restore-old 有遗留原文件时拒绝恢复且不碰它。
#[tokio::test]
async fn review_restore_swap_failure_keeps_files_and_db() {
    let (base, _st, files_dir) = spawn_backup_env("sf").await;
    let c = http();
    let admin = register(&base, &format!("rvsf_{}", uniq())).await;
    std::fs::write(files_dir.join("keep.bin"), b"before").unwrap();
    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    // 备份后的改动：恢复失败时必须原样保留
    let r = c
        .patch(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"display_name": "changed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    std::fs::write(files_dir.join("after.bin"), b"after").unwrap();

    let mut entries = read_backup(&bytes);
    entries.insert("files/.backups/evil".into(), b"x".to_vec());
    let r = restore_upload(
        &base,
        &admin.access,
        write_backup(&entries),
        Some("RESTORE"),
    )
    .await;
    assert_eq!(r.status(), 500, "{}", r.text().await.unwrap());

    assert_eq!(
        std::fs::read(files_dir.join("keep.bin")).unwrap(),
        b"before"
    );
    assert_eq!(
        std::fs::read(files_dir.join("after.bin")).unwrap(),
        b"after"
    );
    assert!(!files_dir.join(".restore-old").exists());
    assert!(!files_dir.join(".restore-new").exists());
    assert!(files_dir.join(".backups").join(&name).exists());
    // 库没回滚到备份时刻、凭据没作废
    let me: Value = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["display_name"], "changed");

    // 遗留的 .restore-old（上次撤回失败的原文件）：拒绝恢复，文件不动
    let old = files_dir.join(".restore-old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("orphan.bin"), b"orig").unwrap();
    let r = restore_upload(&base, &admin.access, bytes, Some("RESTORE")).await;
    assert_eq!(r.status(), 500);
    assert_eq!(std::fs::read(old.join("orphan.bin")).unwrap(), b"orig");
    assert_eq!(
        std::fs::read(files_dir.join("after.bin")).unwrap(),
        b"after"
    );
}

/// 4. 空库并发注册只能产生一个管理员
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_register_race_single_admin() {
    let (base, st) = spawn_named_state(&format!("chat_it_rvreg_{}", uniq())).await;
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let base = base.clone();
            tokio::spawn(async move {
                http()
                    .post(format!("{base}/v1/auth/register"))
                    .json(
                        &json!({"username": format!("rv{i}_{}", uniq()), "password": "secret123"}),
                    )
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin'")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(admins, 1);
}

/// 5. 登出撤销会话后，该会话名下的 WS 立即收到 force_logout
#[tokio::test]
async fn review_logout_kicks_session_ws() {
    let base = spawn().await;
    let u = register(&base, &format!("rvlo_{}", uniq())).await;
    let dev = login_device(&base, &u.username, "secret123", "A").await;
    let (mut s, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev).await)
        .await
        .unwrap();
    next_of(&mut s, "presence_init").await;
    let r = http()
        .post(format!("{base}/v1/auth/logout"))
        .json(&json!({"refresh_token": dev.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let v = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_of(&mut s, "force_logout"),
    )
    .await
    .expect("登出后该会话的 WS 应被踢");
    assert_eq!(v["reason"], "logout");
}

/// 6. 永不读取的连接：发送缓冲塞满后写超时/出站队列满即断开（心跳检查远在窗口外，不能靠它）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_ws_stuck_reader_is_dropped() {
    use std::sync::atomic::Ordering::Relaxed;
    let (base, st) = spawn_named_state(&format!("chat_it_rvws_{}", uniq())).await;
    st.ws_ping_interval_ms.store(60_000, Relaxed);
    st.ws_idle_timeout_ms.store(1000, Relaxed);
    let z = register(&base, &format!("rvws_{}", uniq())).await;
    let zid = uuid::Uuid::parse_str(&z.id).unwrap();
    let _zombie = tokio_tungstenite::connect_async(ws_url(&base, &z).await)
        .await
        .unwrap();
    for _ in 0..100 {
        if st.is_online(zid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(st.is_online(zid));

    let big = "x".repeat(64 * 1024);
    let t0 = std::time::Instant::now();
    let mut dropped = false;
    while t0.elapsed() < std::time::Duration::from_secs(8) {
        st.send_to_user(zid, &big);
        if !st.is_online(zid) {
            dropped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(dropped, "读不动的连接应被断开");
}

/// 8. 并发换头像：输家只删自己的文件，赢家的头像始终可取
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_avatar_concurrent_upload_keeps_winner_file() {
    let (base, _files_dir) = spawn_avatar().await;
    let u = register(&base, &format!("rvav_{}", uniq())).await;
    for round in 0..8 {
        let (r1, r2) = tokio::join!(
            put_avatar(&base, &u.access, png_bytes(), "image/png"),
            put_avatar(&base, &u.access, png_bytes(), "image/png"),
        );
        for r in [r1.status(), r2.status()] {
            assert!(r == 200 || r == 409, "round {round}: {r}");
        }
        let me: Value = http()
            .get(format!("{base}/v1/users/me"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let r = http()
            .get(format!(
                "{base}/v1/users/{}/avatar?v={}",
                u.id, me["avatar_version"]
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "round {round}: 赢家的头像文件被删了");
    }
}

/// 自己发的消息不算未读：发言即把本人已读推进到这条，并给本人其他设备推 read_sync
#[tokio::test]
async fn review_own_messages_not_unread() {
    let base = spawn().await;
    let a = register(&base, &format!("rvun_{}", uniq())).await;
    let b = register(&base, &format!("rvuo_{}", uniq())).await;
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
    // A 的第二台设备
    let a2 = login_device(&base, &a.username, "secret123", "A2").await;
    let (mut sa2, _) = tokio_tungstenite::connect_async(ws_url(&base, &a2).await)
        .await
        .unwrap();
    next_of(&mut sa2, "presence_init").await;

    ws_send(
        &mut sb,
        &conv,
        json!({"client_msg_id": "b1", "content": "from b"}),
    )
    .await;
    for i in 0..3 {
        ws_send(
            &mut sa,
            &conv,
            json!({"client_msg_id": format!("a{i}"), "content": "from a"}),
        )
        .await;
    }
    let unread = |tok: String| {
        let base = base.clone();
        let conv = conv.clone();
        async move {
            let list: Value = http()
                .get(format!("{base}/v1/conversations"))
                .header("authorization", authz(&tok))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            list.as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == conv.as_str())
                .unwrap()["unread_count"]
                .as_i64()
                .unwrap()
        }
    };
    assert_eq!(
        unread(a.access.clone()).await,
        0,
        "自己发言后本会话不应有未读"
    );
    assert_eq!(unread(b.access.clone()).await, 3, "对方的未读不受影响");
    let rs = next_of(&mut sa2, "read_sync").await;
    assert_eq!(rs["conversation_id"], conv.as_str());
}

// ===== Agent A01 =====
// 端到端跑真 agent-host 库 + node tests/fake-acp-agent.mjs（需 PATH 有 node）。

use std::time::Duration as A01Dur;

/// 用账号 access token 换 WS 票的 TicketSource（宿主连接也走同一票口）
struct AgentTickets {
    base: String,
    access: String,
}

impl agent_host::TicketSource for AgentTickets {
    fn ticket(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>> {
        let (base, access) = (self.base.clone(), self.access.clone());
        Box::pin(async move {
            let v: Value = http()
                .get(format!("{base}/v1/ws/ticket"))
                .header("authorization", authz(&access))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            v["ticket"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "no ticket".into())
        })
    }
}

/// 假 ACP agent 的绝对路径（node 第一个参数）
fn fake_acp_args() -> Vec<String> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-host/tests/fake-acp-agent.mjs");
    assert!(p.exists(), "fake agent 脚本不存在: {p:?}");
    vec![p.to_string_lossy().to_string()]
}

/// 起一个真宿主连接，等到 host_ready。返回 (handle, host_id)。
async fn spawn_host(
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
async fn create_agent(base: &str, u: &TestUser, host_id: &str, name: &str) -> (u16, Value) {
    create_agent_with(base, u, host_id, name, fake_acp_args()).await
}

async fn create_agent_with(
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
async fn until_frame<S>(s: &mut S, want: &str, timeout: A01Dur) -> Vec<Value>
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
async fn frames_for<S>(s: &mut S, dur: A01Dur) -> Vec<Value>
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

async fn send_ws<S>(s: &mut S, conv: &str, cmid: &str, content: &str)
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

/// REST 授权与校验：别人的宿主/Agent 列表不可见、按 id 建 Agent 404、
/// 名字规则 400、大小写不敏感重名 409。
#[tokio::test]
async fn agent_rest_scope_and_validation() {
    let base = spawn_named_db(&format!("chat_t_a01rest_{}", uniq())).await;
    let a = register(&base, &format!("a01a_{}", uniq())).await;
    let b = register(&base, &format!("a01b_{}", uniq())).await;
    let c = http();

    // 未连宿主前 /agent-hosts 为空
    let hosts: Value = c
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts.as_array().unwrap().len(), 0);

    // 不存在的宿主 → 404
    let (st, _) = create_agent(&base, &a, &uuid::Uuid::new_v4().to_string(), "myagent").await;
    assert_eq!(st, 404);

    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;

    // 别人的宿主 id 建 Agent → 404（不泄露存在性）
    let (st, _) = create_agent(&base, &b, &host_id, "steal").await;
    assert_eq!(st, 404);

    // 名字校验：空 / 过长 / 非白名单字符 / 首字符 '-'（1–32 字符合法，"na" 是合法名）
    for bad in [
        "",
        "-bad",
        "bad name",
        "中文名",
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    ] {
        let (st, body) = create_agent(&base, &a, &host_id, bad).await;
        assert_eq!(st, 400, "bad name {bad:?} 应 400: {body}");
        assert_eq!(body["error"], "bad_name");
    }
    // runtime 校验：空 command、>50 args
    let r = c
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name":"ok1","host_id":host_id,"runtime":{"command":"","args":[]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = c
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({"name":"ok2","host_id":host_id,
                      "runtime":{"command":"node","args":vec!["x";51]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 正常创建 + 大小写不敏感重名 409
    let (st, agent) = create_agent(&base, &a, &host_id, "myAgent").await;
    assert_eq!(st, 200, "{agent}");
    assert_eq!(agent["name"], "myAgent");
    assert!(agent["conversation_id"].as_str().is_some());
    assert_eq!(agent["host_online"], true);
    let (st, body) = create_agent(&base, &a, &host_id, "MYAGENT").await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "name_taken");

    // 可见性：b 的 hosts/agents 列表都为空；a 的都有一条
    for u in [&a, &b] {
        let hosts: Value = c
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let agents: Value = c
            .get(format!("{base}/v1/agents"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if u.id == a.id {
            assert_eq!(hosts.as_array().unwrap().len(), 1);
            assert_eq!(hosts[0]["online"], true);
            assert_eq!(agents.as_array().unwrap().len(), 1);
        } else {
            assert!(hosts.as_array().unwrap().is_empty());
            assert!(agents.as_array().unwrap().is_empty());
        }
    }
    ha.shutdown().await;
}

/// 专属会话出现在会话列表、带 agent 字段；非成员读历史 403；
/// 拉人/退出/改名/转让/解散/建投票全部 400。
#[tokio::test]
async fn agent_conv_restrictions() {
    let base = spawn_named_db(&format!("chat_t_a01conv_{}", uniq())).await;
    let a = register(&base, &format!("a01c_{}", uniq())).await;
    let b = register(&base, &format!("a01d_{}", uniq())).await;
    // 先成为好友：拉人入口的好友校验通过后才能命中类型守卫 400（同 p24 self 用例）
    befriend(&base, &a, &b).await;
    let c = http();

    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "helper").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();

    // 会话列表：kind='agent' + agent {id,name,host_online}
    let convs: Value = c
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let conv = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "agent")
        .expect("agent conv in list");
    assert_eq!(conv["agent"]["id"], agent_id.as_str());
    assert_eq!(conv["agent"]["name"], "helper");
    assert_eq!(conv["agent"]["host_online"], true);
    assert!(conv["name"].is_null());
    let conv_id = conv["id"].as_str().unwrap().to_string();

    // 非成员读历史 → 403
    let r = c
        .get(format!("{base}/v1/conversations/{conv_id}/messages"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 全部结构性操作对 agent 会话拒绝（400）
    let cases: Vec<(String, String, Value)> = vec![
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/members"),
            json!({"user_ids": [b.id]}),
        ),
        (
            "DELETE".into(),
            format!("{base}/v1/conversations/{conv_id}/members/me"),
            json!(null),
        ),
        (
            "PATCH".into(),
            format!("{base}/v1/conversations/{conv_id}"),
            json!({"name": "x"}),
        ),
        (
            "DELETE".into(),
            format!("{base}/v1/conversations/{conv_id}"),
            json!(null),
        ),
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/transfer"),
            json!({"user_id": b.id}),
        ),
        (
            "POST".into(),
            format!("{base}/v1/conversations/{conv_id}/polls"),
            json!({"question": "q?", "options": ["a", "b"]}),
        ),
    ];
    for (method, url, body) in cases {
        let req = match method.as_str() {
            "POST" => c.post(url),
            "PATCH" => c.patch(url),
            _ => c.delete(url),
        };
        let req = req.header("authorization", authz(&a.access));
        let r = if body.is_null() {
            req.send().await.unwrap()
        } else {
            req.json(&body).send().await.unwrap()
        };
        assert_eq!(r.status(), 400, "{method} {body} 应 400");
    }
    // 踢人（唯一成员是 owner，对他人操作也 400）
    let r = c
        .delete(format!(
            "{base}/v1/conversations/{conv_id}/members/{}",
            b.id
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    ha.shutdown().await;
}

/// 宿主生命周期：agent_host_status 上下线广播、GET /agent-hosts 在线态、
/// 宿主连接不产生好友 presence、同 install_id 重连复用 host_id、
/// 别的用户同 install_id 是另一条记录。
#[tokio::test]
async fn agent_host_lifecycle() {
    let base = spawn_named_db(&format!("chat_t_a01host_{}", uniq())).await;
    let a = register(&base, &format!("a01e_{}", uniq())).await;
    let b = register(&base, &format!("a01f_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let c = http();

    // b 先连、a 后连：先把 a 上线广播给 b 的 presence 帧收掉，后面的静默窗口才只反映宿主连接
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    until_frame(&mut sb, "presence", A01Dur::from_secs(5)).await;

    let install = uuid::Uuid::new_v4();
    let (h1, host_id) = spawn_host(&base, &a, install).await;

    // a 收 agent_host_status online=true
    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let st = frames
        .iter()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(st["host_id"], host_id.as_str());
    assert_eq!(st["online"], true);

    // GET /agent-hosts → online
    let hosts: Value = c
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(hosts[0]["online"], true);

    // 宿主连接不产生 presence：1s 窗口内 b 不应收到 user_id=a 的 presence 帧
    let frames = frames_for(&mut sb, A01Dur::from_secs(1)).await;
    assert!(
        !frames
            .iter()
            .any(|f| f["type"] == "presence" && f["user_id"] == a.id.as_str()),
        "宿主连接不应向好友广播 presence: {frames:?}"
    );

    // 断开 → online=false
    h1.shutdown().await;
    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let st = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(st["online"], false);

    // 同 install_id 重连 → 同一 host_id
    let (h2, host_id2) = spawn_host(&base, &a, install).await;
    assert_eq!(host_id2, host_id);
    // 另一用户用同一 install_id → 不同 host_id（(user,install) 唯一下各管各的）
    let (h3, host_id3) = spawn_host(&base, &b, install).await;
    assert_ne!(host_id3, host_id);
    h2.shutdown().await;
    h3.shutdown().await;
}

/// 主路径：echo → running → Agent 消息（归属正确）→ succeeded；
/// 未读 +1；Agent 消息不触发主人的 read_sync。
#[tokio::test]
async fn agent_invoke_echo_end_to_end() {
    let base = spawn_named_db(&format!("chat_t_a01echo_{}", uniq())).await;
    let a = register(&base, &format!("a01g_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "echoer").await;
    assert_eq!(st, 200);
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "e1", "echo:hello world").await;

    // 先等 running（调用开始即推帧）
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    assert_eq!(inv["invocation"]["conversation_id"], conv.as_str());
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();

    // 再等终态；期间的 Agent 消息帧一并收集
    let mut frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    while !frames
        .iter()
        .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
    }
    // 多留 300ms 收尾巴帧
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(300)).await);

    let final_inv = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id.as_str())
        .expect("succeeded frame");
    assert_eq!(final_inv["invocation"]["status"], "succeeded");

    // Agent 消息：sender_id=主人、agent.id/name 正确、正文 hello world
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 消息");
    assert_eq!(agent_msg["message"]["agent"]["id"], agent_id.as_str());
    assert_eq!(agent_msg["message"]["agent"]["name"], "echoer");
    assert_eq!(agent_msg["message"]["sender_id"], a.id.as_str());
    assert_eq!(agent_msg["message"]["content"], "hello world");
    let agent_seq = agent_msg["message"]["seq"].as_i64().unwrap();

    // Agent 消息不给主人发 read_sync（read_sync 只覆盖主人自己的触发消息）
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "read_sync"
                && f["conversation_id"] == conv.as_str()
                && f["seq"].as_i64().unwrap() >= agent_seq
        }),
        "Agent 消息不应带 read_sync: {frames:?}"
    );

    // 未读 = 1（主人自己的触发消息不算未读，Agent 回复算）
    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv.as_str())
        .unwrap();
    assert_eq!(c["unread_count"].as_i64().unwrap(), 1);
    ha.shutdown().await;
}

/// fail / crash → failed + 非空 error；不产生 Agent 消息。
#[tokio::test]
async fn agent_invoke_fail_and_crash() {
    let base = spawn_named_db(&format!("chat_t_a01fail_{}", uniq())).await;
    let a = register(&base, &format!("a01h_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "x").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    for (i, prompt) in ["fail", "crash"].iter().enumerate() {
        send_ws(&mut sa, &conv, &format!("f{i}"), prompt).await;
        // 等到该调用进终态（running 帧先，failed 帧后）
        let mut done = false;
        let mut collected = Vec::new();
        for _ in 0..2 {
            collected.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
            if collected
                .iter()
                .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "failed")
            {
                done = true;
                break;
            }
        }
        assert!(done, "{prompt} 应 failed: {collected:?}");
        let failed = collected
            .iter()
            .find(|f| f["invocation"]["status"] == "failed")
            .unwrap();
        assert!(
            failed["invocation"]["error"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "error 不应为空"
        );
    }
    // 没有产生任何 Agent 消息
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["agent"].is_object()),
        "失败调用不应产生 Agent 消息"
    );
    ha.shutdown().await;
}

/// 宿主离线时发消息：排队（A04），不是立即 failed。
#[tokio::test]
async fn agent_invoke_host_offline() {
    let base = spawn_named_db(&format!("chat_t_a01off_{}", uniq())).await;
    let a = register(&base, &format!("a01i_{}", uniq())).await;
    // 宿主连过一次再断：留下 host 行 + 离线态
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    ha.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "off").await;
    assert_eq!(agent["host_online"], false);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "o1", "echo:hi").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "queued");
    assert!(inv["invocation"]["error"].is_null());
    assert!(inv["invocation"]["started_at"].is_null());
}

/// 调用跑到一半宿主断开 → failed + host_disconnected。
#[tokio::test]
async fn agent_invoke_host_disconnect_mid_run() {
    let base = spawn_named_db(&format!("chat_t_a01dc_{}", uniq())).await;
    let a = register(&base, &format!("a01j_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "sleeper").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:10000").await;
    // 先等到 running
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();

    ha.shutdown().await; // 宿主进程下线
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await;
    let done = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id.as_str())
        .expect("断线后应有终态帧");
    assert_eq!(done["invocation"]["status"], "failed");
    assert_eq!(done["invocation"]["error"], "host_disconnected");
}

/// long:9000 → 三条按序 Agent 消息，拼接等于原输出。
#[tokio::test]
async fn agent_invoke_long_output_split() {
    let base = spawn_named_db(&format!("chat_t_a01long_{}", uniq())).await;
    let a = register(&base, &format!("a01k_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "longer").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "l1", "long:9000").await;
    // 收到 succeeded 为止的全部帧
    let mut frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(20)).await;
    while !frames
        .iter()
        .any(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(20)).await);
    }
    let mut agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    agent_msgs.sort_by_key(|f| f["message"]["seq"].as_i64().unwrap());
    assert_eq!(agent_msgs.len(), 3, "9000 字符应拆成 3 条: {frames:?}");
    let joined: String = agent_msgs
        .iter()
        .map(|f| f["message"]["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined.len(), 9000);
    assert!(joined.chars().all(|ch| ch == 'x'));
    ha.shutdown().await;
}

/// 同一 Agent 多次调用并发执行：两个 sleep:1500 总耗时约 1.5s 而非 3s。
#[tokio::test]
async fn agent_invoke_concurrent() {
    let base = spawn_named_db(&format!("chat_t_a01par_{}", uniq())).await;
    let a = register(&base, &format!("a01m_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "par").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    // 各睡 3s：并行约 3s + 起进程开销，串行至少 6s；阈值留足余量，机器繁忙时也不误报
    send_ws(&mut sa, &conv, "p1", "sleep:3000").await;
    send_ws(&mut sa, &conv, "p2", "sleep:3000").await;

    let mut frames: Vec<Value> = Vec::new();
    while frames
        .iter()
        .filter(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .count()
        < 2
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(10)).await);
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < A01Dur::from_millis(5500),
        "并发两次 3s 调用应约 3-4s 完成，实际 {elapsed:?}（串行会 6s+）"
    );
    ha.shutdown().await;
}

/// 调用历史：GET /conversations/{id}/agent-invocations 返回终态；非成员 403。
#[tokio::test]
async fn agent_invocation_history() {
    let base = spawn_named_db(&format!("chat_t_a01hist_{}", uniq())).await;
    let a = register(&base, &format!("a01n_{}", uniq())).await;
    let b = register(&base, &format!("a01o_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "hist").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    send_ws(&mut sa, &conv, "h1", "echo:one").await;
    send_ws(&mut sa, &conv, "h2", "fail").await;
    // 等两条终态
    let mut frames: Vec<Value> = Vec::new();
    while frames
        .iter()
        .filter(|f| {
            f["type"] == "agent_invocation"
                && (f["invocation"]["status"] == "succeeded"
                    || f["invocation"]["status"] == "failed")
        })
        .count()
        < 2
    {
        frames.extend(until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await);
    }

    let rows: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let statuses: Vec<&str> = rows.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert!(statuses.contains(&"succeeded"));
    assert!(statuses.contains(&"failed"));

    // 非成员 → 403
    let r = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    ha.shutdown().await;
}

// ===== Agent 设置层（第 2 批前预重构） =====

#[tokio::test]
async fn agent_settings_defaults_and_overrides() {
    let base = spawn_named_db(&format!("chat_t_agset_{}", uniq())).await;
    let a = register(&base, &format!("agseta_{}", uniq())).await;
    let b = register(&base, &format!("agsetb_{}", uniq())).await;
    let c = http();
    let settings = |u: &TestUser| {
        c.get(format!("{base}/v1/users/me/agent-settings"))
            .header("authorization", authz(&u.access))
    };

    // 没设置过 → 默认值
    let s: Value = settings(&a).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["output_mode"], "stream");
    assert_eq!(s["queue_timeout_secs"], 86400);

    // 部分更新：只改输出方式；非法值 400
    let put = |u: &TestUser, body: Value| {
        c.put(format!("{base}/v1/users/me/agent-settings"))
            .header("authorization", authz(&u.access))
            .json(&body)
            .send()
    };
    let r = put(&a, json!({"output_mode": "status"})).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let s: Value = settings(&a).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["output_mode"], "status");
    assert_eq!(s["queue_timeout_secs"], 86400);
    assert_eq!(
        put(&a, json!({"output_mode": "loud"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    assert_eq!(
        put(&a, json!({"queue_timeout_secs": -1}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    assert_eq!(
        put(&a, json!({"queue_timeout_secs": 0}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    // 别人的设置不受影响
    let sb: Value = settings(&b).send().await.unwrap().json().await.unwrap();
    assert_eq!(sb["output_mode"], "stream");

    // Agent 覆盖：新建时为 null；PATCH 设值、显式 null 清除、缺省字段不动
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "setter").await;
    assert!(agent["output_mode"].is_null() && agent["queue_timeout_secs"].is_null());
    let id = agent["id"].as_str().unwrap();
    let patch = |u: &TestUser, body: Value| {
        c.patch(format!("{base}/v1/agents/{id}"))
            .header("authorization", authz(&u.access))
            .json(&body)
            .send()
    };
    let v: Value = patch(
        &a,
        json!({"output_mode": "steps", "queue_timeout_secs": 600}),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(v["output_mode"], "steps");
    assert_eq!(v["queue_timeout_secs"], 600);
    let v: Value = patch(&a, json!({"output_mode": null}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["output_mode"].is_null());
    assert_eq!(v["queue_timeout_secs"], 600);
    assert_eq!(
        patch(&a, json!({"output_mode": "x"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    // 别人的 Agent → 404
    assert_eq!(
        patch(&b, json!({"output_mode": "steps"}))
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );
    // 列表里带覆盖值
    let list: Value = c
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["queue_timeout_secs"], 600);
    h.shutdown().await;
}

// ===== Agent A02 =====
// 同一 AgentConversation 接着聊：运行时会话 id 由成功的 finish 记住，重置后不再带上。

struct A02Boot {
    base: String,
    user: TestUser,
    host: agent_host::HostHandle,
    host_id: String,
}

async fn a02_boot(tag: &str) -> A02Boot {
    let base = spawn_named_db(&format!("chat_t_a02{tag}_{}", uniq())).await;
    let user = register(&base, &format!("a02{tag}_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    A02Boot {
        base,
        user,
        host,
        host_id,
    }
}

/// 带 `--caps=` 的变体。不改 create_agent 的签名。
async fn create_agent_caps(
    base: &str,
    u: &TestUser,
    host_id: &str,
    name: &str,
    caps: &str,
) -> (u16, Value) {
    let mut args = fake_acp_args();
    args.push(format!("--caps={caps}"));
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

async fn a02_agent(b: &A02Boot, name: &str, caps: Option<&str>) -> String {
    let (st, agent) = match caps {
        Some(c) => create_agent_caps(&b.base, &b.user, &b.host_id, name, c).await,
        None => create_agent(&b.base, &b.user, &b.host_id, name).await,
    };
    assert_eq!(st, 200, "create agent {name}: {agent}");
    agent["conversation_id"].as_str().unwrap().to_string()
}

async fn a02_conn(
    base: &str,
    u: &TestUser,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap()
        .0
}

/// 等到这次调用进入终态，返回 (status, Agent 回复拼接)。
async fn await_agent_turn<S>(sock: &mut S) -> (String, String)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut frames: Vec<Value> = Vec::new();
    let mut status = String::new();
    for _ in 0..6 {
        frames.extend(until_frame(sock, "agent_invocation", A01Dur::from_secs(20)).await);
        if let Some(st) = frames.iter().rev().find_map(|f| {
            if f["type"] != "agent_invocation" {
                return None;
            }
            let s = f["invocation"]["status"].as_str()?;
            if matches!(s, "succeeded" | "failed" | "cancelled" | "timed_out") {
                Some(s.to_string())
            } else {
                None
            }
        }) {
            status = st;
            break;
        }
    }
    assert!(!status.is_empty(), "调用未进入终态: {frames:?}");
    let reply: String = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .map(|f| f["message"]["content"].as_str().unwrap_or(""))
        .collect();
    (status, reply)
}

async fn a02_say<S>(sock: &mut S, conv: &str, cmid: &str, content: &str) -> (String, String)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMsg>
        + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    send_ws(sock, conv, cmid, content).await;
    await_agent_turn(sock).await
}

async fn reset_context(base: &str, u: &TestUser, conv: &str) -> u16 {
    http()
        .post(format!(
            "{base}/v1/conversations/{conv}/agent-context/reset"
        ))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 1. 同一专属会话先 echo 再 recall，回复带上之前的指令。
#[tokio::test]
async fn agent_context_continues_in_same_conversation() {
    let b = a02_boot("go").await;
    let conv = a02_agent(&b, "cont", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "one");
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:one");
    b.host.shutdown().await;
}

/// 2. 重置后再 recall 是空会话；非成员被拒；私聊可重置、文件传输助手 400。
#[tokio::test]
async fn agent_context_reset_clears_and_rejects() {
    let b = a02_boot("rs").await;
    let other = register(&b.base, &format!("a02rsb_{}", uniq())).await;
    let conv = a02_agent(&b, "rst", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));

    assert_eq!(reset_context(&b.base, &b.user, &conv).await, 204);
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "(empty)");

    assert_eq!(reset_context(&b.base, &other, &conv).await, 403);
    befriend(&b.base, &b.user, &other).await;
    let dm = dm_conv(&b.base, &b.user).await;
    // 私聊 / 群也能开新话题（清请求者自己的 Agent 在这里的上下文）；文件传输助手仍不支持
    assert_eq!(reset_context(&b.base, &b.user, &dm).await, 204);
    let selfc = self_conv(&b.base, &b.user).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reset_context(&b.base, &b.user, &selfc).await, 400);
    b.host.shutdown().await;
}

/// 3. load 能力同样能续上，且回放的旧回复正文不混进这次回复。
#[tokio::test]
async fn agent_context_load_drops_replay() {
    let b = a02_boot("ld").await;
    let conv = a02_agent(&b, "loader", Some("load")).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    // 回放出来的旧回复是 "one"；混进来会变成 "oneecho:one"
    assert_eq!(reply, "echo:one");
    b.host.shutdown().await;
}

/// 4. 不声明恢复能力时退回新会话，第二次调用仍然成功，recall 为空。
#[tokio::test]
async fn agent_context_without_caps_starts_fresh() {
    let b = a02_boot("no").await;
    let conv = a02_agent(&b, "fresh", Some("none")).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "(empty)");
    b.host.shutdown().await;
}

/// 5. 两个 Agent 的上下文互不影响。
#[tokio::test]
async fn agent_context_isolated_per_agent() {
    let b = a02_boot("iso").await;
    let c1 = a02_agent(&b, "iso1", None).await;
    let c2 = a02_agent(&b, "iso2", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &c1, "a1", "echo:alpha").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "alpha"));
    let (st, reply) = a02_say(&mut sa, &c2, "b1", "echo:beta").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "beta"));
    let (st, reply) = a02_say(&mut sa, &c1, "a2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:alpha");
    let (st, reply) = a02_say(&mut sa, &c2, "b2", "recall").await;
    assert_eq!(st, "succeeded");
    assert_eq!(reply, "echo:beta");
    b.host.shutdown().await;
}

/// 6. 失败的调用不覆盖已记住的运行时会话，之后 recall 仍能接上。
#[tokio::test]
async fn agent_context_failed_invocation_keeps_session() {
    let b = a02_boot("fl").await;
    let conv = a02_agent(&b, "keep", None).await;
    let mut sa = a02_conn(&b.base, &b.user).await;
    let (st, reply) = a02_say(&mut sa, &conv, "e1", "echo:one").await;
    assert_eq!((st.as_str(), reply.as_str()), ("succeeded", "one"));
    let (st, reply) = a02_say(&mut sa, &conv, "e2", "fail").await;
    assert_eq!(st, "failed");
    assert!(reply.is_empty(), "失败调用不应产生回复: {reply}");
    let (st, reply) = a02_say(&mut sa, &conv, "e3", "recall").await;
    assert_eq!(st, "succeeded");
    assert!(reply.contains("echo:one"), "失败后应仍接上旧会话: {reply}");
    assert_ne!(reply, "(empty)");
    b.host.shutdown().await;
}

// ===== Agent A06 =====

async fn patch_agent(base: &str, u: &TestUser, id: &str, body: Value) -> reqwest::Response {
    http()
        .patch(format!("{base}/v1/agents/{id}"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn list_my_agents(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn agent_named<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("列表里没有 {name}: {list}"))
}

/// PATCH 各字段回读可见；非法值 400；host_id 400；重名 409；别人 404。
#[tokio::test]
async fn a06_patch_fields_and_rejections() {
    let base = spawn_named_db(&format!("chat_t_a06p_{}", uniq())).await;
    let a = register(&base, &format!("a06a_{}", uniq())).await;
    let b = register(&base, &format!("a06b_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, one) = create_agent(&base, &a, &host_id, "cfgA").await;
    let (_, two) = create_agent(&base, &a, &host_id, "cfgB").await;
    let id = one["id"].as_str().unwrap();

    let body = json!({
        "name": "cfgRenamed",
        "runtime": {"command": "node", "args": ["--one", "--two"]},
        "cwd": "C:/work",
        "extra_dirs": ["C:/extra", "D:/other"],
        "model": "smart",
        "mode": "code",
        "instructions": "先看 README",
        "env": {"FAKE_ECHO": "sekrit", "OTHER": "1"},
        "output_mode": "status",
        "queue_timeout_secs": 600,
    });
    let patched: Value = patch_agent(&base, &a, id, body).await.json().await.unwrap();
    assert_eq!(patched["name"], "cfgRenamed");
    assert_eq!(patched["runtime"]["args"][1], "--two");
    assert_eq!(patched["cwd"], "C:/work");
    assert_eq!(patched["extra_dirs"][0], "C:/extra");
    assert_eq!(patched["model"], "smart");
    assert_eq!(patched["mode"], "code");
    assert_eq!(patched["instructions"], "先看 README");
    assert_eq!(patched["env"]["FAKE_ECHO"], "sekrit");
    assert_eq!(patched["output_mode"], "status");
    assert_eq!(patched["deleted"], false);
    let list = list_my_agents(&base, &a).await;
    let listed = agent_named(&list, "cfgRenamed");
    assert_eq!(listed["env"]["OTHER"], "1");
    assert_eq!(listed["instructions"], "先看 README");

    // 缺省字段不动：只改 model 不应清掉 instructions
    let only: Value = patch_agent(&base, &a, id, json!({"model": "fast"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(only["model"], "fast");
    assert_eq!(only["instructions"], "先看 README");
    assert_eq!(only["mode"], "code");

    let mut too_many = serde_json::Map::new();
    for i in 0..51 {
        too_many.insert(format!("K{i}"), json!("v"));
    }
    for (body, code) in [
        (json!({"name": "bad name"}), "bad_name"),
        (
            json!({"instructions": "字".repeat(8001)}),
            "bad_instructions",
        ),
        (json!({"env": too_many}), "bad_env"),
        (json!({"env": {"A": 1}}), "bad_env"),
        (json!({"env": {"": "x"}}), "bad_env"),
        (
            json!({"extra_dirs": (0..11).map(|i| format!("d{i}")).collect::<Vec<_>>()}),
            "bad_extra_dirs",
        ),
        (json!({"extra_dirs": ["x".repeat(501)]}), "bad_extra_dirs"),
    ] {
        let r = patch_agent(&base, &a, id, body).await;
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap();
        assert_eq!(status, 400, "{code} → {v}");
        assert_eq!(v["error"], code);
    }
    let r = patch_agent(&base, &a, id, json!({"host_id": host_id})).await;
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "host_immutable");

    let r = patch_agent(&base, &a, id, json!({"name": "CFGB"})).await;
    assert_eq!(r.status().as_u16(), 409);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "name_taken");

    let other = two["id"].as_str().unwrap();
    let r = patch_agent(&base, &b, other, json!({"model": "smart"})).await;
    assert_eq!(r.status().as_u16(), 404);

    h.shutdown().await;
}

/// 改完配置再发 whoami：回复里是新的 mode / model / 环境变量 / 第一个文本块。
#[tokio::test]
async fn a06_whoami_uses_new_config() {
    let base = spawn_named_db(&format!("chat_t_a06w_{}", uniq())).await;
    let a = register(&base, &format!("a06w_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "who").await;
    let id = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let r = patch_agent(
        &base,
        &a,
        id,
        json!({
            "mode": "code",
            "model": "smart",
            "instructions": "INST-a06",
            "env": {"FAKE_ECHO": "sekrit"},
            "extra_dirs": ["C:/a06-extra"],
        }),
    )
    .await;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert_eq!(status, 200, "{text}");

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "w1", "whoami").await;
    let mut frames = Vec::new();
    let mut reply = None;
    for _ in 0..6 {
        frames.extend(until_frame(&mut sa, "message", A01Dur::from_secs(20)).await);
        if let Some(m) = frames
            .iter()
            .rev()
            .find(|f| f["message"]["agent"].is_object())
        {
            reply = Some(m["message"].clone());
            break;
        }
    }
    let msg = reply.expect("应收到 Agent 回复");
    let text = msg["content"].as_str().unwrap();
    assert!(text.contains("mode=code"), "{text}");
    assert!(text.contains("model=smart"), "{text}");
    assert!(text.contains("FAKE_ECHO=sekrit"), "{text}");
    assert!(text.contains("first=INST-a06"), "{text}");
    assert!(text.contains("dirs=C:/a06-extra"), "{text}");
    assert_eq!(msg["agent"]["avatar_version"], 0);
    h.shutdown().await;
}

/// probe：在线 202 后 reported_options 有两个 mode；宿主离线 409。
#[tokio::test]
async fn a06_probe_online_then_offline() {
    let base = spawn_named_db(&format!("chat_t_a06r_{}", uniq())).await;
    let a = register(&base, &format!("a06r_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "prober").await;
    let id = agent["id"].as_str().unwrap();

    let r = http()
        .post(format!("{base}/v1/agents/{id}/probe"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);

    let mut modes = 0;
    for _ in 0..50 {
        tokio::time::sleep(A01Dur::from_millis(100)).await;
        let list = list_my_agents(&base, &a).await;
        modes = agent_named(&list, "prober")["reported_options"]["modes"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if modes == 2 {
            break;
        }
    }
    assert_eq!(modes, 2, "probe 后应有两个 mode");

    h.shutdown().await;
    let mut offline = false;
    for _ in 0..30 {
        tokio::time::sleep(A01Dur::from_millis(100)).await;
        let hosts: Value = http()
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if hosts[0]["online"] == false {
            offline = true;
            break;
        }
    }
    assert!(offline, "宿主应已离线");
    let r = http()
        .post(format!("{base}/v1/agents/{id}/probe"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "host_offline");
}

/// 删除：发消息被拒且不产生调用；列表消失；会话 deleted；同名可重建；运行中的调用失败。
#[tokio::test]
async fn a06_delete_blocks_send_and_frees_name() {
    let base = spawn_named_db(&format!("chat_t_a06d_{}", uniq())).await;
    let a = register(&base, &format!("a06d_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "doomed").await;
    let id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:8000").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    assert_eq!(
        frames
            .iter()
            .rev()
            .find(|f| f["type"] == "agent_invocation")
            .unwrap()["invocation"]["status"],
        "running"
    );

    let r = http()
        .delete(format!("{base}/v1/agents/{id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);

    let invs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invs.as_array().unwrap().len(), 1);
    assert_eq!(invs[0]["status"], "failed");
    assert_eq!(invs[0]["error"], "agent_deleted");

    let list = list_my_agents(&base, &a).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .all(|x| x["name"] != "doomed"));

    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv.as_str())
        .unwrap();
    assert_eq!(c["agent"]["deleted"], true);
    assert_eq!(c["agent"]["name"], "doomed");

    send_ws(&mut sa, &conv, "d2", "echo:nope").await;
    let frames = until_frame(&mut sa, "error", A01Dur::from_secs(5)).await;
    let err = frames.iter().rev().find(|f| f["type"] == "error").unwrap();
    assert_eq!(err["code"], "agent_deleted");
    let invs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invs.as_array().unwrap().len(), 1, "拒绝的消息不应产生调用");

    let r = patch_agent(&base, &a, &id, json!({"model": "smart"})).await;
    assert_eq!(r.status().as_u16(), 404);

    let (st, again) = create_agent(&base, &a, &host_id, "doomed").await;
    assert_eq!(st, 200, "{again}");
    assert_ne!(again["id"].as_str().unwrap(), id.as_str());
    h.shutdown().await;
}

/// 头像：上传后版本变化且能取回；别人不能上传。
#[tokio::test]
async fn a06_avatar_upload_and_acl() {
    let base = spawn_named_db(&format!("chat_t_a06v_{}", uniq())).await;
    let a = register(&base, &format!("a06v_{}", uniq())).await;
    let b = register(&base, &format!("a06vb_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "faced").await;
    let id = agent["id"].as_str().unwrap();
    assert_eq!(agent["avatar_version"], 0);

    let png = png_bytes();
    let up = http()
        .put(format!("{base}/v1/agents/{id}/avatar"))
        .header("authorization", authz(&a.access))
        .multipart(avatar_part(png.clone(), "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(up.status().as_u16(), 200);
    let view: Value = up.json().await.unwrap();
    assert_eq!(view["avatar_version"], 1);

    let cur = http()
        .get(format!("{base}/v1/agents/{id}/avatar?v=1"))
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
    assert_eq!(cur.bytes().await.unwrap().as_ref(), png.as_slice());

    let denied = http()
        .put(format!("{base}/v1/agents/{id}/avatar"))
        .header("authorization", authz(&b.access))
        .multipart(avatar_part(png, "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 404);
    h.shutdown().await;
}

// ===== Agent A04 =====
// 离线排队、上线按创建顺序执行、超时清扫、取消、断线只失败运行中的调用。

async fn cancel_inv(base: &str, u: &TestUser, id: &str) -> (u16, Value) {
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

async fn list_inv(base: &str, u: &TestUser, conv: &str) -> Value {
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

async fn conv_messages(base: &str, u: &TestUser, conv: &str) -> Vec<Value> {
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

fn agent_texts(msgs: &[Value]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m["agent"].is_object())
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

/// 读到满足 pred 的 agent_invocation 帧（超时 panic）
async fn wait_inv<S>(s: &mut S, pred: impl Fn(&Value) -> bool, timeout: A01Dur) -> Value
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

async fn offline_agent(base: &str, u: &TestUser, name: &str) -> (String, String, uuid::Uuid) {
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(base, u, install).await;
    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (st, agent) = create_agent(base, u, &host_id, name).await;
    assert_eq!(st, 200, "{agent}");
    assert_eq!(agent["host_online"], false);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    (conv, agent["id"].as_str().unwrap().to_string(), install)
}

/// 1. 离线发消息 → queued；上线后 running → 回复 → succeeded。
#[tokio::test]
async fn agent_a04_queue_then_run() {
    let base = spawn_named_db(&format!("chat_it_a04q_{}", uniq())).await;
    let a = register(&base, &format!("a04a_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "qagent").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "q1", "echo:ping").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    assert!(f["invocation"]["error"].is_null());
    let id = f["invocation"]["id"].as_str().unwrap().to_string();

    let (h, _) = spawn_host(&base, &a, install).await;
    let run = wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id && f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    assert!(run["invocation"]["started_at"].is_string());
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id && f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    assert_eq!(texts, vec!["ping".to_string()]);
    h.shutdown().await;
}

/// 2. 离线连发两条，上线后按创建顺序进入 running，各得到一条回复。
#[tokio::test]
async fn agent_a04_queue_order() {
    let base = spawn_named_db(&format!("chat_it_a04ord_{}", uniq())).await;
    let a = register(&base, &format!("a04b_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "order").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "o1", "echo:one").await;
    send_ws(&mut sa, &conv, "o2", "echo:two").await;
    let mut queued = Vec::new();
    while queued.len() < 2 {
        let f = wait_inv(
            &mut sa,
            |f| f["invocation"]["status"] == "queued",
            A01Dur::from_secs(10),
        )
        .await;
        let id = f["invocation"]["id"].as_str().unwrap().to_string();
        if !queued.contains(&id) {
            queued.push(id);
        }
    }

    let (h, _) = spawn_host(&base, &a, install).await;
    // 一条循环收齐，避免 running / succeeded 交错时把帧丢掉
    let mut running = Vec::new();
    let mut done = Vec::new();
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    while done.len() < 2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let f = wait_inv(
            &mut sa,
            |f| {
                let id = f["invocation"]["id"].as_str().unwrap_or("");
                queued.iter().any(|q| q.as_str() == id)
            },
            left,
        )
        .await;
        let id = f["invocation"]["id"].as_str().unwrap().to_string();
        let status = f["invocation"]["status"].as_str().unwrap_or("");
        if status == "running" && !running.contains(&id) {
            running.push(id.clone());
        }
        if status == "succeeded" && !done.contains(&id) {
            done.push(id);
        }
    }
    assert_eq!(running, queued, "应按创建顺序下发");
    let mut texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    texts.sort();
    assert_eq!(texts, vec!["one".to_string(), "two".to_string()]);
    h.shutdown().await;
}

/// 3. Agent 覆盖超时 2 秒、清扫间隔调小 → timed_out；之后上线不再执行。
#[tokio::test]
async fn agent_a04_queue_timeout() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04to_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a04c_{}", uniq())).await;
    let (conv, agent_id, install) = offline_agent(&base, &a, "slowq").await;
    let r = http()
        .patch(format!("{base}/v1/agents/{agent_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"queue_timeout_secs": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "t1", "echo:late").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "timed_out",
        A01Dur::from_secs(8),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();

    let (h, _) = spawn_host(&base, &a, install).await;
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    let rows = list_inv(&base, &a, &conv).await;
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(row["status"], "timed_out");
    assert!(agent_texts(&conv_messages(&base, &a, &conv).await).is_empty());
    h.shutdown().await;
}

/// 4. 个人默认 0（永不）→ 清扫间隔过了仍是 queued。
#[tokio::test]
async fn agent_a04_queue_never() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04nv_{}", uniq())).await;
    st.queue_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a04d_{}", uniq())).await;
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&a.access))
        .json(&json!({"queue_timeout_secs": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (conv, _, _) = offline_agent(&base, &a, "forever").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "n1", "echo:stay").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();
    let frames = frames_for(&mut sa, A01Dur::from_millis(1200)).await;
    assert!(
        !frames
            .iter()
            .any(|f| f["invocation"]["id"] == id && f["invocation"]["status"] == "timed_out"),
        "永不超时不应被清扫: {frames:?}"
    );
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "queued");
}

/// 5. 取消 queued 不上线执行；取消 running 无回复；已完成 409；别人 404。
#[tokio::test]
async fn agent_a04_cancel_rules() {
    let base = spawn_named_db(&format!("chat_it_a04ca_{}", uniq())).await;
    let a = register(&base, &format!("a04e_{}", uniq())).await;
    let b = register(&base, &format!("a04f_{}", uniq())).await;
    let (conv, _, install) = offline_agent(&base, &a, "can").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, _) = cancel_inv(&base, &a, &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(st, 404);

    send_ws(&mut sa, &conv, "c1", "echo:nope").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let qid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, _) = cancel_inv(&base, &b, &qid).await;
    assert_eq!(st, 404, "别人取消应 404");
    let (st, body) = cancel_inv(&base, &a, &qid).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == qid && f["invocation"]["status"] == "cancelled",
        A01Dur::from_secs(10),
    )
    .await;

    let (h, _) = spawn_host(&base, &a, install).await;
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    assert!(
        agent_texts(&conv_messages(&base, &a, &conv).await).is_empty(),
        "已取消的排队调用不应执行"
    );

    send_ws(&mut sa, &conv, "c2", "sleep:10000").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let rid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, body) = cancel_inv(&base, &a, &rid).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    tokio::time::sleep(A01Dur::from_millis(1500)).await;
    assert!(
        !agent_texts(&conv_messages(&base, &a, &conv).await)
            .iter()
            .any(|t| t == "done"),
        "取消运行中的调用不应有回复"
    );

    send_ws(&mut sa, &conv, "c3", "echo:ok").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let sid = f["invocation"]["id"].as_str().unwrap().to_string();
    let (st, body) = cancel_inv(&base, &a, &sid).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "already_finished");
    let (st, _) = cancel_inv(&base, &b, &sid).await;
    assert_eq!(st, 404);
    h.shutdown().await;
}

/// 命令行里带 --ignore-cancel 的 node 还在不在（Windows：用 PowerShell 查进程表）。
#[cfg(windows)]
fn ignore_cancel_nodes() -> usize {
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "@(Get-CimInstance Win32_Process -Filter \"Name = 'node.exe'\" | Where-Object { $_.CommandLine -like '*--ignore-cancel*' }).Count",
        ])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// 6. --ignore-cancel：取消后约 5 秒子进程结束，宿主不再上报该调用。
#[cfg(windows)]
#[tokio::test]
async fn agent_a04_cancel_ignore() {
    let base = spawn_named_db(&format!("chat_it_a04ig_{}", uniq())).await;
    let a = register(&base, &format!("a04g_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(&base, &a, install).await;
    let mut args = fake_acp_args();
    args.push("--ignore-cancel".into());
    let (st, agent) = create_agent_with(&base, &a, &host_id, "stubborn", args).await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "i1", "sleep:30000").await;
    let f = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let id = f["invocation"]["id"].as_str().unwrap().to_string();
    // 等子进程真正起来再取消
    tokio::time::sleep(A01Dur::from_millis(800)).await;
    let (st, body) = cancel_inv(&base, &a, &id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    tokio::time::sleep(A01Dur::from_millis(1000)).await;
    assert!(
        ignore_cancel_nodes() >= 1,
        "取消后 1 秒内不应立刻杀掉忽略取消的子进程"
    );
    tokio::time::sleep(A01Dur::from_millis(6500)).await;
    assert_eq!(ignore_cancel_nodes(), 0, "约 5 秒宽限后子进程应被结束");
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap()[0]["status"], "cancelled");
    assert!(agent_texts(&conv_messages(&base, &a, &conv).await).is_empty());
    h.shutdown().await;
}

/// 7. 运行中断开：running → failed/host_disconnected；并存的 queued 保持，重连后执行。
#[tokio::test]
async fn agent_a04_disconnect_keeps_queue() {
    let (base, st) = spawn_named_state(&format!("chat_it_a04dc_{}", uniq())).await;
    let a = register(&base, &format!("a04h_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h, host_id) = spawn_host(&base, &a, install).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "disc").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:20000").await;
    let fa = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;
    let id_a = fa["invocation"]["id"].as_str().unwrap().to_string();
    send_ws(&mut sa, &conv, "d2", "sleep:4000").await;
    let fb = wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "running" && f["invocation"]["id"] != id_a,
        A01Dur::from_secs(15),
    )
    .await;
    let id_b = fb["invocation"]["id"].as_str().unwrap().to_string();
    // 在线时第二条也会直接 running。把它改回 queued，复现「断开时运行中与排队并存」。
    let n = sqlx::query(
        "UPDATE agent_invocations
            SET status = 'queued', started_at = NULL, finished_at = NULL,
                error = NULL, queue_deadline = NULL
          WHERE id = $1 AND status = 'running'",
    )
    .bind(uuid::Uuid::parse_str(&id_b).unwrap())
    .execute(&st.pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1, "第二条应还在 running，才能拨回 queued");

    h.shutdown().await;
    let failed = wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id_a && f["invocation"]["status"] == "failed",
        A01Dur::from_secs(10),
    )
    .await;
    assert_eq!(failed["invocation"]["error"], "host_disconnected");
    let rows = list_inv(&base, &a, &conv).await;
    let b = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id_b)
        .unwrap();
    assert_eq!(b["status"], "queued");

    let (h2, _) = spawn_host(&base, &a, install).await;
    wait_inv(
        &mut sa,
        |f| f["invocation"]["id"] == id_b && f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(20),
    )
    .await;
    let texts = agent_texts(&conv_messages(&base, &a, &conv).await);
    assert_eq!(texts, vec!["done".to_string()]);
    let rows = list_inv(&base, &a, &conv).await;
    let arow = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id_a)
        .unwrap();
    assert_eq!(arow["status"], "failed");
    h2.shutdown().await;
}

// ===== Agent A13 =====
// 工具注入通道 + 读当前会话历史：
// - invocation_start 下发调用专用凭据 tools_token（kind="invocation" 的 JWT）；
// - 宿主本地 MCP 服务凭它访问 GET /v1/agent-tools/history（Authorization: Invocation <token>）；
// - 凭据只在调用进行中（running/awaiting_approval）且主人未禁用时有效，且不能访问普通接口。
// 凭据相关测试用裸宿主 WS（不经过 agent-host 库）抓取并扣住 tools_token；
// 端到端测试走真 agent-host + node 假 agent（--mcp=http）。

type RawHostWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 裸宿主连接：role=host + host_hello → host_ready。返回 (socket, host_id)。
async fn spawn_raw_host(base: &str, u: &TestUser) -> (RawHostWs, String) {
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

/// 建一个 Agent 并发一条消息，让调用挂在 running（宿主不发 finish）。
/// 返回 (宿主WS, 主人WS, 调用id, tools_token, 专属会话id, agent_id)。
async fn a13_running_invocation(
    base: &str,
    u: &TestUser,
) -> (RawHostWs, RawHostWs, String, String, String, String) {
    let (mut sh, host_id) = spawn_raw_host(base, u).await;
    let (st, agent) = create_agent(base, u, &host_id, "tooler").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap();
    send_ws(&mut su, &conv, &format!("a13-{}", uniq()), "echo:hold").await;
    let frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start = frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    let token = start["tools_token"]
        .as_str()
        .expect("invocation_start 应带 tools_token")
        .to_string();
    (sh, su, inv_id, token, conv, agent_id)
}

/// GET /v1/agent-tools/history；auth 是完整的 Authorization 头值。
async fn tool_history(base: &str, auth: &str, query: &str) -> reqwest::Response {
    http()
        .get(format!("{base}/v1/agent-tools/history{query}"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
}

/// 宿主侧结束调用（invocation_finish ok:true）
async fn host_finish(sh: &mut RawHostWs, inv_id: &str) {
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv_id,"ok":true,"text":"done"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

/// 自签调用凭据（测试库 jwt_secret = "test-secret"）。
fn a13_mint(secret: &str, claims: Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// 主路径：真宿主 + 假 agent（--mcp=http）。两条普通消息后发 tool:read_history，
/// Agent 的回复里应包含前两条消息正文（工具结果经 MCP 服务 ← GET /v1/agent-tools/history）。
#[tokio::test]
async fn a13_tool_read_history_end_to_end() {
    let base = spawn_named_db(&format!("chat_t_a13e2e_{}", uniq())).await;
    let a = register(&base, &format!("a13e_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    // --mcp=http：假 agent 在 initialize 里声明 agentCapabilities.mcpCapabilities.http
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": "reader",
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let agent: Value = r.json().await.unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "h1", "第一条历史消息").await;
    send_ws(&mut sa, &conv, "h2", "第二条历史消息").await;
    send_ws(&mut sa, &conv, "h3", "tool:read_history").await;

    // 三条消息各触发一个调用；等到 3 个 succeeded，期间收集 message 帧
    let mut frames: Vec<Value> = Vec::new();
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    while frames
        .iter()
        .filter(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .count()
        < 3
    {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "三次调用未全部成功: {frames:?}");
        frames.extend(until_frame(&mut sa, "agent_invocation", left).await);
    }
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(500)).await);

    // 第三个调用的 Agent 回复 = 工具结果文本，须包含前两条消息正文
    let found = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .map(|f| f["message"]["content"].as_str().unwrap_or_default())
        .any(|c| c.contains("第一条历史消息") && c.contains("第二条历史消息"));
    assert!(found, "回复应包含两条历史正文: {frames:?}");
    ha.shutdown().await;
}

/// 凭据生命周期：running 中可用（返回的是发起会话的消息，带分页参数语义）；
/// 调用结束（宿主 finish）后同一 token → 401。
#[tokio::test]
async fn a13_tools_token_dies_with_invocation() {
    let base = spawn_named_db(&format!("chat_t_a13tok_{}", uniq())).await;
    let a = register(&base, &format!("a13t_{}", uniq())).await;
    let (mut sh, mut su, inv_id, token, conv, _agent_id) = a13_running_invocation(&base, &a).await;

    // running 中：200，返回发起会话的消息
    let r = tool_history(&base, &format!("Invocation {token}"), "").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{page}");
    assert_eq!(msgs[0]["content"], "echo:hold");
    assert_eq!(msgs[0]["is_agent"], false);
    assert_eq!(msgs[0]["seq"].as_i64().unwrap(), 1);
    assert_eq!(page["has_more"], false);

    // 再发一条（第二个调用并行挂着）：limit=1 只回最新一条且 has_more；
    // before_seq=2 只回第一条
    send_ws(&mut su, &conv, "a13-2", "echo:more").await;
    let _ = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let r = tool_history(&base, &format!("Invocation {token}"), "?limit=1").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{page}");
    assert_eq!(msgs[0]["content"], "echo:more");
    assert_eq!(page["has_more"], true);
    let r = tool_history(&base, &format!("Invocation {token}"), "?before_seq=2").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["messages"][0]["seq"].as_i64().unwrap(), 1);

    // 结束第一个调用 → 它的凭据立刻失效（轮询等 finish 落库）
    host_finish(&mut sh, &inv_id).await;
    let mut dead = false;
    for _ in 0..50 {
        if tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status()
            == 401
        {
            dead = true;
            break;
        }
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    assert!(dead, "调用结束后 tools_token 应 401");
}

/// 两种凭据互不通用：access token 走 Invocation 通道 → 401；
/// 调用凭据访问普通 REST（Bearer 与 Invocation 两种写法）→ 401。
#[tokio::test]
async fn a13_token_kind_isolation() {
    let base = spawn_named_db(&format!("chat_t_a13iso_{}", uniq())).await;
    let a = register(&base, &format!("a13i_{}", uniq())).await;
    let (mut sh, _su, inv_id, token, _conv, _agent_id) = a13_running_invocation(&base, &a).await;

    // 普通 access token 不能当调用凭据用（无论 scheme）
    for auth in [format!("Invocation {}", a.access), authz(&a.access)] {
        assert_eq!(
            tool_history(&base, &auth, "").await.status(),
            401,
            "auth={auth}"
        );
    }
    // 调用凭据不能访问普通接口（无论 scheme）
    for auth in [authz(&token), format!("Invocation {token}")] {
        let r = http()
            .get(format!("{base}/v1/conversations"))
            .header("authorization", auth.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "auth={auth}");
    }
    host_finish(&mut sh, &inv_id).await;
}

/// 伪造（错签名/缺 kind/错 kind/指向不存在的调用）与过期凭据 → 401；
/// 主人被禁用后，仍挂在 running 的调用凭据 → 401。
#[tokio::test]
async fn a13_invocation_token_rejects_forgery_and_disabled() {
    let base = spawn_named_db(&format!("chat_t_a13sec_{}", uniq())).await;
    // 首个注册用户是管理员；主人单开（管理员账号不可被禁用，不能拿主人当第一用户）
    let admin = register(&base, &format!("a13adm_{}", uniq())).await;
    let a = register(&base, &format!("a13s_{}", uniq())).await;
    let (mut sh, _su, inv_id, token, _conv, agent_id) = a13_running_invocation(&base, &a).await;

    let now = chrono::Utc::now().timestamp();
    let claims = json!({"kind":"invocation","sub":inv_id,"owner":a.id,
                        "agent":agent_id,"depth":0,"iat":now,"exp":now+3600});
    // 对照组：真 token 与正确自签的 token 都是 200（凭据不登记，签名合法即可）
    assert_eq!(
        tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status(),
        200
    );
    assert_eq!(
        tool_history(
            &base,
            &format!("Invocation {}", a13_mint("test-secret", claims.clone())),
            ""
        )
        .await
        .status(),
        200
    );

    let mut expired = claims.clone();
    expired["exp"] = json!(now - 3600);
    let mut no_kind = claims.clone();
    no_kind.as_object_mut().unwrap().remove("kind");
    let mut wrong_kind = claims.clone();
    wrong_kind["kind"] = json!("access");
    let mut ghost = claims.clone();
    ghost["sub"] = json!(uuid::Uuid::new_v4());
    for (name, t) in [
        ("bad_sig", a13_mint("other-secret", claims.clone())),
        ("expired", a13_mint("test-secret", expired)),
        ("no_kind", a13_mint("test-secret", no_kind)),
        ("wrong_kind", a13_mint("test-secret", wrong_kind)),
        ("ghost_invocation", a13_mint("test-secret", ghost)),
    ] {
        assert_eq!(
            tool_history(&base, &format!("Invocation {t}"), "")
                .await
                .status(),
            401,
            "{name}"
        );
    }

    // 主人被禁用 → 调用仍是 running，凭据已死
    let r = http()
        .post(format!("{base}/v1/admin/users/{}/disable", a.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(
        tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status(),
        401
    );
    host_finish(&mut sh, &inv_id).await;
}

/// 接口只读发起调用的那个会话：没有会话参数可用来构造越权；
/// 主人在其它会话里的消息不会出现在返回里。
#[tokio::test]
async fn a13_history_scoped_to_invoking_conversation() {
    let base = spawn_named_db(&format!("chat_t_a13scp_{}", uniq())).await;
    let a = register(&base, &format!("a13x_{}", uniq())).await;
    let b = register(&base, &format!("a13y_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    // DM 里放一条不该出现在工具结果里的消息，等它落库（收到 message 帧即已提交）
    send_ws(&mut sa, &dm, "dm1", "不该被读到的私聊正文").await;
    let _ = next_of(&mut sa, "message").await;

    let (mut sh, _su, inv_id, token, _conv, _agent_id) = a13_running_invocation(&base, &a).await;
    let r = tool_history(&base, &format!("Invocation {token}"), "?limit=200").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "应只返回发起会话的消息: {msgs:?}");
    assert_eq!(msgs[0]["content"], "echo:hold");
    host_finish(&mut sh, &inv_id).await;
}

// ===== Agent A09 =====
// 宿主名称只在第一次 host_hello 写入；之后改名走 PATCH，重连不再覆盖。

async fn spawn_host_labeled(
    base: &str,
    u: &TestUser,
    install_id: uuid::Uuid,
    label: &str,
    platform: &str,
) -> (agent_host::HostHandle, String) {
    let h = agent_host::spawn(
        agent_host::HostConfig {
            server_url: base.into(),
            install_id,
            label: label.into(),
            platform: platform.into(),
            mcp_bridge: None,
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

async fn patch_host(base: &str, u: &TestUser, host_id: &str, label: &str) -> (u16, Value) {
    let r = http()
        .patch(format!("{base}/v1/agent-hosts/{host_id}"))
        .header("authorization", authz(&u.access))
        .json(&json!({ "label": label }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

async fn agent_hosts_of(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/agent-hosts"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// 首次 host_hello 写入 label；改名后重连带另一个 label，名称保持改后的值，平台仍更新。
#[tokio::test]
async fn agent_a09_label_written_once() {
    let base = spawn_named_db(&format!("chat_it_a09lab_{}", uniq())).await;
    let a = register(&base, &format!("a09a_{}", uniq())).await;
    let install = uuid::Uuid::new_v4();
    let (h1, host_id) = spawn_host_labeled(&base, &a, install, "first-pc", "windows").await;

    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(hosts[0]["label"], "first-pc");
    assert_eq!(hosts[0]["platform"], "windows");

    let (st, body) = patch_host(&base, &a, &host_id, "  家里 Windows  ").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "家里 Windows");
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "家里 Windows");

    h1.shutdown().await;
    let (h2, host_id2) = spawn_host_labeled(&base, &a, install, "other-label", "linux").await;
    assert_eq!(host_id2, host_id);
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts.as_array().unwrap().len(), 1);
    assert_eq!(hosts[0]["id"], host_id.as_str());
    assert_eq!(
        hosts[0]["label"], "家里 Windows",
        "重连的 host_hello 不得覆盖已改名称"
    );
    assert_eq!(hosts[0]["platform"], "linux", "重连仍更新 platform");
    h2.shutdown().await;
}

/// PATCH：空 / 超长 400；别人的宿主 404；改名后主人的普通连接收到带新 label 的更新。
#[tokio::test]
async fn agent_a09_patch_host_label() {
    let base = spawn_named_db(&format!("chat_it_a09pat_{}", uniq())).await;
    let a = register(&base, &format!("a09c_{}", uniq())).await;
    let b = register(&base, &format!("a09d_{}", uniq())).await;
    // 主人先连上，才能收到随后的上线帧和改名帧
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (h, host_id) = spawn_host_labeled(&base, &a, uuid::Uuid::new_v4(), "pc", "windows").await;
    until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let (st, body) = patch_host(&base, &a, &host_id, "书房").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "书房");
    assert_eq!(body["id"], host_id.as_str());
    assert_eq!(body["online"], true);

    let frames = until_frame(&mut sa, "agent_host_status", A01Dur::from_secs(5)).await;
    let upd = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_host_status")
        .unwrap();
    assert_eq!(upd["host_id"], host_id.as_str());
    assert_eq!(upd["online"], true);
    assert_eq!(upd["label"], "书房");

    let others = frames_for(&mut sb, A01Dur::from_millis(400)).await;
    assert!(
        !others.iter().any(|f| f["type"] == "agent_host_status"),
        "改名只推给主人: {others:?}"
    );

    let too_long = "x".repeat(61);
    let too_long_han = "测".repeat(61);
    for bad in ["", "   ", too_long.as_str(), too_long_han.as_str()] {
        let (st, body) = patch_host(&base, &a, &host_id, bad).await;
        assert_eq!(st, 400, "label {bad:?} 应 400: {body}");
        assert_eq!(body["error"], "bad_label");
    }
    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "书房", "非法改名不得落库");

    let sixty = "测".repeat(60);
    let (st, body) = patch_host(&base, &a, &host_id, &sixty).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], sixty);

    let padded = format!("  {}  ", "y".repeat(60));
    let (st, body) = patch_host(&base, &a, &host_id, &padded).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["label"], "y".repeat(60));

    let (st, body) = patch_host(&base, &b, &host_id, "stolen").await;
    assert_eq!(st, 404, "别人的宿主应 404: {body}");
    let (st, body) = patch_host(&base, &a, &uuid::Uuid::new_v4().to_string(), "nope").await;
    assert_eq!(st, 404, "不存在的宿主应 404: {body}");

    let hosts = agent_hosts_of(&base, &a).await;
    assert_eq!(hosts[0]["label"], "y".repeat(60));
    let b_hosts = agent_hosts_of(&base, &b).await;
    assert!(b_hosts.as_array().unwrap().is_empty());
    h.shutdown().await;
}

// ===== Agent A11 =====
// 主人在私聊 / 群里 @ 自己的 Agent。回复进原会话，状态帧给全部成员。

async fn a11_group(base: &str, owner: &TestUser, members: &[&TestUser]) -> String {
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

async fn a11_unread(base: &str, u: &TestUser, conv: &str) -> i64 {
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

async fn delete_agent_req(base: &str, u: &TestUser, id: &str) -> u16 {
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
async fn wait_agent_reply(base: &str, u: &TestUser, conv: &str, want: &[&str]) -> Vec<String> {
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
    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (st, _) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200);
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

// ===== Agent A08 =====
// 使用权：全局开关、默认、逐用户覆盖、收回时取消并断开宿主、宿主跟随登录会话。

async fn put_settings(base: &str, admin: &TestUser, body: Value) -> Value {
    let r = http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "settings {v}");
    v
}

async fn put_access(base: &str, admin: &TestUser, user_id: &str, allowed: Option<bool>) -> u16 {
    http()
        .put(format!("{base}/v1/admin/users/{user_id}/agent-access"))
        .header("authorization", authz(&admin.access))
        .json(&json!({ "allowed": allowed }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn me_json(base: &str, access: &str) -> Value {
    http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn deny_agent(base: &str, method: &str, path: &str, access: &str, body: Option<Value>) {
    let c = http();
    let url = format!("{base}/v1{path}");
    let req = match method {
        "GET" => c.get(url),
        "POST" => c.post(url),
        "PUT" => c.put(url),
        "PATCH" => c.patch(url),
        "DELETE" => c.delete(url),
        _ => unreachable!(),
    };
    let req = req.header("authorization", authz(access));
    let r = if let Some(b) = &body {
        req.json(b).send().await.unwrap()
    } else {
        req.send().await.unwrap()
    };
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 403, "{method} {path} -> {v}");
    assert_eq!(v["error"], "agent_access_denied");
}

/// 无权时升级成功但马上被关掉，不会 host_ready。
async fn assert_host_rejected(base: &str, u: &TestUser) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let install = uuid::Uuid::new_v4();
    let _ = sock
        .send(WsMsg::Text(
            json!({"type":"host_hello","install_id":install,"label":"nope","platform":"test"})
                .to_string()
                .into(),
        ))
        .await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(3);
    let mut ready = false;
    let mut ended = false;
    while std::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if t.contains("host_ready") {
                    ready = true;
                    break;
                }
            }
            Ok(Some(Ok(WsMsg::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                ended = true;
                break;
            }
            Err(_) => break,
            _ => {}
        }
    }
    assert!(!ready, "无权宿主不应拿到 host_ready");
    assert!(ended, "无权宿主连接应在握手后被关闭");
}

/// 读宿主文本帧直到断言成立。`__closed__` 表示套接字已结束。
async fn host_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pred: impl Fn(&str, Option<&Value>) -> bool,
    timeout: A01Dur,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等宿主帧超时");
        let t = tokio::time::timeout(left, rx.recv())
            .await
            .expect("等宿主帧超时")
            .expect("宿主管道已关");
        let parsed = serde_json::from_str::<Value>(&t).ok();
        if pred(&t, parsed.as_ref()) {
            return t;
        }
    }
}

/// 真 WS 宿主：hello 之后把后续文本帧转进通道。关掉 close 发送端即断开。
async fn raw_host(
    base: &str,
    u: &TestUser,
    install: uuid::Uuid,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::oneshot::Sender<()>,
) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":install,"label":"raw","platform":"test"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let frames = until_frame(&mut sock, "host_ready", A01Dur::from_secs(5)).await;
    let id = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "host_ready")
        .unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut close_rx => break,
                incoming = sock.next() => {
                    match incoming {
                        Some(Ok(WsMsg::Text(t))) => { let _ = tx.send(t.to_string()); }
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
        }
        let _ = tx.send("__closed__".into());
    });
    (id, rx, close_tx)
}

async fn wait_online(base: &str, access: &str, host_id: &str, online: bool) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let hosts: Value = http()
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let on = hosts
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == host_id)
            .and_then(|h| h["online"].as_bool())
            .unwrap_or(false);
        if on == online {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "host {host_id} online={on} 期望 {online}: {hosts}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

/// 1. 全局关：普通用户 Agent REST 403、宿主连不上、专属会话发消息被拒且不落库；管理员照常。
#[tokio::test]
async fn agent_a08_global_off() {
    let base = spawn_named_db(&format!("chat_it_a08off_{}", uniq())).await;
    let admin = register(&base, &format!("a08ad_{}", uniq())).await;
    let user = register(&base, &format!("a08u_{}", uniq())).await;
    befriend(&base, &admin, &user).await;

    let (h, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &user, &host_id, "runner").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    h.shutdown().await;

    let settings = put_settings(&base, &admin, json!({"agent_enabled": false})).await;
    assert_eq!(settings["agent_enabled"], false);
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);
    assert_eq!(me_json(&base, &admin.access).await["agent_access"], true);

    deny_agent(&base, "GET", "/agent-hosts", &user.access, None).await;
    deny_agent(&base, "GET", "/agents", &user.access, None).await;
    deny_agent(
        &base,
        "POST",
        "/agents",
        &user.access,
        Some(json!({"name":"x","host_id":host_id,"runtime":{"command":"node","args":[]}})),
    )
    .await;
    deny_agent(
        &base,
        "PATCH",
        &format!("/agents/{agent_id}"),
        &user.access,
        Some(json!({"cwd":"/tmp"})),
    )
    .await;
    deny_agent(
        &base,
        "DELETE",
        &format!("/agents/{agent_id}"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/agents/{agent_id}/probe"),
        &user.access,
        None,
    )
    .await;
    deny_agent(&base, "GET", "/users/me/agent-settings", &user.access, None).await;
    deny_agent(
        &base,
        "PUT",
        "/users/me/agent-settings",
        &user.access,
        Some(json!({"output_mode":"stream"})),
    )
    .await;
    deny_agent(
        &base,
        "GET",
        &format!("/conversations/{conv}/agent-invocations"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/conversations/{conv}/agent-context/reset"),
        &user.access,
        None,
    )
    .await;
    deny_agent(
        &base,
        "POST",
        &format!("/agent-invocations/{}/cancel", uuid::Uuid::new_v4()),
        &user.access,
        None,
    )
    .await;
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(b"not-an-image".as_slice())
            .file_name("a.png")
            .mime_str("image/png")
            .unwrap(),
    );
    let avatar = http()
        .put(format!("{base}/v1/agents/{agent_id}/avatar"))
        .header("authorization", authz(&user.access))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(avatar.status(), 403);
    assert_eq!(
        avatar.json::<Value>().await.unwrap()["error"],
        "agent_access_denied"
    );
    // 头像 GET 不鉴权
    let guest = http()
        .get(format!("{base}/v1/agents/{agent_id}/avatar"))
        .send()
        .await
        .unwrap();
    assert_eq!(guest.status(), 404);

    assert_host_rejected(&base, &user).await;

    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;
    send_ws(&mut su, &conv, "blocked", "should-not-store").await;
    let err = next_of(&mut su, "error").await;
    assert_eq!(err["code"], "agent_access_denied");
    let msgs = conv_messages(&base, &user, &conv).await;
    assert!(
        msgs.iter().all(|m| m["content"] != "should-not-store"),
        "无权时专属会话消息不应落库: {msgs:?}"
    );

    let gid = mk_group(&base, &user, &[admin.id.clone()]).await;
    send_ws(&mut su, &gid, "at1", "@runner hello-group").await;
    let quiet = frames_for(&mut su, A01Dur::from_millis(400)).await;
    assert!(
        quiet.iter().all(|f| f["type"] != "agent_invocation"),
        "群里 @ 不应触发调用: {quiet:?}"
    );
    let group_msgs = conv_messages(&base, &user, &gid).await;
    assert!(
        group_msgs
            .iter()
            .any(|m| m["content"] == "@runner hello-group"),
        "群消息本身要留下: {group_msgs:?}"
    );

    // 管理员不受全局开关影响
    let agents: Value = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(agents.as_array().unwrap().is_empty());
    let (ha, ahost) = spawn_host(&base, &admin, uuid::Uuid::new_v4()).await;
    let (st, created) = create_agent(&base, &admin, &ahost, "boss").await;
    assert_eq!(st, 200, "{created}");
    let aconv = created["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    send_ws(&mut sa, &aconv, "ok1", "echo:admin-ok").await;
    wait_inv(
        &mut sa,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    ha.shutdown().await;
}

/// 2. 默认禁止 + 覆盖允许；改回 null 后又跟随默认。
#[tokio::test]
async fn agent_a08_default_and_override() {
    let base = spawn_named_db(&format!("chat_it_a08def_{}", uniq())).await;
    let admin = register(&base, &format!("a08bd_{}", uniq())).await;
    let allow = register(&base, &format!("a08ba_{}", uniq())).await;
    let other = register(&base, &format!("a08bb_{}", uniq())).await;

    let s = put_settings(&base, &admin, json!({"agent_default_allowed": false})).await;
    assert_eq!(s["agent_default_allowed"], false);
    deny_agent(&base, "GET", "/agents", &allow.access, None).await;
    deny_agent(&base, "GET", "/agents", &other.access, None).await;

    assert_eq!(put_access(&base, &admin, &allow.id, Some(true)).await, 204);
    let ok = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&allow.access))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    deny_agent(&base, "GET", "/agent-hosts", &other.access, None).await;

    assert_eq!(put_access(&base, &admin, &allow.id, None).await, 204);
    deny_agent(
        &base,
        "GET",
        "/users/me/agent-settings",
        &allow.access,
        None,
    )
    .await;

    let missing = put_access(&base, &admin, &uuid::Uuid::new_v4().to_string(), Some(true)).await;
    assert_eq!(missing, 404);

    let users: Value = http()
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
        .find(|u| u["id"] == allow.id.as_str())
        .unwrap();
    assert!(row["agent_access"].is_null());
    assert_eq!(row["agent_access_effective"], false);
    let adm = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == admin.id.as_str())
        .unwrap();
    assert_eq!(adm["agent_access_effective"], true);
}

/// 3. 收回使用权：running 收到取消并断开，queued 也取消；恢复后可以再调用。
#[tokio::test]
async fn agent_a08_revoke_cancels() {
    let base = spawn_named_db(&format!("chat_it_a08rv_{}", uniq())).await;
    let admin = register(&base, &format!("a08rd_{}", uniq())).await;
    let user = register(&base, &format!("a08ru_{}", uniq())).await;
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;

    let install = uuid::Uuid::new_v4();
    let (host_id, mut host_rx, host_keep) = raw_host(&base, &user, install).await;
    let (st, agent) = create_agent(&base, &user, &host_id, "runner").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let install2 = uuid::Uuid::new_v4();
    let (host2, mut rx2, host2_keep) = raw_host(&base, &user, install2).await;
    let (st, agent2) = create_agent(&base, &user, &host2, "waiter").await;
    assert_eq!(st, 200, "{agent2}");
    let conv2 = agent2["conversation_id"].as_str().unwrap().to_string();
    drop(host2_keep);
    host_until(&mut rx2, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    // 等服务端把这条宿主标离线，再发消息才会进排队而不是直接 running
    let off_deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let left = off_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "宿主 2 没有离线");
        let frames = until_frame(&mut su, "agent_host_status", left).await;
        if frames
            .iter()
            .any(|f| f["host_id"] == host2.as_str() && f["online"] == false)
        {
            break;
        }
    }

    send_ws(&mut su, &conv2, "q1", "echo:later").await;
    let queued = wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "queued",
        A01Dur::from_secs(10),
    )
    .await;
    let qid = queued["invocation"]["id"].as_str().unwrap().to_string();

    send_ws(&mut su, &conv, "r1", "echo:hold").await;
    let running = wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(10),
    )
    .await;
    let rid = running["invocation"]["id"].as_str().unwrap().to_string();
    host_until(
        &mut host_rx,
        |_, v| v.is_some_and(|v| v["type"] == "invocation_start" && v["invocation_id"] == rid),
        A01Dur::from_secs(5),
    )
    .await;

    assert_eq!(put_access(&base, &admin, &user.id, Some(false)).await, 204);
    host_until(
        &mut host_rx,
        |_, v| v.is_some_and(|v| v["type"] == "invocation_cancel" && v["invocation_id"] == rid),
        A01Dur::from_secs(5),
    )
    .await;
    host_until(&mut host_rx, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(host_keep);

    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut saw_run = false;
    let mut saw_q = false;
    let mut saw_off = false;
    while !(saw_run && saw_q && saw_off) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "收回后的帧没收齐 run={saw_run} q={saw_q} off={saw_off}"
        );
        let msg = tokio::time::timeout(left, su.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "agent_invocation" {
            let id = v["invocation"]["id"].as_str().unwrap_or("");
            if v["invocation"]["status"] == "cancelled"
                && v["invocation"]["error"] == "agent_access_revoked"
            {
                if id == rid {
                    saw_run = true;
                }
                if id == qid {
                    saw_q = true;
                }
            }
        }
        if v["type"] == "agent_host_status" && v["host_id"] == host_id && v["online"] == false {
            saw_off = true;
        }
    }

    assert_eq!(put_access(&base, &admin, &user.id, Some(true)).await, 204);
    let (h, id2) = spawn_host(&base, &user, install).await;
    assert_eq!(id2, host_id);
    send_ws(&mut su, &conv, "r2", "echo:back").await;
    wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "succeeded",
        A01Dur::from_secs(15),
    )
    .await;
    let rows = list_inv(&base, &user, &conv).await;
    let old = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == rid)
        .unwrap();
    assert_eq!(old["status"], "cancelled");
    assert_eq!(old["error"], "agent_access_revoked");
    let queued_rows = list_inv(&base, &user, &conv2).await;
    assert_eq!(queued_rows[0]["status"], "cancelled");
    assert_eq!(queued_rows[0]["error"], "agent_access_revoked");
    h.shutdown().await;
}

/// 4. 禁用账号：进行中的调用取消，宿主断开（online=false）。
#[tokio::test]
async fn agent_a08_disable_drops_host() {
    let base = spawn_named_db(&format!("chat_it_a08dis_{}", uniq())).await;
    let admin = register(&base, &format!("a08dd_{}", uniq())).await;
    let user = register(&base, &format!("a08du_{}", uniq())).await;
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;
    let (h, host_id) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let online = until_frame(&mut su, "agent_host_status", A01Dur::from_secs(5)).await;
    let (_, agent) = create_agent(&base, &user, &host_id, "sleeper").await;
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    assert!(online
        .iter()
        .any(|f| f["host_id"] == host_id && f["online"] == true));
    send_ws(&mut su, &conv, "s1", "sleep:20000").await;
    wait_inv(
        &mut su,
        |f| f["invocation"]["status"] == "running",
        A01Dur::from_secs(15),
    )
    .await;

    let r = http()
        .post(format!("{base}/v1/admin/users/{}/disable", user.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut saw_cancel = false;
    let mut saw_off = false;
    while !(saw_cancel && saw_off) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "禁用后没收齐 cancel={saw_cancel} off={saw_off}"
        );
        let msg = tokio::time::timeout(left, su.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMsg::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "agent_invocation"
            && v["invocation"]["status"] == "cancelled"
            && v["invocation"]["error"] == "agent_access_revoked"
        {
            saw_cancel = true;
        }
        if v["type"] == "agent_host_status" && v["host_id"] == host_id && v["online"] == false {
            saw_off = true;
        }
    }
    h.shutdown().await;
}

/// 5. 移出宿主所用的登录会话会断开该宿主；改密只断开其他会话的宿主。
#[tokio::test]
async fn agent_a08_host_follows_session() {
    let base = spawn_named_db(&format!("chat_it_a08ses_{}", uniq())).await;
    let reg = register(&base, &format!("a08s_{}", uniq())).await;
    let dev_a = login_device(&base, &reg.username, "secret123", "设备甲").await;
    let dev_b = login_device(&base, &reg.username, "secret123", "设备乙").await;

    let (id_a, mut rx_a, keep_a) = raw_host(&base, &dev_a, uuid::Uuid::new_v4()).await;
    let (id_b, mut rx_b, keep_b) = raw_host(&base, &dev_b, uuid::Uuid::new_v4()).await;
    let (id_r, mut rx_r, keep_r) = raw_host(&base, &reg, uuid::Uuid::new_v4()).await;
    wait_online(&base, &dev_a.access, &id_a, true).await;
    wait_online(&base, &dev_a.access, &id_b, true).await;
    wait_online(&base, &dev_a.access, &id_r, true).await;

    let sessions = sessions_of(&base, &dev_a.access).await;
    let sid_b = sessions
        .iter()
        .find(|s| s["device_label"] == "设备乙")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let r = http()
        .delete(format!("{base}/v1/users/me/sessions/{sid_b}"))
        .header("authorization", authz(&dev_a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    host_until(&mut rx_b, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(keep_b);
    wait_online(&base, &dev_a.access, &id_b, false).await;
    wait_online(&base, &dev_a.access, &id_a, true).await;
    wait_online(&base, &dev_a.access, &id_r, true).await;

    let r = http()
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"secret456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let new_access = body["access_token"].as_str().unwrap();
    host_until(&mut rx_r, |t, _| t == "__closed__", A01Dur::from_secs(5)).await;
    drop(keep_r);
    wait_online(&base, new_access, &id_r, false).await;
    wait_online(&base, new_access, &id_a, true).await;
    let stray = tokio::time::timeout(A01Dur::from_millis(400), rx_a.recv()).await;
    if let Ok(Some(t)) = stray {
        assert_ne!(t, "__closed__", "当前会话的宿主不应被改密断开");
    }
    drop(keep_a);
}

/// 6. /users/me 的 agent_access 随设置变化，变化时收到 invalidate。
#[tokio::test]
async fn agent_a08_me_and_invalidate() {
    let base = spawn_named_db(&format!("chat_it_a08me_{}", uniq())).await;
    let admin = register(&base, &format!("a08md_{}", uniq())).await;
    let user = register(&base, &format!("a08mu_{}", uniq())).await;
    assert_eq!(me_json(&base, &user.access).await["agent_access"], true);

    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    next_of(&mut su, "presence_init").await;

    assert_eq!(put_access(&base, &admin, &user.id, Some(false)).await, 204);
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);

    assert_eq!(put_access(&base, &admin, &user.id, None).await, 204);
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], true);

    put_settings(&base, &admin, json!({"agent_enabled": false})).await;
    let f = next_of(&mut su, "invalidate").await;
    assert_eq!(f["what"], "me");
    assert_eq!(me_json(&base, &user.access).await["agent_access"], false);
    assert_eq!(me_json(&base, &admin.access).await["agent_access"], true);
}

// ===== Agent A03：三种输出方式 + 调用状态标签 =====

/// 收集帧直到指定调用进入终态（succeeded/failed），返回含终态帧的全部帧。
async fn until_inv_done<S>(s: &mut S, inv_id: &str, timeout: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 {inv_id} 终态超时；已收到: {out:?}");
        match tokio::time::timeout(left, s.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let done = v["type"] == "agent_invocation"
                        && v["invocation"]["id"] == inv_id
                        && matches!(
                            v["invocation"]["status"].as_str(),
                            Some("succeeded") | Some("failed")
                        );
                    out.push(v);
                    if done {
                        return out;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("等 {inv_id} 终态时 ws 异常: {other:?}; 已收到: {out:?}"),
        }
    }
}

/// A03 夹具：在已连接的会话里发送 prompt 并等到 running 帧，返回调用 id。
async fn a03_invoke<S>(conv: &str, prompt: &str, sa: &mut S) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<WsMsg>
        + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    send_ws(sa, conv, &format!("a03{}", uniq()), prompt).await;
    let frames = until_frame(sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .expect("应有 running 帧");
    assert_eq!(inv["invocation"]["status"], "running");
    inv["invocation"]["id"].as_str().unwrap().to_string()
}

/// stream：agent_stream 帧（含 delta 与 step）、历史无中间内容、定稿带 2 条 ok 步骤。
#[tokio::test]
async fn agent_a03_stream_mode() {
    let base = spawn_named_db(&format!("chat_t_a03s_{}", uniq())).await;
    let a = register(&base, &format!("a03s_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "streamer").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let mut frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(400)).await);

    let streams: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "agent_stream")
        .collect();
    assert!(
        !streams.is_empty(),
        "stream 模式应收到 agent_stream: {frames:?}"
    );
    assert!(streams
        .iter()
        .all(|f| f["invocation_id"] == inv_id && f["conversation_id"] == conv.as_str()));
    assert!(
        streams.iter().any(|f| f["delta"].is_string()),
        "至少一帧带 delta: {streams:?}"
    );
    let step_frames: Vec<&&Value> = streams.iter().filter(|f| f["step"].is_object()).collect();
    assert!(
        step_frames
            .iter()
            .any(|f| f["step"]["id"] == "step-1" && f["step"]["ok"].is_null()),
        "应有 step-1 的进行中帧: {step_frames:?}"
    );
    assert!(
        step_frames
            .iter()
            .any(|f| f["step"]["id"] == "step-1" && f["step"]["ok"] == true)
            && step_frames
                .iter()
                .any(|f| f["step"]["id"] == "step-2" && f["step"]["ok"] == true),
        "两个步骤都应有完成帧: {step_frames:?}"
    );
    // 终态帧之后不再有 agent_stream
    let tail: Vec<&Value> = frames
        .iter()
        .skip_while(|f| {
            !(f["type"] == "agent_invocation"
                && f["invocation"]["id"] == inv_id
                && f["invocation"]["status"] == "succeeded")
        })
        .collect();
    assert!(
        !tail.iter().any(|f| f["type"] == "agent_stream"),
        "终态后不应再有 agent_stream: {tail:?}"
    );

    // 定稿消息：Agent 归属 + agent_steps 两条且全 ok
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有定稿 Agent 消息");
    let steps = agent_msg["message"]["agent_steps"]
        .as_array()
        .expect("定稿消息应带 agent_steps");
    assert_eq!(steps.len(), 2);
    assert!(steps.iter().all(|s| s["ok"] == true));

    // 历史：触发消息 + 一条定稿，没有任何中间增量消息
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2, "历史只应有触发消息和定稿: {list:?}");
    assert!(list[0]["agent"].is_null());
    assert_eq!(list[1]["agent_steps"].as_array().unwrap().len(), 2);
    assert!(list[1]["content"].as_str().unwrap().ends_with("done"));
    ha.shutdown().await;
}

/// status：不推增量；步骤变化推带 current_step 的 agent_invocation；结束一条结果消息。
#[tokio::test]
async fn agent_a03_status_mode() {
    let base = spawn_named_db(&format!("chat_t_a03st_{}", uniq())).await;
    let a = register(&base, &format!("a03st_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "stater").await;
    assert_eq!(st, 200);
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    // 单个 Agent 覆盖为 status
    let r = http()
        .patch(format!("{base}/v1/agents/{agent_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"output_mode": "status"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let mut frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(400)).await);

    assert!(
        !frames.iter().any(|f| f["type"] == "agent_stream"),
        "status 模式不得推 agent_stream: {frames:?}"
    );
    // running 期间应有带 current_step 的状态帧；终态帧 current_step 清空
    assert!(
        frames.iter().any(|f| f["type"] == "agent_invocation"
            && f["invocation"]["id"] == inv_id
            && f["invocation"]["current_step"].is_string()),
        "应有带 current_step 的状态帧: {frames:?}"
    );
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .unwrap();
    assert!(term["invocation"]["current_step"].is_null());

    // 恰好一条 Agent 结果消息（无步骤消息），带 agent_steps
    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2, "status 模式只应有触发 + 结果两条: {list:?}");
    let steps = list[1]["agent_steps"].as_array().expect("结果消息带步骤");
    assert_eq!(steps.len(), 2);
    assert!(list[1]["content"].as_str().unwrap().ends_with("done"));
    ha.shutdown().await;
}

/// steps：个人默认设为 steps、Agent 不覆盖 → 每步一条 ✓ 消息，正文最后且 agent_steps 为空。
#[tokio::test]
async fn agent_a03_steps_mode() {
    let base = spawn_named_db(&format!("chat_t_a03p_{}", uniq())).await;
    let a = register(&base, &format!("a03p_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "stepper").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    // 只设个人默认；Agent 覆盖保持 null（跟随默认）
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&a.access))
        .json(&json!({"output_mode": "steps"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let inv_id = a03_invoke(&conv, "steps:2", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;

    // 不推 agent_stream（每步消息走普通 message 帧）
    assert!(!frames.iter().any(|f| f["type"] == "agent_stream"));

    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 4, "触发 + 2 条步骤 + 正文: {list:?}");
    assert_eq!(list[1]["content"], "✓ 步骤 1");
    assert_eq!(list[2]["content"], "✓ 步骤 2");
    assert!(list[1]["agent"].is_object() && list[2]["agent"].is_object());
    let body = &list[3];
    assert!(body["agent"].is_object());
    assert!(body["content"].as_str().unwrap().ends_with("done"));
    assert!(
        body["agent_steps"].is_null()
            || body["agent_steps"].as_array().is_some_and(|a| a.is_empty()),
        "steps 模式正文的 agent_steps 应为空: {body:?}"
    );
    ha.shutdown().await;
}

/// stepfail：步骤记录 ok=false，调用仍 succeeded。
#[tokio::test]
async fn agent_a03_stepfail() {
    let base = spawn_named_db(&format!("chat_t_a03f_{}", uniq())).await;
    let a = register(&base, &format!("a03f_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "failer").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let inv_id = a03_invoke(&conv, "stepfail", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id)
        .unwrap();
    assert_eq!(term["invocation"]["status"], "succeeded");

    let msgs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = msgs["messages"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let steps = list[1]["agent_steps"].as_array().expect("应有步骤记录");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["ok"], false);
    assert_eq!(steps[0]["title"], "读取配置");
    ha.shutdown().await;
}

/// 限频：一次调用产生的 agent_stream 帧数 ≤ 时长/200ms + 少量余量。
#[tokio::test]
async fn agent_a03_stream_rate_limit() {
    let base = spawn_named_db(&format!("chat_t_a03r_{}", uniq())).await;
    let a = register(&base, &format!("a03r_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "dripper").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // drip:1500 → 假 agent 每 ~30ms 一个 chunk，全程 ~1.5s；不限频会产生 ~50 帧
    let t0 = std::time::Instant::now();
    let inv_id = a03_invoke(&conv, "drip:1500", &mut sa).await;
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(20)).await;
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let n = frames
        .iter()
        .filter(|f| f["type"] == "agent_stream")
        .count();
    let bound = elapsed_ms / 200 + 5;
    assert!(
        n >= 2 && n as u64 <= bound,
        "agent_stream 帧数 {n} 应 ≤ {bound}（时长 {elapsed_ms}ms /200ms + 余量）且 ≥2"
    );
    ha.shutdown().await;
}

/// 别的宿主伪造 invocation_event：被忽略，调用不受影响。
#[tokio::test]
async fn agent_a03_event_forgery_ignored() {
    let base = spawn_named_db(&format!("chat_t_a03g_{}", uniq())).await;
    let a = register(&base, &format!("a03g_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "real").await;
    assert_eq!(st, 200);
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // 先让真实调用跑起来（sleep:2000 保证伪造帧落在 running 窗口内）
    let inv_id = a03_invoke(&conv, "sleep:2000", &mut sa).await;

    // 另开一条宿主连接（不同 install_id → 不同 host_id）：伪造者
    let ticket: Value = http()
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let host_ws_url = format!(
        "{}/v1/ws?ticket={}&role=host",
        base.replacen("http://", "ws://", 1),
        ticket["ticket"].as_str().unwrap()
    );
    let (mut hb, _) = tokio_tungstenite::connect_async(host_ws_url).await.unwrap();
    hb.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),
               "label":"forge","platform":"windows"})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = until_frame(&mut hb, "host_ready", A01Dur::from_secs(10)).await;
    let forge_host = ready.iter().find(|f| f["type"] == "host_ready").unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(forge_host, host_id, "伪造者应是不相关的另一条宿主连接");

    // 伪造三类事件 + 伪造 finish：全部应被忽略
    for ev in [
        json!({"type":"text","text":"FORGED"}),
        json!({"type":"step_start","id":"evil-1","title":"伪造步骤","kind":"other"}),
        json!({"type":"step_end","id":"evil-1","ok":true}),
    ] {
        hb.send(WsMsg::Text(
            json!({"type":"invocation_event","invocation_id":inv_id,"event":ev})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    hb.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv_id,"ok":true,"text":"FORGED BODY"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // 自然终态（sleep:2000 → 真宿主回 done）
    let frames = until_inv_done(&mut sa, &inv_id, A01Dur::from_secs(15)).await;
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "agent_stream" && serde_json::to_string(f).unwrap().contains("FORGED")
        }),
        "伪造事件不得产生 agent_stream: {frames:?}"
    );
    let term = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id)
        .unwrap();
    // 伪造 finish 不会抢先终结；真宿主结束为 succeeded
    assert_eq!(term["invocation"]["status"], "succeeded");
    let agent_msg = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有定稿消息");
    assert_eq!(agent_msg["message"]["content"], "done");
    ha.shutdown().await;
}

// ===== Agent A17 =====
// 定时调用。到点用把 next_run_at 拨到过去来模拟，不改服务器时钟。

async fn a17_send(method: &str, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let c = http();
    let req = match method {
        "GET" => c.get(url),
        "POST" => c.post(url),
        "PATCH" => c.patch(url),
        "DELETE" => c.delete(url),
        other => panic!("method {other}"),
    };
    let req = req.header("authorization", authz(token));
    let req = if let Some(body) = body {
        req.json(&body)
    } else {
        req
    };
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(Value::Null);
    (status, body)
}

fn a17_daily(name: &str, prompt: &str, time: &str) -> Value {
    json!({
        "name": name,
        "prompt": prompt,
        "spec": {"type": "daily", "time": time},
        "timezone": "Asia/Shanghai",
    })
}

async fn a17_due(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "UPDATE agent_schedules SET next_run_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

async fn a17_due_hours(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "UPDATE agent_schedules SET next_run_at = now() - interval '3 hours' WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

async fn a17_wait_text(base: &str, u: &TestUser, conv: &str, exact: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    loop {
        let texts = agent_texts(&conv_messages(base, u, conv).await);
        if texts.iter().any(|t| t == exact) {
            return texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等定时回复 {exact:?} 超时，当前 {texts:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

fn a17_sched_reply(name: &str, body: &str) -> String {
    format!("⏰ 定时任务「{name}」\n{body}")
}

/// 1. 创建 / 列表 / 修改 / 删除；别人 404；不是成员 400；只读私聊 400；超过 50 条 400。
#[tokio::test]
async fn agent_a17_schedule_crud() {
    let base = spawn_named_db(&format!("chat_it_a17c_{}", uniq())).await;
    let a = register(&base, &format!("a17a_{}", uniq())).await;
    let b = register(&base, &format!("a17b_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "sched").await;
    assert_eq!(st, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, _) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &b.access,
        None,
    )
    .await;
    assert_eq!(code, 404, "别人列定时任务应 404");

    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "x",
            "prompt": "echo:x",
            "target_conversation_id": uuid::Uuid::new_v4().to_string(),
            "spec": {"type":"daily","time":"09:00"},
            "timezone": "Asia/Shanghai",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "not_member");

    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let un = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(un.status(), 204);
    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "只读",
            "prompt": "echo:x",
            "target_conversation_id": dm,
            "spec": {"type":"interval","minutes":30},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "readonly_dm");

    for (label, payload) in [
        (
            "空名",
            json!({"name":"","prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "超长名",
            json!({"name":"n".repeat(41),"prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "空指令",
            json!({"name":"ok","prompt":"","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "间隔太短",
            json!({"name":"ok","prompt":"echo:x","spec":{"type":"interval","minutes":4},"timezone":"UTC"}),
        ),
        (
            "坏时区",
            json!({"name":"ok","prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"Mars/Base"}),
        ),
    ] {
        let (code, body) = a17_send(
            "POST",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            Some(payload),
        )
        .await;
        assert_eq!(code, 400, "{label}: {body}");
    }

    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨报", "echo:ping", "09:00")),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    assert_eq!(created["created_by"], "user");
    assert_eq!(created["target_conversation_id"], conv);
    assert_eq!(created["enabled"], true);
    assert!(created["next_run_at"].is_string());
    let sid = created["id"].as_str().unwrap().to_string();

    let (code, _) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &b.access,
        Some(a17_daily("偷", "echo:x", "09:00")),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &b.access,
        Some(json!({"name":"no"})),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &b.access,
        None,
    )
    .await;
    assert_eq!(code, 404);

    let (code, patched) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        Some(json!({"name": "晨报改", "enabled": false})),
    )
    .await;
    assert_eq!(code, 200, "{patched}");
    assert_eq!(patched["name"], "晨报改");
    assert_eq!(patched["enabled"], false);

    let before = chrono::Utc::now();
    let (code, patched) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        Some(json!({
            "enabled": true,
            "spec": {"type":"interval","minutes":60},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{patched}");
    assert_eq!(patched["enabled"], true);
    assert_eq!(patched["spec"]["type"], "interval");
    let next = chrono::DateTime::parse_from_rfc3339(patched["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mins = (next - before).num_minutes();
    assert!(
        (59..=61).contains(&mins),
        "间隔 60 分钟的下次运行应在约 1 小时后，实际差 {mins} 分钟 ({next})"
    );

    let (code, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "晨报改");

    for i in 0..49 {
        let (code, body) = a17_send(
            "POST",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            Some(json!({
                "name": format!("n{i}"),
                "prompt": "echo:x",
                "spec": {"type":"interval","minutes":60},
                "timezone": "UTC",
            })),
        )
        .await;
        assert_eq!(code, 200, "第 {i} 条填充失败 {body}");
    }
    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "overflow",
            "prompt": "echo:x",
            "spec": {"type":"interval","minutes":60},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "too_many");

    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 204);
    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 49);
    assert!(list.as_array().unwrap().iter().all(|s| s["id"] != sid));
}

/// 2. 到点触发：回复带前缀；once 停用；daily 的下次运行是明天同一钟点。
#[tokio::test]
async fn agent_a17_due_fires_once_and_daily() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17f_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17f_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "duebot").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap();

    let (code, daily) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨报", "echo:ping", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{daily}");
    let daily_id = daily["id"].as_str().unwrap().to_string();

    let soon = chrono::Utc::now() + chrono::Duration::hours(2);
    let (code, once) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "一次",
            "prompt": "echo:oncebody",
            "spec": {"type":"once","at": soon.to_rfc3339()},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{once}");
    let once_id = once["id"].as_str().unwrap().to_string();

    a17_due(&st.pool, &daily_id).await;
    a17_due(&st.pool, &once_id).await;

    let daily_reply = a17_sched_reply("晨报", "ping");
    let once_reply = a17_sched_reply("一次", "oncebody");
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let texts = loop {
        let texts = agent_texts(&conv_messages(&base, &a, conv).await);
        if texts.iter().any(|t| t == &daily_reply) && texts.iter().any(|t| t == &once_reply) {
            break texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等两条定时回复超时 {texts:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert!(texts.contains(&daily_reply));
    assert!(texts.contains(&once_reply));

    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    let once_row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == once_id)
        .unwrap();
    assert_eq!(once_row["enabled"], false);
    assert!(once_row["next_run_at"].is_null(), "{once_row}");
    let daily_row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == daily_id)
        .unwrap();
    let got = chrono::DateTime::parse_from_rfc3339(daily_row["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    use chrono::{Datelike, TimeZone};
    let shanghai = chrono_tz::Asia::Shanghai;
    let now_local = chrono::Utc::now().with_timezone(&shanghai);
    let tomorrow = now_local.date_naive().succ_opt().unwrap();
    let expected = shanghai
        .with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 0, 0)
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let delta_ms = (got.timestamp_millis() - expected.timestamp_millis()).abs();
    assert!(
        delta_ms <= 1000,
        "daily 下次应是明天 00:00 上海，got {got} expected {expected}"
    );

    let invs = list_inv(&base, &a, conv).await;
    let sched_invs: Vec<&Value> = invs
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["trigger_message_id"].is_null())
        .collect();
    assert_eq!(sched_invs.len(), 2, "{invs}");
    assert!(sched_invs.iter().all(|r| r["schedule_id"].is_string()));

    let keys: Vec<String> =
        sqlx::query_scalar("SELECT context_key FROM agent_contexts WHERE agent_id = $1")
            .bind(uuid::Uuid::parse_str(aid).unwrap())
            .fetch_all(&st.pool)
            .await
            .unwrap();
    assert!(keys.contains(&format!("schedule:{daily_id}")), "{keys:?}");
    assert!(keys.contains(&format!("schedule:{once_id}")), "{keys:?}");

    let once_inv = sched_invs
        .iter()
        .find(|r| r["schedule_id"] == once_id)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{once_id}"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 204, "跑完的 once 应能删掉，历史调用保留");
    let invs = list_inv(&base, &a, conv).await;
    let kept = invs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == once_inv)
        .unwrap();
    assert!(
        kept["schedule_id"].is_null() || kept.get("schedule_id").is_none(),
        "{kept}"
    );
    assert!(kept["trigger_message_id"].is_null());
    host.shutdown().await;
}

/// 3. 宿主离线时到点 → queued，上线后执行并带前缀。
#[tokio::test]
async fn agent_a17_offline_queues_then_runs() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17q_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17q_{}", uniq())).await;
    let (conv, aid, install) = offline_agent(&base, &a, "offbot").await;
    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("离线", "echo:offbody", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    a17_due(&st.pool, created["id"].as_str().unwrap()).await;

    let deadline = std::time::Instant::now() + A01Dur::from_secs(15);
    let queued = loop {
        let rows = list_inv(&base, &a, &conv).await;
        if let Some(row) = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["status"] == "queued")
        {
            break row.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "离线到点应排队 {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert!(queued["trigger_message_id"].is_null());
    assert!(queued["schedule_id"].is_string());

    let (host, _) = spawn_host(&base, &a, install).await;
    let texts = a17_wait_text(&base, &a, &conv, &a17_sched_reply("离线", "offbody")).await;
    assert_eq!(texts, vec![a17_sched_reply("离线", "offbody")]);
    host.shutdown().await;
}

/// 4. Agent 删除后不再触发；主人离开目标群后停用；主人被禁用后停用。
#[tokio::test]
async fn agent_a17_guards_stop_the_schedule() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17g_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(3_600_000, std::sync::atomic::Ordering::Relaxed);
    let admin = register(&base, &format!("a17ad_{}", uniq())).await;
    let owner = register(&base, &format!("a17ow_{}", uniq())).await;
    let friend = register(&base, &format!("a17fr_{}", uniq())).await;

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, doomed) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(a17_daily("将删", "echo:gone", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{doomed}");
    assert_eq!(delete_agent_req(&base, &owner, &aid).await, 204);
    a17_due(&st.pool, doomed["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT enabled, disabled_reason FROM agent_schedules WHERE id = $1")
            .bind(uuid::Uuid::parse_str(doomed["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(row.0, false);
    assert_eq!(row.1.as_deref(), Some("agent_deleted"));
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE schedule_id = $1")
            .bind(uuid::Uuid::parse_str(doomed["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "已删除的 Agent 不应再被触发");
    assert!(agent_texts(&conv_messages(&base, &owner, &conv).await).is_empty());

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard2").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    host.shutdown().await;
    let gid = a11_group(&base, &owner, &[&friend]).await;
    let (code, group_sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(json!({
            "name": "群报",
            "prompt": "echo:nope",
            "target_conversation_id": gid,
            "spec": {"type":"daily","time":"00:00"},
            "timezone": "Asia/Shanghai",
        })),
    )
    .await;
    assert_eq!(code, 200, "{group_sched}");
    let leave = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap();
    assert_eq!(leave.status(), 204);
    a17_due(&st.pool, group_sched["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let (code, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        None,
    )
    .await;
    assert_eq!(code, 200, "{list}");
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == group_sched["id"])
        .unwrap();
    assert_eq!(row["enabled"], false);
    assert_eq!(row["disabled_reason"], "not_member");
    assert!(agent_texts(&conv_messages(&base, &friend, &gid).await).is_empty());

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard3").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    host.shutdown().await;
    let (code, sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(a17_daily("禁用", "echo:no", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{sched}");
    let dis = http()
        .post(format!("{base}/v1/admin/users/{}/disable", owner.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(dis.status(), 204);
    a17_due(&st.pool, sched["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT enabled, disabled_reason FROM agent_schedules WHERE id = $1")
            .bind(uuid::Uuid::parse_str(sched["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(row.0, false);
    assert_eq!(row.1.as_deref(), Some("owner_disabled"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE agent_id = $1")
        .bind(uuid::Uuid::parse_str(aid).unwrap())
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// 5. 错过多次只补一次；并发两次扫描只建一个调用。
#[tokio::test]
async fn agent_a17_one_fire_per_schedule() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17o_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(3_600_000, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17o_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "oncefire").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "间隔",
            "prompt": "echo:gap",
            "spec": {"type":"interval","minutes":30},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    let sid = created["id"].as_str().unwrap();
    a17_due_hours(&st.pool, sid).await;
    let before = chrono::Utc::now();
    let (left, right) = tokio::join!(
        chat_server::schedules::sweep_due_schedules(&st),
        chat_server::schedules::sweep_due_schedules(&st),
    );
    left.unwrap();
    right.unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE schedule_id = $1")
            .bind(uuid::Uuid::parse_str(sid).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "并发扫描只能建一个调用");
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["status"], "queued");

    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    let next = chrono::DateTime::parse_from_rfc3339(list[0]["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let after = chrono::Utc::now();
    assert!(
        next > before + chrono::Duration::minutes(29),
        "错过的间隔不应回填到过去，next={next} before={before}"
    );
    assert!(
        next < after + chrono::Duration::minutes(31),
        "下一次应约为现在加 30 分钟，next={next} after={after}"
    );
}

/// 6. 假 agent 经 MCP 调 create_schedule，列表里出现 created_by=agent。
#[tokio::test]
async fn agent_a17_tool_create_schedule() {
    let base = spawn_named_db(&format!("chat_it_a17t_{}", uniq())).await;
    let a = register(&base, &format!("a17t_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let (code, agent) = create_agent_with(&base, &a, &host_id, "toolbot", args).await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "t1", "tool:create_schedule").await;

    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let row = loop {
        let (code, list) = a17_send(
            "GET",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            None,
        )
        .await;
        assert_eq!(code, 200, "{list}");
        if let Some(row) = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["created_by"] == "agent")
        {
            break row.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "工具创建的定时任务没出现。列表 {list} 会话 {:?}",
            agent_texts(&conv_messages(&base, &a, &conv).await)
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert_eq!(row["name"], "from-agent");
    assert_eq!(row["target_conversation_id"], conv);
    assert_eq!(row["created_by"], "agent");
    host.shutdown().await;
}

// ===== Agent A07 =====
// 假登记表：本地 HTTP 提供 registry.json 和一个 zip。宿主的登记表地址指向它。

fn a07_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// 单个文件、不压缩的 zip。登记表测试不依赖额外的打包库。
fn a07_store_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let name_b = name.as_bytes();
    let crc = a07_crc32(data);
    let mut out = Vec::new();
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name_b);
    out.extend_from_slice(data);
    let central_at = out.len();
    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name_b);
    let central_len = out.len() - central_at;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(central_len as u32).to_le_bytes());
    out.extend_from_slice(&(central_at as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn a07_bin(archive: &str, sha: &str, cmd: &str) -> Value {
    json!({
        "archive": archive,
        "sha256": sha,
        "cmd": cmd,
        "args": []
    })
}

fn a07_agent(id: &str, name: &str, desc: &str, icon: &str, key: &str, bin: Value) -> Value {
    let mut binary = serde_json::Map::new();
    binary.insert(key.to_string(), bin);
    json!({
        "id": id,
        "name": name,
        "version": "1.0.0",
        "description": desc,
        "icon": icon,
        "distribution": {"binary": binary}
    })
}

async fn a07_spawn_host(
    base: &str,
    u: &TestUser,
    registry_url: &str,
    data_dir: std::path::PathBuf,
) -> (agent_host::HostHandle, String) {
    let h = agent_host::spawn(
        agent_host::HostConfig {
            server_url: base.into(),
            install_id: uuid::Uuid::new_v4(),
            label: "a07-host".into(),
            platform: "windows".into(),
            mcp_bridge: None,
            registry_url: registry_url.into(),
            data_dir,
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

async fn a07_registry(base: &str, u: &TestUser, host_id: &str) -> (u16, Value) {
    let r = http()
        .get(format!("{base}/v1/agent-hosts/{host_id}/registry"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

async fn a07_install(base: &str, u: &TestUser, host_id: &str, entry: &str, version: &str) -> u16 {
    http()
        .post(format!(
            "{base}/v1/agent-hosts/{host_id}/registry/{entry}/install"
        ))
        .header("authorization", authz(&u.access))
        .json(&json!({"version": version}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn a07_progress<S>(s: &mut S, entry: &str, stage: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等安装进度 {entry}/{stage} 超时");
        let frames = until_frame(s, "registry_install_progress", left).await;
        if let Some(f) = frames.into_iter().find(|f| {
            f["entry_id"] == entry
                && f["stage"] == stage
                && f["type"] == "registry_install_progress"
        }) {
            return f;
        }
    }
}

async fn a07_failed_error<S>(s: &mut S) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 failed 调用超时");
        let frames = until_frame(s, "agent_invocation", left).await;
        if let Some(err) = frames.iter().find_map(|f| {
            (f["invocation"]["status"] == "failed")
                .then(|| f["invocation"]["error"].as_str().unwrap_or("").to_string())
        }) {
            return err;
        }
    }
}

/// 列表三种状态、sha256 失败、装上 binary 后调用成功、离线 409、别人的宿主 404、
/// 未安装时调用 failed/registry_not_installed。
#[tokio::test]
async fn a07_registry_install_and_invoke() {
    let base = spawn_named_db(&format!("chat_it_a07_{}", uniq())).await;
    let a = register(&base, &format!("a07a_{}", uniq())).await;
    let b = register(&base, &format!("a07b_{}", uniq())).await;

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-host/tests/fake-acp-agent.mjs");
    // canonicalize 在 Windows 上会带 \\?\ 前缀，node 会把它解析成盘符目录。
    let script = script.canonicalize().unwrap();
    let script = script
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    let cmd = format!("@echo off\r\nnode \"{script}\" %*\r\n");
    let zip = a07_store_zip("run.cmd", cmd.as_bytes());
    let sha = agent_host::registry::sha256_hex(&zip);
    let bad_sha = "0".repeat(64);
    let platform = agent_host::registry::current_platform();
    let other = if platform == "darwin-aarch64" {
        "linux-x86_64"
    } else {
        "darwin-aarch64"
    };

    let data = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/wt")
        .join(format!("a07-data-{}", uniq()));
    let seeded = data.join("agents").join("bin").join("seeded").join("1.0.0");
    std::fs::create_dir_all(&seeded).unwrap();
    std::fs::write(seeded.join("run.cmd"), b"@echo off\r\n").unwrap();

    // 先占端口，JSON 里的 archive 才能写成这个地址。文件在 accept 之前放进表。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let registry = json!({
        "version": 1,
        "agents": [
            a07_agent("seeded", "Seeded", "already extracted", &format!("{origin}/icon.svg"), platform, a07_bin(&format!("{origin}/none.zip"), "", "run.cmd")),
            a07_agent("widget", "Widget", "install me", &format!("{origin}/icon.svg"), platform, a07_bin(&format!("{origin}/widget.zip"), &sha, "run.cmd")),
            a07_agent("maconly", "Mac Only", "wrong platform", "", other, a07_bin(&format!("{origin}/none.zip"), "", "run")),
            a07_agent("badsum", "Bad Sum", "sha mismatch", "", platform, a07_bin(&format!("{origin}/widget.zip"), &bad_sha, "run.cmd")),
        ]
    });
    let mut files: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    files.insert("/registry.json".into(), registry.to_string().into_bytes());
    files.insert("/widget.zip".into(), zip);
    files.insert(
        "/icon.svg".into(),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
    );
    let files = std::sync::Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let files = files.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/");
                let (status, body) = match files.get(path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", b"missing".to_vec()),
                };
                let ctype = if path.ends_with(".json") {
                    "application/json"
                } else if path.ends_with(".svg") {
                    "image/svg+xml"
                } else {
                    "application/octet-stream"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });

    let (host, host_id) =
        a07_spawn_host(&base, &a, &format!("{origin}/registry.json"), data.clone()).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, list) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 200, "{list}");
    let entries = list["entries"].as_array().unwrap();
    let status_of = |id: &str| {
        entries
            .iter()
            .find(|e| e["id"] == id)
            .unwrap_or_else(|| panic!("缺条目 {id}: {entries:?}"))["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(status_of("seeded"), "ready");
    assert_eq!(status_of("widget"), "installable");
    assert_eq!(status_of("maconly"), "unsupported");
    assert!(
        entries.iter().any(
            |e| e["id"] == "maconly" && e["reason"].as_str().unwrap_or("").contains("没有对应")
        ),
        "{entries:?}"
    );
    let kinds: std::collections::HashSet<_> = entries
        .iter()
        .map(|e| e["status"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains("ready") && kinds.contains("installable") && kinds.contains("unsupported")
    );

    // 登记表形状只校验格式：版本没装也能创建。
    let miss_name = format!("m{}", uniq());
    let created = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": miss_name,
            "host_id": host_id,
            "runtime": {"registry": {"id": "widget", "version": "9.9.9"}},
            "cwd": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let miss: Value = created.json().await.unwrap();
    assert_eq!(miss["runtime"]["registry"]["id"], "widget");
    assert_eq!(miss["runtime"]["registry"]["version"], "9.9.9");
    let miss_id = miss["id"].as_str().unwrap();
    let miss_conv = miss["conversation_id"].as_str().unwrap().to_string();

    let bad = http()
        .patch(format!("{base}/v1/agents/{miss_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"runtime": {"registry": {"id": "../x", "version": "1"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let bad: Value = bad.json().await.unwrap();
    assert_eq!(bad["error"], "bad_runtime");

    let (st, body) = a07_registry(&base, &b, &host_id).await;
    assert_eq!(st, 404, "别人的宿主应 404: {body}");
    assert_eq!(
        a07_install(&base, &b, &host_id, "widget", "1.0.0").await,
        404
    );
    let (st, _) = a07_registry(&base, &a, &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(st, 404);

    send_ws(&mut sa, &miss_conv, "miss1", "echo:nope").await;
    let err = a07_failed_error(&mut sa).await;
    assert_eq!(err, "registry_not_installed", "{err}");

    assert_eq!(
        a07_install(&base, &a, &host_id, "badsum", "1.0.0").await,
        202
    );
    let failed = a07_progress(&mut sa, "badsum", "失败").await;
    assert_eq!(failed["host_id"], host_id);
    assert!(
        failed["error"].as_str().unwrap_or("").contains("sha256"),
        "{failed}"
    );

    assert_eq!(
        a07_install(&base, &a, &host_id, "widget", "1.0.0").await,
        202
    );
    let done = a07_progress(&mut sa, "widget", "完成").await;
    assert_eq!(done["host_id"], host_id);
    assert!(
        done.get("error").is_none() || done["error"].is_null(),
        "{done}"
    );

    let (st, list) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 200, "{list}");
    let widget = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "widget")
        .unwrap();
    assert_eq!(widget["status"], "ready", "{widget}");

    let ok_name = format!("w{}", uniq());
    let created = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": ok_name,
            "host_id": host_id,
            "runtime": {"registry": {"id": "widget", "version": "1.0.0"}},
            "cwd": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let agent: Value = created.json().await.unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    send_ws(&mut sa, &conv, "ok1", "echo:pong-a07").await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        buf.extend(frames_for(&mut sa, left.min(A01Dur::from_millis(500))).await);
        let failed = buf
            .iter()
            .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "failed");
        if let Some(f) = failed {
            panic!("登记表 Agent 调用失败: {}", f["invocation"]["error"]);
        }
        let replied = buf.iter().any(|f| {
            f["type"] == "message"
                && f["message"]["agent"].is_object()
                && f["message"]["content"] == "pong-a07"
        });
        let succeeded = buf.iter().any(|f| {
            f["type"] == "agent_invocation"
                && f["invocation"]["status"] == "succeeded"
                && f["invocation"]["agent_id"] == agent["id"]
        });
        if replied && succeeded {
            break;
        }
    }
    let replied = buf.iter().any(|f| {
        f["type"] == "message"
            && f["message"]["agent"].is_object()
            && f["message"]["content"] == "pong-a07"
    });
    if !replied {
        let msgs: Value = http()
            .get(format!("{base}/v1/conversations/{conv}/messages"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        panic!("没有看到 Agent 回复。ws={buf:?} rest={msgs}");
    }

    host.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(400)).await;
    let (st, body) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(body["error"], "host_offline");
    assert_eq!(
        a07_install(&base, &a, &host_id, "widget", "1.0.0").await,
        409
    );
}

// ===== Agent A12 =====
// 权限档位 + 审批卡片。真宿主 + 假 ACP agent 的 perm:<kind>:<路径>。

async fn decide_approval(base: &str, u: &TestUser, id: &str, option_id: &str) -> (u16, Value) {
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

async fn wait_pending_approval(base: &str, u: &TestUser, conv: &str) -> Value {
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

fn latest_inv_status(rows: &Value) -> Option<&str> {
    rows.as_array()?.first()?.get("status")?.as_str()
}

/// 1 + 7：只读档位 read 自动放行且没有卡片；edit 出卡片、调用变 awaiting_approval，并计入未读。
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
    send_ws(&mut sa, &conv, "r1", "perm:read:/x").await;
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

// ===== Agent A05 =====
// 附件进出 Agent 专属会话：主人的附件消息是「待交付文件」，不触发调用；
// 下一条文字指令把它们带走（invocation_start.attachments + InvocationView.attachment_ids，
// 上限 10 个）。Agent 产出文件经 /v1/agent-tools/files（Invocation 凭据）上传，
// invocation_finish.files 回传后服务端按序发 Agent 的附件消息。

/// 带附件的 send_message（attachment_id 字段；content 为空即纯附件消息）
async fn send_ws_att<S>(s: &mut S, conv: &str, cmid: &str, att: &str, content: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    s.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":cmid,
               "content":content,"attachment_id":att})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

/// POST /v1/agent-tools/files：返回裸 Response（要断言 200/401/413）
async fn tool_upload(
    base: &str,
    auth: &str,
    name: &str,
    mime: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(body)
        .file_name(name.to_string())
        .mime_str(mime)
        .unwrap();
    http()
        .post(format!("{base}/v1/agent-tools/files"))
        .header("authorization", auth)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

/// 收集用户侧帧直到指定调用到终态；inv_id 为空时只看第一个终态调用
async fn until_inv_terminal(s: &mut RawHostWs, inv_id: &str) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        frames.extend(until_frame(s, "agent_invocation", A01Dur::from_secs(20)).await);
        if frames.iter().any(|f| {
            f["type"] == "agent_invocation"
                && (inv_id.is_empty() || f["invocation"]["id"] == inv_id)
                && matches!(
                    f["invocation"]["status"].as_str(),
                    Some("succeeded") | Some("failed") | Some("cancelled") | Some("timed_out")
                )
        }) {
            return frames;
        }
    }
}

/// 主路径：附件消息不触发调用；下一条文字指令把待交付附件交给 Agent。
/// 假 agent inspect 回报 image:/link: 两行 → 证明 host 收到了 attachments 元数据、
/// 下载了文件并按 image 能力构造了提示块。
#[tokio::test]
async fn a05_attachments_ride_next_text() {
    let base = spawn_named_db(&format!("chat_t_a05a_{}", uniq())).await;
    let a = register(&base, &format!("a05a_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "filesaw").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let png = png_fixture(64, 48);
    let png_len = png.len();
    let (st, p) = upload_att(&base, &conv, &a.access, "shot.png", "image/png", png).await;
    assert_eq!(st, 200, "{p}");
    let png_id = p["id"].as_str().unwrap().to_string();
    let (st, t) = upload_att(
        &base,
        &conv,
        &a.access,
        "notes.txt",
        "text/plain",
        b"note-body".to_vec(),
    )
    .await;
    assert_eq!(st, 200, "{t}");
    let txt_id = t["id"].as_str().unwrap().to_string();

    // 附件消息（含一条带文字附言的）不产生调用
    send_ws_att(&mut sa, &conv, "a05-p1", &png_id, "").await;
    send_ws_att(&mut sa, &conv, "a05-p2", &txt_id, "先看这两个文件").await;
    let quiet = frames_for(&mut sa, A01Dur::from_millis(800)).await;
    assert!(
        !quiet.iter().any(|f| f["type"] == "agent_invocation"),
        "附件消息不应触发调用: {quiet:?}"
    );
    assert!(
        quiet
            .iter()
            .filter(|f| f["type"] == "message" && f["message"]["attachment"].is_object())
            .count()
            >= 2,
        "附件消息本身仍应广播: {quiet:?}"
    );

    // 下一条文字 → 调用带走两个附件（按消息 seq 顺序）
    send_ws(&mut sa, &conv, "a05-c1", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();
    let ids: Vec<String> = inv["invocation"]["attachment_ids"]
        .as_array()
        .expect("agent_invocation 帧应带 attachment_ids")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![png_id.clone(), txt_id.clone()]);

    let frames = until_inv_terminal(&mut sa, &inv_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    // PNG 走了 image 块（假 agent 声明了 promptCapabilities.image）
    assert!(
        content.contains(&format!("image:image/png:{png_len}")),
        "{content}"
    );
    // 文本文件走 resource_link，且文件确实下载到了 in/
    assert!(content.contains("link:notes.txt:true"), "{content}");

    // REST 读模型同样带 attachment_ids
    let rows: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<String> = rows.as_array().unwrap()[0]["attachment_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![png_id, txt_id]);

    ha.shutdown().await;
}

/// --no-image：agent 未声明 image 能力时图片也走 resource_link
#[tokio::test]
async fn a05_no_image_capability_uses_link() {
    let base = spawn_named_db(&format!("chat_t_a05b_{}", uniq())).await;
    let a = register(&base, &format!("a05b_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--no-image".into());
    let (st, agent) = create_agent_with(&base, &a, &host_id, "nopic", args).await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, p) = upload_att(
        &base,
        &conv,
        &a.access,
        "pic.png",
        "image/png",
        png_fixture(20, 20),
    )
    .await;
    assert_eq!(st, 200, "{p}");
    let png_id = p["id"].as_str().unwrap().to_string();
    send_ws_att(&mut sa, &conv, "a05-n1", &png_id, "").await;
    let _ = next_of(&mut sa, "message").await;

    send_ws(&mut sa, &conv, "a05-n2", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv_id = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap()["invocation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    assert!(content.contains("link:pic.png:true"), "{content}");
    assert!(!content.contains("image:"), "{content}");

    ha.shutdown().await;
}

/// 附件只被带走一次：第二次 inspect 不再收到上次的附件
#[tokio::test]
async fn a05_attachments_consumed_once() {
    let base = spawn_named_db(&format!("chat_t_a05c_{}", uniq())).await;
    let a = register(&base, &format!("a05c_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "once").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, p) = upload_att(
        &base,
        &conv,
        &a.access,
        "a.txt",
        "text/plain",
        b"1".to_vec(),
    )
    .await;
    assert_eq!(st, 200, "{p}");
    let att_id = p["id"].as_str().unwrap().to_string();
    send_ws_att(&mut sa, &conv, "a05-o1", &att_id, "").await;
    let _ = next_of(&mut sa, "message").await;

    send_ws(&mut sa, &conv, "a05-o2", "inspect").await;
    let frames = until_inv_terminal(&mut sa, "").await;
    let inv1 = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .unwrap();
    assert_eq!(
        inv1["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 第二次 inspect：不带附件
    send_ws(&mut sa, &conv, "a05-o3", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv2 = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    let inv2_id = inv2["invocation"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        inv2["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "已交付的附件不应再次携带"
    );
    let frames = until_inv_terminal(&mut sa, &inv2_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("(no attachments)"),
        "第二次 inspect 不该看到上次附件: {content}"
    );

    ha.shutdown().await;
}

/// 边界：待交付附件上限 10；超出部分继续待交付，被再下一条指令带走。
/// 用裸宿主直接看 invocation_start.attachments 的形状（id/name/mime/size）。
#[tokio::test]
async fn a05_pending_attachments_capped_at_10() {
    let base = spawn_named_db(&format!("chat_t_a05d_{}", uniq())).await;
    let a = register(&base, &format!("a05d_{}", uniq())).await;
    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "capper").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..12 {
        let (st, p) = upload_att(
            &base,
            &conv,
            &a.access,
            &format!("f{i:02}.txt"),
            "text/plain",
            format!("b{i}").into_bytes(),
        )
        .await;
        assert_eq!(st, 200, "{p}");
        ids.push(p["id"].as_str().unwrap().to_string());
        send_ws_att(&mut su, &conv, &format!("a05-k{i}"), &ids[i], "").await;
    }
    let quiet = frames_for(&mut su, A01Dur::from_millis(800)).await;
    assert!(
        !quiet.iter().any(|f| f["type"] == "agent_invocation"),
        "12 条附件消息都不该触发: {quiet:?}"
    );

    // 第 13 条消息是文字 → 触发调用，只带前 10 个
    send_ws(&mut su, &conv, "a05-cmd", "go").await;
    let host_frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start = host_frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let atts = start["attachments"]
        .as_array()
        .expect("invocation_start 应带 attachments");
    assert_eq!(atts.len(), 10, "附件上限 10: {atts:?}");
    for (i, att) in atts.iter().enumerate() {
        assert_eq!(att["id"].as_str().unwrap(), ids[i]);
        assert_eq!(att["name"].as_str().unwrap(), format!("f{i:02}.txt"));
        assert_eq!(att["mime"].as_str().unwrap(), "text/plain");
        assert_eq!(att["size"].as_i64().unwrap(), 2);
    }
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();

    // 用户侧帧同样携带 10 个 attachment_ids
    let frames = until_frame(&mut su, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(
        inv["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    host_finish(&mut sh, &inv_id).await;
    let _ = until_inv_terminal(&mut su, &inv_id).await;

    // 剩余 2 个仍处于待交付：下一条文字带走
    send_ws(&mut su, &conv, "a05-cmd2", "go2").await;
    let host_frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start2 = host_frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let atts2 = start2["attachments"].as_array().unwrap();
    assert_eq!(atts2.len(), 2, "剩余附件应继续待交付: {atts2:?}");
    assert_eq!(atts2[0]["id"].as_str().unwrap(), ids[10]);
    assert_eq!(atts2[1]["id"].as_str().unwrap(), ids[11]);
    let inv2 = start2["invocation_id"].as_str().unwrap().to_string();
    host_finish(&mut sh, &inv2).await;
    let _ = until_inv_terminal(&mut su, &inv2).await;
}

/// Agent 产出文件：先正文后附件消息；附件可下载；占用主人的存储配额
#[tokio::test]
async fn a05_agent_output_file_becomes_message() {
    let base = spawn_named_db(&format!("chat_t_a05e_{}", uniq())).await;
    let a = register(&base, &format!("a05e_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "writer").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let st0: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let used0 = st0["used_bytes"].as_i64().unwrap();

    send_ws(&mut sa, &conv, "a05-m1", "makefile:report.txt:hello").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv_id = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap()["invocation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;

    // 两条 Agent 消息：先文本 "ok"，再文件消息
    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    assert_eq!(agent_msgs.len(), 2, "应是一条正文+一条附件消息: {frames:?}");
    assert_eq!(agent_msgs[0]["message"]["content"], "ok");
    assert!(agent_msgs[0]["message"]["attachment"].is_null());
    let fm = &agent_msgs[1]["message"];
    assert_eq!(fm["kind"], "file");
    assert_eq!(fm["agent"]["id"], agent_id.as_str());
    assert_eq!(fm["attachment"]["file_name"], "report.txt");
    assert_eq!(fm["attachment"]["mime"], "text/plain");
    let att_id = fm["attachment"]["id"].as_str().unwrap().to_string();
    assert!(
        fm["seq"].as_i64().unwrap() > agent_msgs[0]["message"]["seq"].as_i64().unwrap(),
        "附件消息应在正文之后"
    );

    // 能力 URL 下载到写入的内容
    let data = http()
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&data[..], b"hello");

    // 上传者=主人：存储用量增加 5 字节
    let st1: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st1["used_bytes"].as_i64().unwrap(), used0 + 5);

    ha.shutdown().await;
}

/// 产出文件超配额：调用仍成功，正文追加「未发送」提示，不发附件消息
#[tokio::test]
async fn a05_output_over_quota_skipped() {
    let base = spawn_named_db(&format!("chat_t_a05f_{}", uniq())).await;
    let admin = register(&base, &format!("a05f_{}", uniq())).await;
    let r = admin_put_settings(&base, &admin, json!({"storage_quota_mb": 1})).await;
    assert_eq!(r.status(), 200);
    let (ha, host_id) = spawn_host(&base, &admin, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &admin, &host_id, "fat").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();

    // 1.5MB 产出 > 1MB 配额 → 服务端 413 → files_skipped=1
    send_ws(&mut sa, &conv, "a05-q1", "makebig:big.bin:1572864").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;

    let fin = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id.as_str())
        .unwrap();
    assert_eq!(fin["invocation"]["status"], "succeeded");

    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    // 只有一条正文，追加配额提示；无附件消息
    assert_eq!(agent_msgs.len(), 1, "{frames:?}");
    let content = agent_msgs[0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("ok"), "{content}");
    assert!(content.contains("1 个文件"), "{content}");
    assert!(content.contains("未发送"), "{content}");

    ha.shutdown().await;
}

/// 凭据与归属：tools_token 只在调用存活期有效；finish 里别调用的附件 id 被忽略
#[tokio::test]
async fn a05_tool_upload_scoped_to_invocation() {
    let base = spawn_named_db(&format!("chat_t_a05g_{}", uniq())).await;
    let a = register(&base, &format!("a05g_{}", uniq())).await;
    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "tooler2").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // 无凭据 / 普通 Bearer 都被拒
    let r = tool_upload(&base, "", "a.txt", "text/plain", b"x".to_vec()).await;
    assert_eq!(r.status(), 401);
    let r = tool_upload(
        &base,
        &format!("Bearer {}", a.access),
        "a.txt",
        "text/plain",
        b"x".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 401);

    // inv1 running：上传成功，归属=主人+专属会话+inv1
    send_ws(&mut su, &conv, "a05-s1", "t1").await;
    let f = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s1 = f.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv1 = s1["invocation_id"].as_str().unwrap().to_string();
    let tok1 = s1["tools_token"].as_str().unwrap().to_string();
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "a.txt",
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let att_a = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();

    // inv2 running：att_a 属于 inv1，写进 inv2 的 finish.files 被忽略
    send_ws(&mut su, &conv, "a05-s2", "t2").await;
    let f = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s2 = f.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv2 = s2["invocation_id"].as_str().unwrap().to_string();
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv2,"ok":true,"text":"done",
               "files":[att_a]})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let frames = until_inv_terminal(&mut su, &inv2).await;
    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    assert_eq!(agent_msgs.len(), 1, "越权附件 id 不应产生消息: {frames:?}");
    assert_eq!(agent_msgs[0]["message"]["content"], "done");

    // inv1 还活着：凭据仍可用；finish 里写自己的两个文件 → 两条附件消息按序到达
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "b.txt",
        "text/plain",
        b"yo".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let att_b = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv1,"ok":true,"text":"done1",
               "files":[att_a, att_b]})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let frames = until_inv_terminal(&mut su, &inv1).await;
    let file_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["attachment"].is_object())
        .collect();
    assert_eq!(file_msgs.len(), 2, "{frames:?}");
    assert_eq!(file_msgs[0]["message"]["attachment"]["file_name"], "a.txt");
    assert_eq!(file_msgs[1]["message"]["attachment"]["file_name"], "b.txt");

    // inv1 结束后同一凭据再上传 → 401
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "c.txt",
        "text/plain",
        b"late".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 401);
}

// ===== Agent A14 =====
// 同一主人的两个宿主：Agent a 在宿主 1，Agent b 在宿主 2。假 agent 带 --mcp=http。

fn a14_args() -> Vec<String> {
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    args
}

struct A14Env {
    base: String,
    user: TestUser,
    h1: agent_host::HostHandle,
    h2: agent_host::HostHandle,
    host2: String,
    install2: uuid::Uuid,
    a_id: String,
    b_id: String,
    a_conv: String,
    b_conv: String,
}

async fn a14_pair(tag: &str) -> A14Env {
    let base = spawn_named_db(&format!("chat_it_a14{tag}_{}", uniq())).await;
    let user = register(&base, &format!("a14{tag}_{}", uniq())).await;
    let (h1, host1) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let install2 = uuid::Uuid::new_v4();
    let (h2, host2) = spawn_host(&base, &user, install2).await;
    let (st, a) = create_agent_with(&base, &user, &host1, "a", a14_args()).await;
    assert_eq!(st, 200, "{a}");
    let (st, b) = create_agent_with(&base, &user, &host2, "b", a14_args()).await;
    assert_eq!(st, 200, "{b}");
    A14Env {
        base,
        user,
        h1,
        h2,
        host2,
        install2,
        a_id: a["id"].as_str().unwrap().to_string(),
        b_id: b["id"].as_str().unwrap().to_string(),
        a_conv: a["conversation_id"].as_str().unwrap().to_string(),
        b_conv: b["conversation_id"].as_str().unwrap().to_string(),
    }
}

async fn a14_user_ws(env: &A14Env) -> RawHostWs {
    let (s, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    s
}

/// 等到某条 Agent 消息满足 pred，返回正文。
async fn wait_agent_msg(
    base: &str,
    u: &TestUser,
    conv: &str,
    pred: impl Fn(&str) -> bool,
    timeout: A01Dur,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(hit) = msgs.iter().find_map(|m| {
            if !m["agent"].is_object() {
                return None;
            }
            let c = m["content"].as_str().unwrap_or("");
            pred(c).then(|| c.to_string())
        }) {
            return hit;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等 Agent 消息超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn agent_host_online(base: &str, u: &TestUser, agent_id: &str) -> bool {
    let list: Value = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == agent_id)
        .unwrap()["host_online"]
        .as_bool()
        .unwrap()
}

/// 1. a 同步调用 b，回复是 b 的正文；b 的专属会话有「被 @a 调用」记录；父调用 id 对得上。
#[tokio::test]
async fn a14_sync_call_cross_host() {
    let env = a14_pair("sync").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "c1", "call:b:echo:hi").await;
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c == "hi",
        A01Dur::from_secs(45),
    )
    .await;
    assert_eq!(reply, "hi");

    let bmsgs = conv_messages(&env.base, &env.user, &env.b_conv).await;
    assert!(
        bmsgs.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .contains("📨 被 @a 调用")),
        "b 的专属会话应有被调用记录: {bmsgs:?}"
    );
    assert!(
        bmsgs
            .iter()
            .any(|m| m["agent"].is_object() && m["content"] == "hi"),
        "b 自己的回复应是 hi: {bmsgs:?}"
    );

    let a_rows = list_inv(&env.base, &env.user, &env.a_conv).await;
    let parent = a_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_id"] == env.a_id)
        .unwrap();
    assert!(parent["parent_invocation_id"].is_null());
    let b_rows = list_inv(&env.base, &env.user, &env.b_conv).await;
    let child = b_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["parent_invocation_id"] == parent["id"])
        .expect("子调用应挂在父调用上");
    assert_eq!(child["agent_name"], "b");
    assert_eq!(child["root_conversation_id"], env.a_conv.as_str());
    assert_eq!(child["status"], "succeeded");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 2. 并行派出两个 sleep，总耗时明显小于串行。
#[tokio::test]
async fn a14_parallel_calls() {
    let env = a14_pair("par").await;
    let (st, cagent) = create_agent_with(&env.base, &env.user, &env.host2, "c", a14_args()).await;
    assert_eq!(st, 200, "{cagent}");

    let mut sa = a14_user_ws(&env).await;
    let t0 = std::time::Instant::now();
    send_ws(&mut sa, &env.a_conv, "p1", "callasync:b,c:sleep:3000").await;
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("done"),
        A01Dur::from_secs(30),
    )
    .await;
    let elapsed = t0.elapsed();
    assert_eq!(reply, "done|done", "{reply}");
    assert!(
        elapsed < A01Dur::from_millis(8000),
        "并行两次 3s 应明显短于串行（6s+），实际 {elapsed:?}"
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

fn ctx_of(reply: &str) -> String {
    let rest = reply.split("ctx=").nth(1).unwrap_or("");
    rest.split_whitespace().next().unwrap_or("").to_string()
}

/// 3. 带上下文号接着聊；新开的上下文 recall 是空的。
#[tokio::test]
async fn a14_context_resume() {
    let env = a14_pair("ctx").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "k1", "callctx:b:new:echo:one").await;
    let first = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.starts_with("ctx=") && c.contains("result=one"),
        A01Dur::from_secs(45),
    )
    .await;
    let ctx = ctx_of(&first);
    assert!(!ctx.is_empty(), "{first}");

    send_ws(
        &mut sa,
        &env.a_conv,
        "k2",
        &format!("callctx:b:{ctx}:recall"),
    )
    .await;
    let second = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("result=") && c.contains("echo:one"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(second.contains(&format!("ctx={ctx}")), "{second}");

    send_ws(&mut sa, &env.a_conv, "k3", "callctx:b:new:recall").await;
    let third = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("result=(empty)"),
        A01Dur::from_secs(45),
    )
    .await;
    assert_ne!(ctx_of(&third), ctx, "新开的上下文号不应和上次相同: {third}");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

async fn put_agent_settings(base: &str, u: &TestUser, body: Value) -> (u16, Value) {
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

/// 4. 深度上限 1：a→b 可以，b→c 被 400，b 的回复里能看到工具错误。
#[tokio::test]
async fn a14_depth_limit() {
    let env = a14_pair("dep").await;
    let (st, cagent) = create_agent_with(&env.base, &env.user, &env.host2, "c", a14_args()).await;
    assert_eq!(st, 200, "{cagent}");
    let c_conv = cagent["conversation_id"].as_str().unwrap().to_string();

    let settings: Value = http()
        .get(format!("{}/v1/users/me/agent-settings", env.base))
        .header("authorization", authz(&env.user.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["max_call_depth"], 3);
    let (st, body) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 0})).await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"], "bad_max_call_depth");
    let (st, _) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 11})).await;
    assert_eq!(st, 400);
    let (st, body) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 1})).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["max_call_depth"], 1);
    assert_eq!(body["output_mode"], "stream");

    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "d1", "call:b:call:c:echo:hi").await;
    let b_err = wait_agent_msg(
        &env.base,
        &env.user,
        &env.b_conv,
        |c| c.contains("depth_exceeded"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(b_err.contains("depth_exceeded"), "{b_err}");
    let c_rows = list_inv(&env.base, &env.user, &c_conv).await;
    assert!(
        c_rows.as_array().unwrap().is_empty(),
        "超深度不应创建 c 的调用: {c_rows}"
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 5. b 只读，写文件要审批；卡片在 b 的专属会话；允许后 a 拿到结果。
#[tokio::test]
async fn a14_callee_approval() {
    let env = a14_pair("appr").await;
    assert_eq!(
        http()
            .get(format!("{}/v1/agents", env.base))
            .header("authorization", authz(&env.user.access))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == env.b_id)
            .unwrap()["permission_tier"],
        "read_only"
    );
    let mut sa = a14_user_ws(&env).await;
    send_ws(
        &mut sa,
        &env.a_conv,
        "a1",
        "call:b:perm:edit:C:/outside/a14.txt",
    )
    .await;
    let card = wait_pending_approval(&env.base, &env.user, &env.b_conv).await;
    assert!(
        card["content"].as_str().unwrap_or("").contains("a14.txt"),
        "{card}"
    );
    let a_msgs = conv_messages(&env.base, &env.user, &env.a_conv).await;
    assert!(
        a_msgs.iter().all(|m| m["kind"] != "approval"),
        "审批卡片不应出现在 a 的会话: {a_msgs:?}"
    );
    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&env.base, &env.user, id, "allow").await;
    assert_eq!(st, 200, "{body}");
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("allowed:allow"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(reply.contains("allowed:allow"), "{reply}");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 6. b 的宿主离线 → 子调用排队；a 还在等；宿主回来后 a 拿到结果。
#[tokio::test]
async fn a14_offline_callee_queues() {
    let A14Env {
        base,
        user,
        h1,
        h2,
        install2,
        b_id,
        a_conv,
        b_conv,
        ..
    } = a14_pair("off").await;
    h2.shutdown().await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(10);
    while agent_host_online(&base, &user, &b_id).await {
        assert!(std::time::Instant::now() < deadline, "宿主 2 没有离线");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    send_ws(&mut sa, &a_conv, "o1", "call:b:echo:from-offline").await;
    let queued = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let rows = list_inv(&base, &user, &b_conv).await;
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "queued")
        {
            break;
        }
        assert!(std::time::Instant::now() < queued, "子调用没有排队: {rows}");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    let a_rows = list_inv(&base, &user, &a_conv).await;
    // A15：同一列表里还有 root 落在本会话的子调用，且按创建时间倒序排在前面。
    let parent = a_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_name"] == "a" && r["parent_invocation_id"].is_null())
        .expect("父调用应还在 a 的会话里");
    assert_eq!(parent["status"], "running", "{a_rows}");

    let (h2, _) = spawn_host(&base, &user, install2).await;
    let reply = wait_agent_msg(
        &base,
        &user,
        &a_conv,
        |c| c == "from-offline",
        A01Dur::from_secs(45),
    )
    .await;
    assert_eq!(reply, "from-offline");
    h1.shutdown().await;
    h2.shutdown().await;
}

/// 7. 取消父调用，子调用级联 cancelled。
#[tokio::test]
async fn a14_cancel_cascades() {
    let env = a14_pair("can").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "x1", "call:b:sleep:20000").await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let parent_id = loop {
        let rows = list_inv(&env.base, &env.user, &env.b_conv).await;
        let child = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["status"] == "running");
        if let Some(child) = child {
            break child["parent_invocation_id"].as_str().unwrap().to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "子调用没有跑起来: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    let (st, body) = cancel_inv(&env.base, &env.user, &parent_id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    let deadline = std::time::Instant::now() + A01Dur::from_secs(15);
    loop {
        let rows = list_inv(&env.base, &env.user, &env.b_conv).await;
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "cancelled")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "子调用没有被级联取消: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 8. 查别人派出的子调用 404；调别人的 Agent / 不存在的名字 404。
#[tokio::test]
async fn a14_tool_scope_404() {
    let base = spawn_named_db(&format!("chat_it_a14nf_{}", uniq())).await;
    let owner = register(&base, &format!("a14n_{}", uniq())).await;
    let other = register(&base, &format!("a14o_{}", uniq())).await;
    let (mut sh_a, host_a) = spawn_raw_host(&base, &owner).await;
    let (_sh_b, host_b) = spawn_raw_host(&base, &owner).await;
    let (_sh_o, host_o) = spawn_raw_host(&base, &other).await;
    let (st, agent_a) = create_agent(&base, &owner, &host_a, "a").await;
    assert_eq!(st, 200, "{agent_a}");
    let (st, _) = create_agent(&base, &owner, &host_b, "b").await;
    assert_eq!(st, 200);
    let (st, _) = create_agent(&base, &other, &host_o, "foreign").await;
    assert_eq!(st, 200);
    let conv = agent_a["conversation_id"].as_str().unwrap();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    send_ws(&mut su, conv, "t1", "echo:hold").await;
    let start1 = until_frame(&mut sh_a, "invocation_start", A01Dur::from_secs(10)).await;
    let token1 = start1
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap()["tools_token"]
        .as_str()
        .unwrap()
        .to_string();
    send_ws(&mut su, conv, "t2", "echo:hold2").await;
    let start2 = until_frame(&mut sh_a, "invocation_start", A01Dur::from_secs(10)).await;
    let token2 = start2
        .iter()
        .rev()
        .find(|f| f["type"] == "invocation_start")
        .unwrap()["tools_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(token1, token2);

    let auth1 = format!("Invocation {token1}");
    let call = |auth: &str, name: &str| {
        let base = base.clone();
        let auth = auth.to_string();
        let name = name.to_string();
        async move {
            http()
                .post(format!("{base}/v1/agent-tools/invocations"))
                .header("authorization", auth)
                .json(&json!({"agent_name": name, "prompt": "echo:x"}))
                .send()
                .await
                .unwrap()
        }
    };
    let r = call(&auth1, "foreign").await;
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "agent not found");
    let r = call(&auth1, "no-such").await;
    assert_eq!(r.status(), 404);
    let r = call(&auth1, "B").await;
    assert_eq!(r.status(), 200, "大小写不敏感，应能调到自己的 b");
    let created: Value = r.json().await.unwrap();
    let child = created["invocation_id"].as_str().unwrap();

    let r = http()
        .get(format!("{base}/v1/agent-tools/invocations/{child}?wait=0"))
        .header("authorization", format!("Invocation {token2}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "invocation not found"
    );

    let r = http()
        .get(format!("{base}/v1/agent-tools/invocations/{child}?wait=0"))
        .header("authorization", &auth1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let view: Value = r.json().await.unwrap();
    assert_eq!(view["invocation_id"], child);

    let r = http()
        .get(format!(
            "{base}/v1/agent-tools/invocations/{child}?wait=601"
        ))
        .header("authorization", &auth1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "bad_wait");
}

/// 9. 根会话（群）里的另一名成员收到子调用帧，带 parent_invocation_id 和 agent_name。
#[tokio::test]
async fn a14_root_members_see_child_frame() {
    let env = a14_pair("root").await;
    let friend = register(&env.base, &format!("a14f_{}", uniq())).await;
    befriend(&env.base, &env.user, &friend).await;
    let group: Value = http()
        .post(format!("{}/v1/conversations/group", env.base))
        .header("authorization", authz(&env.user.access))
        .json(&json!({"name": "a14", "member_ids": [friend.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = group["id"].as_str().unwrap().to_string();
    let (mut sf, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &friend).await)
        .await
        .unwrap();
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "g1",
            "content": "call:b:echo:hi",
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    let mut seen = Vec::new();
    let hit = 'find: loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "群成员没收到子调用帧: {seen:?}");
        let frames = until_frame(&mut sf, "agent_invocation", left).await;
        for f in frames {
            if f["type"] != "agent_invocation" {
                continue;
            }
            seen.push(f.clone());
            let inv = &f["invocation"];
            if inv["agent_name"] == "b" && inv["parent_invocation_id"].is_string() {
                break 'find f;
            }
        }
    };
    assert_eq!(hit["invocation"]["agent_name"], "b");
    assert!(hit["invocation"]["parent_invocation_id"].is_string());
    assert_eq!(hit["invocation"]["root_conversation_id"], gid.as_str());
    assert_eq!(hit["invocation"]["conversation_id"], env.b_conv.as_str());
    let dump = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/wt/a14-frames.json");
    let _ = std::fs::create_dir_all(dump.parent().unwrap());
    let _ = std::fs::write(
        &dump,
        serde_json::to_string_pretty(&seen).unwrap_or_default(),
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

// ===== Agent A16 =====
// 搜索 / 读取主人参与的其他会话，并以当前 Agent 身份往这些会话发消息。
// 假 agent：tool:list / tool:search:<词> / tool:read:<会话> / tool:send:<会话>:<正文>
// / tool:sendfile:<会话>:<文件名>:<内容>。发消息在 read_only 档先走审批。

async fn a16_mcp_agent(
    base: &str,
    u: &TestUser,
    name: &str,
    tier: &str,
) -> (agent_host::HostHandle, String, String) {
    let (h, host_id) = spawn_host(base, u, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .json(&json!({
            "name": name,
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
            "permission_tier": tier,
        }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let agent: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    (h, agent_id, conv)
}

async fn a16_group(base: &str, owner: &TestUser, peer: &TestUser, name: &str) -> String {
    let r = http()
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&owner.access))
        .json(&json!({"name": name, "member_ids": [peer.id]}))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let detail: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "{detail}");
    detail["id"].as_str().unwrap().to_string()
}

async fn a16_wait_reply(base: &str, u: &TestUser, conv: &str, needle: &str) -> String {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(t) = msgs.iter().rev().find_map(|m| {
            let c = m["content"].as_str().unwrap_or("");
            if m["agent"].is_object() && m["kind"] != "approval" && c.contains(needle) {
                Some(c.to_string())
            } else {
                None
            }
        }) {
            return t;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等回复含「{needle}」超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_msg(
    base: &str,
    u: &TestUser,
    conv: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = msgs.iter().find(|m| pred(m)) {
            return m.clone();
        }
        assert!(std::time::Instant::now() < deadline, "等消息超时：{msgs:?}");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_pending(base: &str, u: &TestUser, conv: &str) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
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
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_succeeded(base: &str, u: &TestUser, conv: &str) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let rows = list_inv(base, u, conv).await;
        if latest_inv_status(&rows) == Some("succeeded") {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "调用未成功：{rows}");
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

fn a16_line<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("输出里没有「{needle}」：{text}"))
}

async fn a16_tool_get(base: &str, auth: &str, path: &str) -> reqwest::Response {
    http()
        .get(format!("{base}{path}"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
}

async fn a16_tool_post(base: &str, auth: &str, path: &str, body: &Value) -> (u16, Value) {
    let r = http()
        .post(format!("{base}{path}"))
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let v = r.json().await.unwrap_or(json!(null));
    (status, v)
}

async fn a16_upload_to(
    base: &str,
    auth: &str,
    conv: Option<&str>,
    name: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut url = format!("{base}/v1/agent-tools/files");
    if let Some(c) = conv {
        url.push_str(&format!("?conversation_id={c}"));
    }
    let part = reqwest::multipart::Part::bytes(body)
        .file_name(name.to_string())
        .mime_str("text/plain")
        .unwrap();
    http()
        .post(url)
        .header("authorization", auth)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

/// 1：列出主人参与的群 / 私聊 / 文件传输助手 / 专属会话，不含主人不在的群。
#[tokio::test]
async fn a16_tool_list_only_owner_conversations() {
    let base = spawn_named_db(&format!("chat_it_a16a_{}", uniq())).await;
    let a = register(&base, &format!("a16a_{}", uniq())).await;
    let b = register(&base, &format!("a16b_{}", uniq())).await;
    let c = register(&base, &format!("a16c_{}", uniq())).await;
    let d = register(&base, &format!("a16d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let tag = uniq();
    let gname = format!("可见群{tag}");
    let sname = format!("秘密群{tag}");
    let group = a16_group(&base, &a, &b, &gname).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, &sname).await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "lister", "read_only").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "g1", "群里打个招呼").await;
    let _ = next_of(&mut sa, "message").await;
    send_ws(&mut sa, &conv, "cmd", "tool:list").await;
    let reply = a16_wait_reply(&base, &a, &conv, &group).await;

    let grow = a16_line(&reply, &group);
    assert!(grow.contains("kind=group"), "{grow}");
    assert!(grow.contains(&format!("title={gname}")), "{grow}");
    assert!(grow.contains("members=2"), "{grow}");
    assert!(grow.contains("last_seq=1"), "{grow}");
    assert!(grow.contains("readonly=false"), "{grow}");

    let dm = a16_line(&reply, "kind=dm");
    assert!(dm.contains(&format!("title={}", b.username)), "{dm}");
    assert!(dm.contains("members=2"), "{dm}");
    assert!(dm.contains("readonly=false"), "{dm}");

    let self_line = a16_line(&reply, "kind=self");
    assert!(self_line.contains("title=文件传输助手"), "{self_line}");
    assert!(self_line.contains("members=1"), "{self_line}");
    assert!(self_line.contains("readonly=false"), "{self_line}");

    let agent_line = a16_line(&reply, "kind=agent");
    assert!(agent_line.contains("title=lister"), "{agent_line}");
    assert!(agent_line.contains("readonly=false"), "{agent_line}");

    assert!(!reply.contains(&sname), "不应列出主人不在的群：{reply}");
    assert!(!reply.contains(&secret), "不应出现外人的群 id：{reply}");
    // 刚说过话的群比没有消息的文件传输助手更靠前
    assert!(
        reply.find(&group).unwrap() < reply.find("文件传输助手").unwrap(),
        "最近活跃的群应排在文件传输助手之前：{reply}"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 2：搜得到主人在群里看得到的消息；搜不到主人不在的群里的消息。
#[tokio::test]
async fn a16_tool_search_respects_membership() {
    let base = spawn_named_db(&format!("chat_it_a16b_{}", uniq())).await;
    let a = register(&base, &format!("a16e_{}", uniq())).await;
    let b = register(&base, &format!("a16f_{}", uniq())).await;
    let c = register(&base, &format!("a16g_{}", uniq())).await;
    let d = register(&base, &format!("a16h_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let tag = uniq();
    let gname = format!("项目群{tag}");
    let sname = format!("密室{tag}");
    let group = a16_group(&base, &a, &b, &gname).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, &sname).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "v1", "甲乙共有词-可见甲").await;
    let _ = next_of(&mut sa, "message").await;
    let (mut sc, _) = tokio_tungstenite::connect_async(ws_url(&base, &c).await)
        .await
        .unwrap();
    send_ws(&mut sc, &secret, "s1", "甲乙共有词-机密乙").await;
    let _ = next_of(&mut sc, "message").await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "finder", "read_only").await;
    send_ws(&mut sa, &conv, "cmd", "tool:search:甲乙共有词").await;
    let reply = a16_wait_reply(&base, &a, &conv, "可见甲").await;
    assert!(reply.contains(&gname), "应带上会话标题：{reply}");
    assert!(reply.contains("可见甲"), "{reply}");
    assert!(!reply.contains("机密乙"), "不应搜到主人不在的群：{reply}");
    assert!(!reply.contains(&sname), "{reply}");
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 3：读得到自己群的历史；读主人不在的会话，工具回错误文本（403）。
#[tokio::test]
async fn a16_tool_read_history_and_forbidden() {
    let base = spawn_named_db(&format!("chat_it_a16c_{}", uniq())).await;
    let a = register(&base, &format!("a16i_{}", uniq())).await;
    let b = register(&base, &format!("a16j_{}", uniq())).await;
    let c = register(&base, &format!("a16k_{}", uniq())).await;
    let d = register(&base, &format!("a16l_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "可读群").await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, "不可读群").await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "h1", "历史甲读得到").await;
    let _ = next_of(&mut sa, "message").await;
    let (mut sc, _) = tokio_tungstenite::connect_async(ws_url(&base, &c).await)
        .await
        .unwrap();
    send_ws(&mut sc, &secret, "h2", "历史乙读不到").await;
    let _ = next_of(&mut sc, "message").await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "reader16", "read_only").await;
    send_ws(&mut sa, &conv, "r1", &format!("tool:read:{group}")).await;
    let ok = a16_wait_reply(&base, &a, &conv, "历史甲读得到").await;
    assert!(ok.contains("[seq "), "应是可读的历史行：{ok}");
    assert!(!ok.contains("历史乙读不到"), "{ok}");

    send_ws(&mut sa, &conv, "r2", &format!("tool:read:{secret}")).await;
    let err = a16_wait_reply(&base, &a, &conv, "tool error").await;
    assert!(err.contains("403"), "{err}");
    assert!(err.contains("not a member"), "{err}");
    assert!(!err.contains("历史乙读不到"), "{err}");
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 4：workspace 直接发。群里出现 Agent 的消息，成员收到并计未读。
#[tokio::test]
async fn a16_workspace_send_reaches_members() {
    let base = spawn_named_db(&format!("chat_it_a16d_{}", uniq())).await;
    let a = register(&base, &format!("a16m_{}", uniq())).await;
    let b = register(&base, &format!("a16n_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "收件群").await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "speaker", "workspace").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "s1",
        &format!("tool:send:{group}:hello-from-agent"),
    )
    .await;

    let frames = until_frame(&mut sb, "message", A01Dur::from_secs(45)).await;
    let hit = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["content"] == "hello-from-agent")
        .unwrap_or_else(|| panic!("成员应收到消息：{frames:?}"));
    assert_eq!(hit["message"]["agent"]["id"], agent_id.as_str());
    assert_eq!(hit["message"]["agent"]["name"], "speaker");
    assert_eq!(a11_unread(&base, &b, &group).await, 1, "群成员未读 +1");

    let own = conv_messages(&base, &a, &conv).await;
    assert!(
        own.iter().all(|m| m["kind"] != "approval"),
        "workspace 不应产生审批卡片：{own:?}"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 5：read_only 先在专属会话出审批卡片。批准后群里出现消息；拒绝则没有消息，回复是拒绝提示。
#[tokio::test]
async fn a16_read_only_send_needs_approval() {
    let base = spawn_named_db(&format!("chat_it_a16e_{}", uniq())).await;
    let a = register(&base, &format!("a16o_{}", uniq())).await;
    let b = register(&base, &format!("a16p_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gname = "审批群";
    let group = a16_group(&base, &a, &b, gname).await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "asker", "read_only").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let allow_text = "批准后的hello";
    send_ws(
        &mut sa,
        &conv,
        "p1",
        &format!("tool:send:{group}:{allow_text}"),
    )
    .await;
    let card = a16_wait_pending(&base, &a, &conv).await;
    assert_eq!(card["agent"]["id"], agent_id.as_str());
    let req = &card["approval"]["request"];
    assert_eq!(req["kind"], "send_message");
    assert!(
        req["locations"].as_array().is_some_and(|a| a.is_empty()),
        "{req}"
    );
    let title = req["title"].as_str().unwrap_or("");
    assert!(title.contains(&format!("发消息到「{gname}」")), "{title}");
    assert!(title.contains(allow_text), "{title}");
    let opts = card["approval"]["options"].as_array().unwrap();
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "allow_once" && o["kind"] == "allow_once"));
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "reject_once" && o["kind"] == "reject_once"));
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(
        latest_inv_status(&rows),
        Some("awaiting_approval"),
        "{rows}"
    );
    assert!(
        conv_messages(&base, &a, &group)
            .await
            .iter()
            .all(|m| m["content"] != allow_text),
        "批准前群里不应有这条消息"
    );

    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id, "allow_once").await;
    assert_eq!(st, 200, "{body}");
    let _ = a16_wait_reply(&base, &a, &conv, "已发送").await;
    let sent = a16_wait_msg(&base, &a, &group, |m| m["content"] == allow_text).await;
    assert_eq!(sent["agent"]["id"], agent_id.as_str());
    assert_eq!(sent["agent"]["name"], "asker");
    a16_wait_succeeded(&base, &a, &conv).await;

    let deny_text = "不该发出去的no";
    send_ws(
        &mut sa,
        &conv,
        "p2",
        &format!("tool:send:{group}:{deny_text}"),
    )
    .await;
    let card = a16_wait_pending(&base, &a, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id, "reject_once").await;
    assert_eq!(st, 200, "{body}");
    let reply = a16_wait_reply(&base, &a, &conv, "主人拒绝了发送").await;
    assert!(reply.contains("主人拒绝了发送"), "{reply}");
    assert!(
        conv_messages(&base, &a, &group)
            .await
            .iter()
            .all(|m| m["content"] != deny_text),
        "拒绝后群里不应有这条消息"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 6：sendfile 在群里留下 Agent 的文件消息，下载内容为写入的字节，计入主人配额。
#[tokio::test]
async fn a16_sendfile_counts_toward_owner_quota() {
    let base = spawn_named_db(&format!("chat_it_a16f_{}", uniq())).await;
    let a = register(&base, &format!("a16q_{}", uniq())).await;
    let b = register(&base, &format!("a16r_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "文件群").await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "filer", "workspace").await;
    let used0: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "f1",
        &format!("tool:sendfile:{group}:a.txt:hi"),
    )
    .await;
    let m = a16_wait_msg(&base, &a, &group, |m| {
        m["kind"] == "file" && m["agent"].is_object()
    })
    .await;
    assert_eq!(m["agent"]["id"], agent_id.as_str());
    assert_eq!(m["attachment"]["file_name"], "a.txt");
    assert_eq!(m["attachment"]["mime"], "text/plain");
    let att = m["attachment"]["id"].as_str().unwrap();
    let data = http()
        .get(format!("{base}/v1/files/{att}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&data[..], b"hi");
    let used1: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        used1["used_bytes"].as_i64().unwrap(),
        used0["used_bytes"].as_i64().unwrap() + 2,
        "文件字节计入主人配额"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 7：只读私聊拒绝；别的调用的附件、以及没指定到目标会话的附件都拒绝；调用结束凭据 401。
#[tokio::test]
async fn a16_send_rejects_readonly_foreign_attachment_and_dead_token() {
    let base = spawn_named_db(&format!("chat_it_a16g_{}", uniq())).await;
    let a = register(&base, &format!("a16s_{}", uniq())).await;
    let b = register(&base, &format!("a16t_{}", uniq())).await;
    let c = register(&base, &format!("a16u_{}", uniq())).await;
    let d = register(&base, &format!("a16v_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "边界群").await;
    let dm = dm_conv(&base, &a).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, "外人群").await;
    let r = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "guard", "workspace").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "d1",
        &format!("tool:send:{dm}:不许发到只读"),
    )
    .await;
    let reply = a16_wait_reply(&base, &a, &conv, "只读").await;
    assert!(reply.contains("tool error"), "{reply}");
    assert!(
        conv_messages(&base, &a, &dm)
            .await
            .iter()
            .all(|m| m["content"] != "不许发到只读"),
        "只读私聊不应落下 Agent 消息"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;

    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "rawbox").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let aconv = agent["conversation_id"].as_str().unwrap();
    send_ws(&mut sa, aconv, "t1", "hold-1").await;
    let f1 = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s1 = f1.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv1 = s1["invocation_id"].as_str().unwrap().to_string();
    let tok1 = s1["tools_token"].as_str().unwrap().to_string();
    let auth1 = format!("Invocation {tok1}");
    send_ws(&mut sa, aconv, "t2", "hold-2").await;
    let f2 = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s2 = f2.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let tok2 = s2["tools_token"].as_str().unwrap().to_string();
    let auth2 = format!("Invocation {tok2}");

    // 主人不是成员：上传和读历史都是 403
    let r = a16_upload_to(&base, &auth1, Some(&secret), "x.txt", b"no".to_vec()).await;
    assert_eq!(r.status(), 403);
    let r = a16_tool_get(
        &base,
        &auth1,
        &format!("/v1/agent-tools/conversations/{secret}/messages"),
    )
    .await;
    assert_eq!(r.status(), 403);
    let err: Value = r.json().await.unwrap();
    assert!(
        err["error"].as_str().unwrap_or("").contains("not a member"),
        "{err}"
    );

    // 空关键词拒绝；没有命中时是空列表
    let r = a16_tool_get(&base, &auth1, "/v1/agent-tools/search?q=").await;
    assert_eq!(r.status(), 400);
    let r = a16_tool_get(
        &base,
        &auth1,
        "/v1/agent-tools/search?q=zzzznotfound&limit=5",
    )
    .await;
    assert_eq!(r.status(), 200);
    let hits: Value = r.json().await.unwrap();
    assert!(hits.as_array().is_some_and(|a| a.is_empty()), "{hits}");

    // 缺省上传进发起会话，不能拿去别的会话发
    let r = a16_upload_to(&base, &auth1, None, "local.txt", b"loc".to_vec()).await;
    assert_eq!(r.status(), 200);
    let att_local = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = a16_tool_post(
        &base,
        &auth1,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "会话不对的附件", "attachment_ids": [att_local]}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(conv_messages(&base, &a, &group)
        .await
        .iter()
        .all(|m| m["content"] != "会话不对的附件"));

    // 指定了目标会话的附件，别的调用不能用
    let r = a16_upload_to(&base, &auth1, Some(&group), "ok.txt", b"ok".to_vec()).await;
    assert_eq!(r.status(), 200);
    let att_ok = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = a16_tool_post(
        &base,
        &auth2,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "借来的附件", "attachment_ids": [att_ok]}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(conv_messages(&base, &a, &group)
        .await
        .iter()
        .all(|m| m["content"] != "借来的附件"));

    let send_body =
        json!({"conversation_id": group, "text": "直发一条", "attachment_ids": [att_ok]});
    let (st, body) = a16_tool_post(&base, &auth1, "/v1/agent-tools/messages", &send_body).await;
    assert_eq!(st, 200, "{body}");
    let message_id = body["message_id"].as_str().unwrap().to_string();
    let sent = a16_wait_msg(&base, &a, &group, |m| m["content"] == "直发一条").await;
    assert_eq!(sent["id"], message_id.as_str());
    assert_eq!(sent["agent"]["id"], agent_id);
    assert_eq!(sent["attachment"]["id"], att_ok.as_str());

    host_finish(&mut sh, &inv1).await;
    let mut dead = false;
    for _ in 0..50 {
        if a16_tool_get(&base, &auth1, "/v1/agent-tools/conversations")
            .await
            .status()
            == 401
        {
            dead = true;
            break;
        }
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    assert!(dead, "调用结束后列会话应 401");
    assert_eq!(
        a16_tool_get(&base, &auth1, "/v1/agent-tools/search?q=直发一条")
            .await
            .status(),
        401
    );
    assert_eq!(
        a16_tool_get(
            &base,
            &auth1,
            &format!("/v1/agent-tools/conversations/{group}/messages")
        )
        .await
        .status(),
        401
    );
    let (st, _) = a16_tool_post(
        &base,
        &auth1,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "太晚了"}),
    )
    .await;
    assert_eq!(st, 401);
}

// ===== Agent A15 =====
// 群里 @a → a 调 b。群成员的历史接口能看到子调用；成功帧只带前 2000 字预览。

fn a15_preview_empty(inv: &Value) -> bool {
    inv.get("result_preview")
        .map(|v| v.is_null())
        .unwrap_or(true)
}

/// 读到这一轮里子调用和它的父调用都 succeeded。返回期间收到的 agent_invocation 帧。
async fn a15_until_pair<S>(sock: &mut S, timeout: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等子调用终态超时: {seen:?}");
        let frames = until_frame(sock, "agent_invocation", left).await;
        for f in frames {
            if f["type"] == "agent_invocation" {
                seen.push(f);
            }
        }
        let parents: Vec<String> = seen
            .iter()
            .filter_map(|f| {
                let i = &f["invocation"];
                if i["agent_name"] == "b" && i["status"] == "succeeded" {
                    i["parent_invocation_id"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        let parent_done = seen.iter().any(|f| {
            let i = &f["invocation"];
            i["status"] == "succeeded" && parents.iter().any(|id| i["id"] == id.as_str())
        });
        if !parents.is_empty() && parent_done {
            return seen;
        }
    }
}

/// 群成员拉得到子调用；子调用成功帧的预览是正文，父调用和进行中帧的预览为空。
/// 超长回复（含一个 emoji）在帧和历史里都只留 2000 个字。
#[tokio::test]
async fn a15_group_child_preview() {
    let env = a14_pair("nest").await;
    let friend = register(&env.base, &format!("a15f_{}", uniq())).await;
    befriend(&env.base, &env.user, &friend).await;
    let stranger = register(&env.base, &format!("a15s_{}", uniq())).await;
    let group: Value = http()
        .post(format!("{}/v1/conversations/group", env.base))
        .header("authorization", authz(&env.user.access))
        .json(&json!({"name": "a15", "member_ids": [friend.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = group["id"].as_str().unwrap().to_string();

    // 成员校验不变：群外的人、以及不是被调方专属会话成员的人，都进不去那条会话的列表。
    let denied = http()
        .get(format!(
            "{}/v1/conversations/{gid}/agent-invocations",
            env.base
        ))
        .header("authorization", authz(&stranger.access))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let denied_b = http()
        .get(format!(
            "{}/v1/conversations/{}/agent-invocations",
            env.base, env.b_conv
        ))
        .header("authorization", authz(&friend.access))
        .send()
        .await
        .unwrap();
    assert_eq!(denied_b.status(), 403);

    let (mut sf, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &friend).await)
        .await
        .unwrap();
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "a15g1",
            "content": "call:b:echo:hi",
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let seen = a15_until_pair(&mut sf, A01Dur::from_secs(45)).await;
    for f in &seen {
        let inv = &f["invocation"];
        let child_ok = inv["parent_invocation_id"].is_string() && inv["status"] == "succeeded";
        if !child_ok {
            assert!(
                a15_preview_empty(inv),
                "非成功子调用的 result_preview 应为空: {inv}"
            );
        }
        assert!(
            inv.get("result_text").is_none(),
            "帧里不应出现完整 result_text: {inv}"
        );
    }
    let child = seen
        .iter()
        .find(|f| {
            let i = &f["invocation"];
            i["agent_name"] == "b" && i["status"] == "succeeded"
        })
        .expect("子调用成功帧")["invocation"]
        .clone();
    assert_eq!(child["result_preview"], "hi");
    let parent_id = child["parent_invocation_id"].as_str().unwrap();
    let parent = seen
        .iter()
        .find(|f| f["invocation"]["id"] == parent_id && f["invocation"]["status"] == "succeeded")
        .expect("父调用成功帧");
    assert!(
        a15_preview_empty(&parent["invocation"]),
        "非子调用的 result_preview 应为空: {}",
        parent["invocation"]
    );

    let rows = list_inv(&env.base, &friend, &gid).await;
    let listed = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == child["id"])
        .expect("群历史应包含子调用");
    assert_eq!(listed["parent_invocation_id"], parent_id);
    assert_eq!(listed["root_conversation_id"], gid.as_str());
    assert_eq!(listed["result_preview"], "hi");
    assert_eq!(listed["agent_name"], "b");
    let listed_parent = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == parent_id)
        .expect("群历史应包含父调用");
    assert!(a15_preview_empty(listed_parent), "{listed_parent}");

    // 边界：2001 个字（中间有一个 emoji）只留前 2000 个，不按字节截断。
    let mut long = "你".repeat(1998);
    long.push('😀');
    long.push('Y');
    long.push('Z');
    assert_eq!(long.chars().count(), 2001);
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "a15g2",
            "content": format!("call:b:echo:{long}"),
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let seen_long = a15_until_pair(&mut sf, A01Dur::from_secs(45)).await;
    let long_child = seen_long
        .iter()
        .find(|f| {
            let i = &f["invocation"];
            i["agent_name"] == "b"
                && i["status"] == "succeeded"
                && i["result_preview"].as_str().unwrap_or("").chars().count() > 2
        })
        .expect("超长子调用成功帧")["invocation"]
        .clone();
    let preview = long_child["result_preview"].as_str().unwrap();
    assert_eq!(preview.chars().count(), 2000, "预览应按字截断");
    assert!(
        preview.ends_with('Y'),
        "第 2000 字应是 Y，而不是把 emoji 拆开"
    );
    assert!(!preview.ends_with('Z'), "第 2001 字不应进帧");
    assert!(preview.contains('😀'));
    let long_parent_id = long_child["parent_invocation_id"].as_str().unwrap();
    let long_parent = seen_long
        .iter()
        .find(|f| {
            f["invocation"]["id"] == long_parent_id && f["invocation"]["status"] == "succeeded"
        })
        .expect("超长调用的父调用帧");
    assert!(a15_preview_empty(&long_parent["invocation"]));

    let rows = list_inv(&env.base, &friend, &gid).await;
    let listed_long = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == long_child["id"])
        .expect("刷新后仍能看到超长子调用");
    assert_eq!(listed_long["result_preview"].as_str(), Some(preview));
    assert_eq!(listed_long["parent_invocation_id"], long_parent_id);

    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

// ===== Agent 验收补丁：私聊 / 群里也能开新话题（spec Q6） =====

#[tokio::test]
async fn agent_reset_context_in_group_only_touches_own_agents() {
    let base = spawn_named_db(&format!("chat_it_rstg_{}", uniq())).await;
    let owner = register(&base, &format!("rsto_{}", uniq())).await;
    let member = register(&base, &format!("rstm_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &owner, &host_id, "bot").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &owner, &[&member]).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    let reset = |u: &TestUser, conv: &str| {
        http()
            .post(format!(
                "{base}/v1/conversations/{conv}/agent-context/reset"
            ))
            .header("authorization", authz(&u.access))
            .send()
    };

    send_ws(&mut so, &gid, "r1", "@bot echo:one").await;
    wait_agent_reply(&base, &owner, &gid, &["one"]).await;

    // 群里别的成员重置：只清他自己的 Agent（他没有），主人的上下文还在
    assert_eq!(reset(&member, &gid).await.unwrap().status().as_u16(), 204);
    send_ws(&mut so, &gid, "r2", "@bot recall").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["one", "echo:one"]).await;
    assert!(texts.iter().any(|t| t.contains("echo:one")), "{texts:?}");

    // 主人重置后，bot 在这个群里从头开始
    assert_eq!(reset(&owner, &gid).await.unwrap().status().as_u16(), 204);
    send_ws(&mut so, &gid, "r3", "@bot recall").await;
    let texts = wait_agent_reply(&base, &owner, &gid, &["one", "echo:one", "(empty)"]).await;
    assert!(texts.contains(&"(empty)".to_string()), "{texts:?}");

    // 文件传输助手不支持；非成员被拒
    let selfc = self_conv(&base, &owner).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reset(&owner, &selfc).await.unwrap().status().as_u16(), 400);
    let outsider = register(&base, &format!("rstx_{}", uniq())).await;
    assert_eq!(reset(&outsider, &gid).await.unwrap().status().as_u16(), 403);
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

async fn a22_send_approval(sh: &mut RawHostWs, inv_id: &str, request_id: &str, title: &str) {
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

async fn a22_inv_status(base: &str, u: &TestUser, conv: &str) -> String {
    latest_inv_status(&list_inv(base, u, conv).await)
        .unwrap_or("")
        .to_string()
}

async fn a22_wait_approval(base: &str, u: &TestUser, conv: &str, request_id: &str) -> Value {
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

async fn a22_wait_decision(sh: &mut RawHostWs, request_id: &str) -> Value {
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

/// 宿主断开：awaiting_approval 的调用失败，全部 pending 一起 expired。
#[tokio::test]
async fn a22_host_disconnect_expires_all_pending_approvals() {
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

    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    loop {
        let rows = list_inv(&base, &a, &conv).await;
        let row = &rows.as_array().unwrap()[0];
        if row["status"] == "failed" && row["error"] == "host_disconnected" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "断线后调用应失败: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(50)).await;
    }
    let id1 = card1["approval"]["id"].as_str().unwrap();
    let id2 = card2["approval"]["id"].as_str().unwrap();
    wait_approval_status(&base, &a, &conv, id1, "expired").await;
    wait_approval_status(&base, &a, &conv, id2, "expired").await;
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

// ===== Agent A21 =====
// 进行中调用的流式接收者随成员变更重算；开新话题之后，重置前派发的调用不得把旧会话写回。

type A21Sock =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn a21_open_host(base: &str, u: &TestUser) -> (String, A21Sock) {
    let url = format!("{}&role=host", ws_url(base, u).await);
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(WsMsg::Text(
        json!({"type":"host_hello","install_id":uuid::Uuid::new_v4(),"label":"a21","platform":"test"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let frames = until_frame(&mut sock, "host_ready", A01Dur::from_secs(5)).await;
    let id = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "host_ready")
        .unwrap()["host_id"]
        .as_str()
        .unwrap()
        .to_string();
    (id, sock)
}

async fn a21_open_user(base: &str, u: &TestUser) -> A21Sock {
    tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap()
        .0
}

async fn a21_next_frame<S>(sock: &mut S, pred: impl Fn(&Value) -> bool) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等宿主/成员帧超时: {seen:?}");
        match tokio::time::timeout(left, sock.next()).await {
            Ok(Some(Ok(WsMsg::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let hit = pred(&v);
                    seen.push(v.clone());
                    if hit {
                        return v;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("读帧异常: {other:?}; 已收到: {seen:?}"),
        }
    }
}

async fn a21_wait_delta<S>(sock: &mut S, inv_id: &str, text: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    a21_next_frame(sock, |v| {
        v["type"] == "agent_stream"
            && v["invocation_id"] == inv_id
            && v["delta"].as_str().unwrap_or("").contains(text)
    })
    .await
}

async fn a21_no_delta<S>(sock: &mut S, inv_id: &str, text: &str)
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let frames = frames_for(sock, A01Dur::from_millis(500)).await;
    assert!(
        !frames.iter().any(|f| {
            f["type"] == "agent_stream"
                && f["invocation_id"] == inv_id
                && f["delta"].as_str().unwrap_or("").contains(text)
        }),
        "{inv_id} 不应再收到 {text}: {frames:?}"
    );
}

async fn a21_push_text<S>(host: &mut S, inv_id: &str, text: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    host.send(WsMsg::Text(
        json!({
            "type": "invocation_event",
            "invocation_id": inv_id,
            "event": {"type": "text", "text": text}
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

async fn a21_finish<S>(host: &mut S, inv_id: &str, session_id: &str, text: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    host.send(WsMsg::Text(
        json!({
            "type": "invocation_finish",
            "invocation_id": inv_id,
            "ok": true,
            "text": text,
            "session_id": session_id
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

async fn a21_wait_status(base: &str, u: &TestUser, conv: &str, inv_id: &str, status: &str) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(8);
    loop {
        let rows: Value = http()
            .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == inv_id && r["status"] == status)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等 {inv_id} 变为 {status} 超时: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(40)).await;
    }
}

fn a21_session(start: &Value) -> Value {
    start["context"]["session_id"].clone()
}

async fn a21_say_start(user: &mut A21Sock, host: &mut A21Sock, conv: &str, content: &str) -> Value {
    send_ws(user, conv, &format!("a21{}", uniq()), content).await;
    a21_next_frame(host, |v| v["type"] == "invocation_start").await
}

/// 群里 @ 自己的 Agent，假宿主推流。踢人后 B 不再收到，A 还收得到；解散后 A 也不再收到。
#[tokio::test]
async fn agent_a21_kick_and_dissolve_stop_stream() {
    let base = spawn_named_db(&format!("chat_it_a21k_{}", uniq())).await;
    let a = register(&base, &format!("a21ka_{}", uniq())).await;
    let b = register(&base, &format!("a21kb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "ka").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start = a21_say_start(&mut sa, &mut host, &gid, "@ka hi").await;
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    a21_push_text(&mut host, &inv_id, "A21BEFORE").await;
    let before_a = a21_wait_delta(&mut sa, &inv_id, "A21BEFORE").await;
    assert_eq!(before_a["conversation_id"], gid.as_str());
    a21_wait_delta(&mut sb, &inv_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    a21_push_text(&mut host, &inv_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &inv_id, "A21AFTER").await;
    a21_no_delta(&mut sb, &inv_id, "A21AFTER").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21THIRD").await;
    a21_no_delta(&mut sa, &inv_id, "A21THIRD").await;
}

/// B 退群后收不到后续增量；最后一人退出把会话删掉后，主人也不再收到。
#[tokio::test]
async fn agent_a21_leave_and_last_member_stop_stream() {
    let base = spawn_named_db(&format!("chat_it_a21l_{}", uniq())).await;
    let a = register(&base, &format!("a21la_{}", uniq())).await;
    let b = register(&base, &format!("a21lb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "la").await;
    assert_eq!(st, 200, "{agent}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start = a21_say_start(&mut sa, &mut host, &gid, "@la hi").await;
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    a21_push_text(&mut host, &inv_id, "A21BEFORE").await;
    a21_wait_delta(&mut sa, &inv_id, "A21BEFORE").await;
    a21_wait_delta(&mut sb, &inv_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &inv_id, "A21AFTER").await;
    a21_no_delta(&mut sb, &inv_id, "A21AFTER").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &inv_id, "A21LAST").await;
    a21_no_delta(&mut sa, &inv_id, "A21LAST").await;
}

/// 子调用的接收者是「专属会话成员 ∪ 根会话成员」。
/// 踢掉只在群里的人后他收不到；主人退出群后仍在专属会话里，流不能断。
#[tokio::test]
async fn agent_a21_root_member_kept_when_still_in_other_conversation() {
    let base = spawn_named_db(&format!("chat_it_a21u_{}", uniq())).await;
    let a = register(&base, &format!("a21ua_{}", uniq())).await;
    let b = register(&base, &format!("a21ub_{}", uniq())).await;
    let c = register(&base, &format!("a21uc_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, parent) = create_agent(&base, &a, &host_id, "pa").await;
    assert_eq!(st, 200, "{parent}");
    let (st, child) = create_agent(&base, &a, &host_id, "ch").await;
    assert_eq!(st, 200, "{child}");
    let child_conv = child["conversation_id"].as_str().unwrap().to_string();
    let gid = a11_group(&base, &a, &[&b, &c]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;
    let mut sc = a21_open_user(&base, &c).await;

    let parent_start = a21_say_start(&mut sa, &mut host, &gid, "@pa go").await;
    let token = parent_start["tools_token"]
        .as_str()
        .expect("父调用应带 tools_token")
        .to_string();
    let resp = http()
        .post(format!("{base}/v1/agent-tools/invocations"))
        .header("authorization", format!("Invocation {token}"))
        .json(&json!({"agent_name": "ch", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let child_id = body["invocation_id"].as_str().unwrap().to_string();
    let child_start = a21_next_frame(&mut host, |v| v["type"] == "invocation_start").await;
    assert_eq!(child_start["invocation_id"], child_id.as_str());

    a21_push_text(&mut host, &child_id, "A21BEFORE").await;
    let frame = a21_wait_delta(&mut sa, &child_id, "A21BEFORE").await;
    assert_eq!(frame["conversation_id"], child_conv.as_str());
    a21_wait_delta(&mut sb, &child_id, "A21BEFORE").await;
    a21_wait_delta(&mut sc, &child_id, "A21BEFORE").await;

    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", c.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &child_id, "A21MID").await;
    a21_wait_delta(&mut sa, &child_id, "A21MID").await;
    a21_wait_delta(&mut sb, &child_id, "A21MID").await;
    a21_no_delta(&mut sc, &child_id, "A21MID").await;

    // 主人退出群：专属会话里还有他，根会话里还有 B。两边都不能被拿掉。
    let r = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    a21_push_text(&mut host, &child_id, "A21AFTER").await;
    a21_wait_delta(&mut sa, &child_id, "A21AFTER").await;
    a21_wait_delta(&mut sb, &child_id, "A21AFTER").await;
    a21_no_delta(&mut sc, &child_id, "A21AFTER").await;
}

/// 专属会话：运行中开新话题后，旧调用的 session 不写回；之后的调用后完成者覆盖，下一次能续上。
#[tokio::test]
async fn agent_a21_reset_dedicated_blocks_inflight_session_writeback() {
    let base = spawn_named_db(&format!("chat_it_a21d_{}", uniq())).await;
    let a = register(&base, &format!("a21da_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "solo").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let mut sa = a21_open_user(&base, &a).await;

    let start1 = a21_say_start(&mut sa, &mut host, &conv, "one").await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start1["context"]["key"], conv.as_str());
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    a21_finish(&mut host, &id1, "old-session", "old").await;
    a21_wait_status(&base, &a, &conv, &id1, "succeeded").await;

    let start2 = a21_say_start(&mut sa, &mut host, &conv, "two").await;
    let id2 = start2["invocation_id"].as_str().unwrap().to_string();
    assert!(
        a21_session(&start2).is_null(),
        "重置前的调用写回应被丢掉: {}",
        start2["context"]
    );
    let start3 = a21_say_start(&mut sa, &mut host, &conv, "three").await;
    let id3 = start3["invocation_id"].as_str().unwrap().to_string();
    assert!(a21_session(&start3).is_null(), "{}", start3["context"]);

    a21_finish(&mut host, &id2, "s2", "two").await;
    a21_wait_status(&base, &a, &conv, &id2, "succeeded").await;
    a21_finish(&mut host, &id3, "s3", "three").await;
    a21_wait_status(&base, &a, &conv, &id3, "succeeded").await;

    let start4 = a21_say_start(&mut sa, &mut host, &conv, "four").await;
    assert_eq!(
        a21_session(&start4),
        json!("s3"),
        "后完成的调用应覆盖上下文: {}",
        start4["context"]
    );
}

/// 群里只清请求者自己的 Agent。别人的进行中调用仍写回；自己的旧调用不写回，新调用可以续上。
#[tokio::test]
async fn agent_a21_reset_group_only_blocks_own_agent() {
    let base = spawn_named_db(&format!("chat_it_a21g_{}", uniq())).await;
    let a = register(&base, &format!("a21ga_{}", uniq())).await;
    let b = register(&base, &format!("a21gb_{}", uniq())).await;
    let (host_a_id, mut host_a) = a21_open_host(&base, &a).await;
    let (host_b_id, mut host_b) = a21_open_host(&base, &b).await;
    let (st, agent_a) = create_agent(&base, &a, &host_a_id, "ga").await;
    assert_eq!(st, 200, "{agent_a}");
    let (st, agent_b) = create_agent(&base, &b, &host_b_id, "gb").await;
    assert_eq!(st, 200, "{agent_b}");
    let gid = a11_group(&base, &a, &[&b]).await;
    let mut sa = a21_open_user(&base, &a).await;
    let mut sb = a21_open_user(&base, &b).await;

    let start_a = a21_say_start(&mut sa, &mut host_a, &gid, "@ga hi").await;
    let id_a = start_a["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start_a["context"]["key"], gid.as_str());
    let start_b = a21_say_start(&mut sb, &mut host_b, &gid, "@gb hi").await;
    let id_b = start_b["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start_b["context"]["key"], gid.as_str());

    assert_eq!(reset_context(&base, &a, &gid).await, 204);
    a21_finish(&mut host_a, &id_a, "a-old", "from-a").await;
    a21_finish(&mut host_b, &id_b, "b-keep", "from-b").await;
    a21_wait_status(&base, &a, &gid, &id_a, "succeeded").await;
    a21_wait_status(&base, &b, &gid, &id_b, "succeeded").await;

    let again_a = a21_say_start(&mut sa, &mut host_a, &gid, "@ga again").await;
    assert!(
        a21_session(&again_a).is_null(),
        "自己的旧调用不应写回: {}",
        again_a["context"]
    );
    let again_b = a21_say_start(&mut sb, &mut host_b, &gid, "@gb again").await;
    assert_eq!(
        a21_session(&again_b),
        json!("b-keep"),
        "别人的 Agent 不受这次重置影响: {}",
        again_b["context"]
    );

    let id_new = again_a["invocation_id"].as_str().unwrap().to_string();
    a21_finish(&mut host_a, &id_new, "a-new", "fresh").await;
    a21_wait_status(&base, &a, &gid, &id_new, "succeeded").await;
    let third = a21_say_start(&mut sa, &mut host_a, &gid, "@ga third").await;
    assert_eq!(a21_session(&third), json!("a-new"), "{}", third["context"]);
}

/// 私聊走和群同一条重置路径：只挡请求者自己的 Agent，重置后的新调用仍能写回。
#[tokio::test]
async fn agent_a21_reset_dm_blocks_inflight_session_writeback() {
    let base = spawn_named_db(&format!("chat_it_a21m_{}", uniq())).await;
    let a = register(&base, &format!("a21ma_{}", uniq())).await;
    let b = register(&base, &format!("a21mb_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "dmbot").await;
    assert_eq!(st, 200, "{agent}");
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let mut sa = a21_open_user(&base, &a).await;

    let start1 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot hi").await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert_eq!(start1["context"]["key"], dm.as_str());
    assert_eq!(reset_context(&base, &a, &dm).await, 204);
    a21_finish(&mut host, &id1, "dm-old", "old").await;
    a21_wait_status(&base, &a, &dm, &id1, "succeeded").await;

    let start2 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot next").await;
    assert!(
        a21_session(&start2).is_null(),
        "私聊重置前的调用不应写回: {}",
        start2["context"]
    );
    let id2 = start2["invocation_id"].as_str().unwrap().to_string();
    a21_finish(&mut host, &id2, "dm-new", "new").await;
    a21_wait_status(&base, &a, &dm, &id2, "succeeded").await;
    let start3 = a21_say_start(&mut sa, &mut host, &dm, "@dmbot cont").await;
    assert_eq!(
        a21_session(&start3),
        json!("dm-new"),
        "{}",
        start3["context"]
    );
}

/// 定时调用的 schedule:<id> 不跟会话重置走：运行中重置专属会话，完成后仍写回，下一次还能续上。
#[tokio::test]
async fn agent_a21_schedule_context_survives_reset() {
    let (base, st) = spawn_named_state(&format!("chat_it_a21s_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a21sa_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "sched").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (code, sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨", "ping", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{sched}");
    let sched_id = sched["id"].as_str().unwrap().to_string();
    let key = format!("schedule:{sched_id}");

    a17_due(&st.pool, &sched_id).await;
    let start1 = a21_next_frame(&mut host, |v| {
        v["type"] == "invocation_start" && v["context"]["key"] == key.as_str()
    })
    .await;
    let id1 = start1["invocation_id"].as_str().unwrap().to_string();
    assert!(a21_session(&start1).is_null(), "{}", start1["context"]);
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    a21_finish(&mut host, &id1, "sched-sess", "ping").await;
    a21_wait_status(&base, &a, &conv, &id1, "succeeded").await;

    let saved: Option<String> = sqlx::query_scalar(
        "SELECT runtime_session_id FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&key)
    .fetch_optional(&st.pool)
    .await
    .unwrap();
    assert_eq!(saved.as_deref(), Some("sched-sess"));
    assert_eq!(reset_context(&base, &a, &conv).await, 204);
    let still: Option<String> = sqlx::query_scalar(
        "SELECT runtime_session_id FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&key)
    .fetch_optional(&st.pool)
    .await
    .unwrap();
    assert_eq!(still.as_deref(), Some("sched-sess"));

    a17_due(&st.pool, &sched_id).await;
    let start2 = a21_next_frame(&mut host, |v| {
        v["type"] == "invocation_start"
            && v["context"]["key"] == key.as_str()
            && v["invocation_id"] != id1.as_str()
    })
    .await;
    assert_eq!(
        a21_session(&start2),
        json!("sched-sess"),
        "定时上下文不应被开新话题清掉: {}",
        start2["context"]
    );
}

/// 重启语义：调用行上留着派发时的旧代次，内存里的运行时快照已经没了，finish 不写回。
#[tokio::test]
async fn agent_a21_stale_epoch_without_run_does_not_restore_session() {
    let (base, st) = spawn_named_state(&format!("chat_it_a21r_{}", uniq())).await;
    let a = register(&base, &format!("a21ra_{}", uniq())).await;
    let (host_id, mut host) = a21_open_host(&base, &a).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "solo").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let mut sa = a21_open_user(&base, &a).await;

    let start = a21_say_start(&mut sa, &mut host, &conv, "one").await;
    let id = start["invocation_id"].as_str().unwrap().to_string();
    let inv_id = uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(reset_context(&base, &a, &conv).await, 204);

    // 重置后上下文代次已是 1、会话为空。调用行改回派发时的旧代次，并摘掉内存快照。
    let epoch: i64 = sqlx::query_scalar(
        "SELECT epoch FROM agent_contexts WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&conv)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(epoch, 1);
    sqlx::query("UPDATE agent_invocations SET context_epoch = 0 WHERE id = $1")
        .bind(inv_id)
        .execute(&st.pool)
        .await
        .unwrap();
    assert!(
        st.inv_runs.remove(&inv_id).is_some(),
        "派发后应有运行时快照可摘"
    );
    assert!(st.inv_runs.get(&inv_id).is_none());

    a21_finish(&mut host, &id, "revived", "nope").await;
    a21_wait_status(&base, &a, &conv, &id, "succeeded").await;
    let saved: (String, i64) = sqlx::query_as(
        "SELECT runtime_session_id, epoch FROM agent_contexts
          WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(uuid::Uuid::parse_str(&aid).unwrap())
    .bind(&conv)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(saved.0, "", "旧代次的 finish 不应把会话写回来");
    assert_eq!(saved.1, 1);

    let next = a21_say_start(&mut sa, &mut host, &conv, "two").await;
    assert!(
        a21_session(&next).is_null(),
        "下一次派发仍不应带上被拒绝的会话: {}",
        next["context"]
    );
}
