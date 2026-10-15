//! MM5：测试库生命周期助手，`tests/it`（经 `#[path]` 引入）与 `tests/migrations.rs`
//! 两个测试二进制共用。
//!
//! 设计：
//! - 每个测试二进制**一次运行**用一个共享库：`TEST_DATABASE_NAME` 指定则用之
//!   （视为用户所有，永不删），否则生成 `chat_it_run_<pid>_<8位hex>`，
//!   进程退出时经 `libc::atexit` 钩子删除（选 atexit 而非 ctor crate：
//!   正常退出路径（`exit()`/`main` 返回）都触发，且 libc 本就在依赖树里，
//!   不需要新 macro 依赖）。退出钩子里**不能**在已有 tokio runtime 上
//!   `block_on`——统一走「新 std::thread + 全新 current-thread runtime，
//!   join 并设超时、吞掉全部错误」的做法，atexit 与 Drop 都绝不 panic。
//! - 每个测试的独立库走 `TempDb` 守卫：守卫存活期间在 admin 连接上持有
//!   `pg_advisory_lock(hashtext(dbname))`（会话级锁，连接断开即释放），
//!   Drop（含 panic 展开路径）用上述阻塞方式删库。
//! - 启动清扫器（每进程一次）：列出 `chat_it_run_`/`chat_t_`/`chat_it_`
//!   前缀的库，对 `pg_try_advisory_lock` 能拿到的（没有活着的属主）执行
//!   DROP——覆盖 SIGKILL/Ctrl-C/abort 留下的残留。绝不触碰 `chat_test`
//!   与前缀之外的任何库。
//! - 顺序一律「先持锁、后建库」：任何已存在的受控前缀库都必然有持锁属主，
//!   清扫器不会误删活着的库。
//! - CREATE/DROP 都跑在普通非事务连接上；DROP 用 `WITH (FORCE)`（PG≥13），
//!   失败回退 `pg_terminate_backend` + `DROP`（PG<13）。CREATE 用
//!   `TEMPLATE template0`（避免 template1 被占用时报错），至多 5 次退避重试。
//! - nextest 下每个测试一个进程：共享库退化为每进程一个（更慢但仍正确），
//!   属未验证的 best-effort。

// 本文件同时编进 it 与 migrations 两个测试二进制：各自只用一部分，
// 互不为对方用的函数对另一个二进制是死代码，属预期。
#![allow(dead_code)]

use sqlx::postgres::PgConnection;
use sqlx::Connection as _;
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

/// 允许纳入自动清理的库名前缀。`chat_test` 刻意不在内：它是历史共享库，
/// 只有用户显式 `TEST_DATABASE_NAME=chat_test` 时才使用，且永不删。
const TEST_PREFIXES: &[&str] = &["chat_it_run_", "chat_t_", "chat_it_"];

/// admin 连接用的库地址（DATABASE_URL；其上 CREATE/DROP DATABASE 与 advisory lock）。
pub fn admin_db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:15432/postgres".into())
}

/// 连接 admin 库（不是测试库），失败不 panic 的版本——供 Drop/atexit 清理用。
pub async fn try_connect_admin() -> Option<PgConnection> {
    PgConnection::connect(&admin_db_url()).await.ok()
}

/// 连接 admin 库（不是测试库）。
pub async fn connect_admin() -> PgConnection {
    try_connect_admin()
        .await
        .expect("connect admin db (docker compose up db)")
}

/// 合法标识符 `^[a-z_][a-z0-9_]{0,62}$`（PG 上限 63 字节）。
fn valid_db_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 把 DATABASE_URL 的库名段替换为 `db_name`（剥掉 query/fragment；无路径则追加）。
pub fn db_url_for(db_name: &str) -> String {
    let base = admin_db_url();
    let base = base.split(['?', '#']).next().unwrap_or(&base);
    // 库名从 authority（`scheme://`）之后的第一个 '/' 开始，找不到就在末尾补路径
    let path_start = base
        .find("://")
        .and_then(|i| base[i + 3..].find('/').map(|j| i + 3 + j));
    match path_start {
        Some(i) => format!("{}/{db_name}", &base[..i]),
        None => format!("{base}/{db_name}"),
    }
}

/// 库是否存在（admin 视角）。
pub async fn db_exists(name: &str) -> bool {
    let mut conn = connect_admin().await;
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(name)
    .fetch_one(&mut conn)
    .await
    .unwrap_or(false)
}

/// CREATE DATABASE（幂等）：已存在跳过；并发撞名（42P04）视为成功；
/// 其余错误退避重试至多 5 次。TEMPLATE template0 避开 template1 被占用。
async fn create_db(conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if exists {
        return Ok(());
    }
    let mut backoff = Duration::from_millis(50);
    let mut last_err = None;
    for _ in 0..5 {
        match sqlx::query(&format!("CREATE DATABASE {} TEMPLATE template0", qi(name)))
            .execute(&mut *conn)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                let code = e
                    .as_database_error()
                    .and_then(|d| d.code().map(|c| c.into_owned()));
                if code.as_deref() == Some("42P04") {
                    return Ok(());
                }
                last_err = Some(e);
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    Err(last_err.unwrap())
}

/// DROP DATABASE：`WITH (FORCE)`（PG≥13，连带踢掉还连着的后端）；
/// 失败回退 `pg_terminate_backend` + 普通 DROP（PG<13）。
async fn drop_db(conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    match sqlx::query(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", qi(name)))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => Ok(()),
        Err(first) => {
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                  WHERE datname = $1 AND pid <> pg_backend_pid()",
            )
            .bind(name)
            .execute(&mut *conn)
            .await;
            sqlx::query(&format!("DROP DATABASE IF EXISTS {}", qi(name)))
                .execute(&mut *conn)
                .await
                .map(|_| ())
                .map_err(|_| first)
        }
    }
}

/// 在 `conn` 上获取 `hashtext(name)` 的会话级 advisory lock（断线即释放）。
/// 用 try+重试而不是裸 `pg_advisory_lock`：`TEST_DATABASE_NAME` 与另一存活
/// 进程撞名时限等待、报清楚，而不是无限期挂起。
async fn lock_db(conn: &mut PgConnection, name: &str) {
    for _ in 0..120 {
        let got: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint)",
        )
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .unwrap_or(false);
        if got {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("test db {name} advisory lock held by another live run for 60s");
}

/// 清扫器主体：删掉受控前缀且没有活着属主（advisory lock 拿不到）的库。
/// 任何一步失败都只影响那一个库，绝不中断清理、绝不 panic。
pub async fn sweep_stale_test_dbs(conn: &mut PgConnection) {
    let stale: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database
          WHERE datname ~ '^(chat_it_run_|chat_t_|chat_it_)' ORDER BY 1",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap_or_default();
    for name in stale {
        let got: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint)",
        )
        .bind(&name)
        .fetch_one(&mut *conn)
        .await
        .unwrap_or(false);
        if !got {
            continue; // 有活着的属主
        }
        let _ = drop_db(&mut *conn, &name).await;
        let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext($1)::bigint)")
            .bind(&name)
            .execute(&mut *conn)
            .await;
    }
}

/// 每进程一次的启动清扫。
static SWEEP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_swept(conn: &mut PgConnection) {
    SWEEP.get_or_init(|| sweep_stale_test_dbs(conn)).await;
}

/// 删库阻塞执行器：全新线程 + 全新 current-thread runtime（绝不在调用方的
/// tokio runtime 里 block_on），join 并设超时兜底，所有错误吞掉。
/// 注意：DROP 必须用本 runtime 里新开的连接——`lock` 那条连接的 socket 注册在
/// 创建它的 runtime 的 reactor 上，跨 runtime poll 会永久卡住。`lock` 只在
/// 删库后 drop（释放 advisory lock，顺序上防止清扫器双删）。
fn blocking_drop(name: String, lock: Option<PgConnection>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("testdb-drop".into())
        .spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async move {
                    if let Some(mut c) = try_connect_admin().await {
                        let _ = drop_db(&mut c, &name).await;
                    }
                    drop(lock);
                });
            }
            let _ = tx.send(());
        });
    if spawned.is_err() {
        return;
    }
    let _ = rx.recv_timeout(Duration::from_secs(60));
}

/// per-test 独立库守卫。存活期持 advisory lock；Drop（含 panic 展开）
/// 阻塞删库，绝不 panic。
pub struct TempDb {
    name: String,
    lock: Option<PgConnection>,
}

impl TempDb {
    /// 清扫（每进程一次）→ 持锁 → CREATE DATABASE TEMPLATE template0。
    /// `db_name` 必须带受控前缀，保证守卫与清扫器对它有所有权。
    pub async fn create(db_name: &str) -> TempDb {
        assert!(
            TEST_PREFIXES.iter().any(|p| db_name.starts_with(p)),
            "test db name must start with one of {TEST_PREFIXES:?}: {db_name}"
        );
        assert!(valid_db_name(db_name), "invalid test db name: {db_name}");
        let mut conn = connect_admin().await;
        ensure_swept(&mut conn).await;
        lock_db(&mut conn, db_name).await;
        create_db(&mut conn, db_name)
            .await
            .expect("create test db");
        TempDb {
            name: db_name.to_string(),
            lock: Some(conn),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> String {
        db_url_for(&self.name)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(conn) = self.lock.take() {
            blocking_drop(std::mem::take(&mut self.name), Some(conn));
        }
    }
}

// ---------- 共享运行库（每测试二进制一次运行一个） ----------

struct RunDb {
    name: String,
    owned: bool,
    lock: Option<PgConnection>,
}

static RUN_DB: OnceLock<Mutex<Option<RunDb>>> = OnceLock::new();
static EXIT_HOOK: Once = Once::new();

/// 运行库名：`TEST_DATABASE_NAME` 优先，否则 `chat_it_run_<pid>_<8位hex>`。
/// 进程内稳定（OnceLock）。
pub fn run_db_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| match std::env::var("TEST_DATABASE_NAME") {
        Ok(n) if !n.is_empty() => n,
        _ => format!(
            "chat_it_run_{}_{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
    })
    .clone()
}

/// `TEST_DATABASE_NAME` 未设置 = 我们自己生成的库 = 归我们删。
fn run_db_owned() -> bool {
    !matches!(std::env::var("TEST_DATABASE_NAME"), Ok(n) if !n.is_empty())
}

/// it 二进制共享库的入口：清扫 → 校验库名 → 持锁 → 建库 →
/// 自建库登记 atexit 删除钩子。迁移由调用方负责。
/// 返回库名（URL 用 `db_url_for` 派生）。
pub async fn prepare_run_db() -> String {
    let name = run_db_name();
    assert!(
        valid_db_name(&name),
        "TEST_DATABASE_NAME must match ^[a-z_][a-z0-9_]{{0,62}}$: {name}"
    );
    let mut conn = connect_admin().await;
    ensure_swept(&mut conn).await;
    lock_db(&mut conn, &name).await;
    create_db(&mut conn, &name).await.expect("create run db");
    let owned = run_db_owned();
    let _ = RUN_DB.set(Mutex::new(Some(RunDb {
        name: name.clone(),
        owned,
        lock: Some(conn),
    })));
    if owned {
        EXIT_HOOK.call_once(|| unsafe {
            libc::atexit(exit_drop_run_db);
        });
    }
    name
}

extern "C" fn exit_drop_run_db() {
    let rec = RUN_DB
        .get()
        .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));
    if let Some(rec) = rec {
        if rec.owned {
            blocking_drop(rec.name, rec.lock);
        }
    }
}
