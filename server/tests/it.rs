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
        files_dir: std::env::temp_dir().join("chat-it-files").to_string_lossy().into(),
        upload_max_bytes: 25 * 1024 * 1024,
    };
    std::fs::create_dir_all(&cfg.files_dir).unwrap();
    let state = AppState::new(pool, cfg.clone());
    let app = build_router(state, &cfg);
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
    let text = resp.text().await.unwrap();
    eprintln!("register {name} -> {status} body={text:?}");
    let r: Value = serde_json::from_str(&text).unwrap();
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
    let app = build_router(state, &cfg);
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

    // 启用 → 恢复
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
