use super::*;

// ===== MM5：测试库守卫/清扫器自身的行为验证 =====

/// TempDb 正常作用域结束即删库。
#[tokio::test]
async fn mm5_temp_db_dropped_at_scope_end() {
    let name = format!("chat_t_mm5ok_{}", uniq());
    {
        let db = testdb::TempDb::create(&name).await;
        assert!(testdb::db_exists(db.name()).await);
    }
    // 守卫 Drop 是阻塞式删库，返回时库应当已删（留轮询余量防抖）
    let deadline = std::time::Instant::now() + Dur::from_secs(10);
    while testdb::db_exists(&name).await {
        assert!(
            std::time::Instant::now() < deadline,
            "{name} not dropped after guard scope end"
        );
        tokio::time::sleep(Dur::from_millis(100)).await;
    }
}

/// panic 展开路径也删库：把「建库后 panic」放进独立线程模拟挂掉的测试
/// （tokio runtime 里不能再 block_on，用新线程自带 runtime 驱动），
/// join 返回 Err 证明确实 panic 过，随后库必须已消失。
#[tokio::test]
async fn mm5_panicking_test_still_drops_its_temp_db() {
    let name = format!("chat_t_mm5panic_{}", uniq());
    let n2 = name.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let _db = testdb::TempDb::create(&n2).await;
            panic!("simulated test panic");
        });
    });
    assert!(handle.join().is_err(), "子线程应当 panic");
    let deadline = std::time::Instant::now() + Dur::from_secs(15);
    while testdb::db_exists(&name).await {
        assert!(
            std::time::Instant::now() < deadline,
            "{name} must be dropped on panic unwind"
        );
        tokio::time::sleep(Dur::from_millis(100)).await;
    }
}

/// 清扫器：删「没有活着属主」的受控前缀库（模拟 SIGKILL 残留：裸建、不持锁），
/// 但跳过持锁的活库，且永远不动前缀之外的库（chat_test 存在时也不得被碰）。
#[tokio::test]
async fn mm5_sweeper_drops_only_orphaned_test_dbs() {
    let mut admin = testdb::connect_admin().await;
    let orphan = format!("chat_t_mm5orphan_{}", uniq());
    sqlx::query(&format!("CREATE DATABASE \"{orphan}\""))
        .execute(&mut admin)
        .await
        .unwrap();
    assert!(testdb::db_exists(&orphan).await);

    // 存活库：持锁的 TempDb 必须躲过本轮清扫
    let live = testdb::TempDb::create(&format!("chat_t_mm5live_{}", uniq())).await;

    testdb::sweep_stale_test_dbs(&mut admin).await;
    assert!(!testdb::db_exists(&orphan).await, "孤儿库应被清扫");
    assert!(testdb::db_exists(live.name()).await, "持锁活库不得被清扫");
    // chat_test 不在受控前缀内：即便存在也绝不被清扫
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = 'chat_test')",
    )
    .fetch_one(&mut admin)
    .await
    .unwrap_or(false)
    {
        testdb::sweep_stale_test_dbs(&mut admin).await;
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = 'chat_test')",
            )
            .fetch_one(&mut admin)
            .await
            .unwrap_or(false),
            "sweeper must never touch chat_test"
        );
    }
}

/// MM5_FORCE_PANIC=1 时人为 panic：供在真实 `cargo test` 进程里验证
/// 「测试 panic → 守卫删库」。用法：
///   MM5_FORCE_PANIC=1 cargo test --locked --test it mm5_forced_panic_drops_temp_db
/// 测试本身会失败（panic），但库必须被删——之后查 pg_database 应无残留。
#[tokio::test]
async fn mm5_forced_panic_drops_temp_db() {
    let db = testdb::TempDb::create(&format!("chat_t_mm5fp_{}", uniq())).await;
    if std::env::var("MM5_FORCE_PANIC").is_ok() {
        panic!("MM5_FORCE_PANIC: 人为 panic，{} 必须仍被守卫删除", db.name());
    }
    assert!(testdb::db_exists(db.name()).await);
}
