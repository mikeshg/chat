use chat_server::domain;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use uuid::Uuid;

#[tokio::test]
async fn upgrade_preserves_shared_attachment_references_and_cleanup() {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:5432/postgres".into());
    let options = PgConnectOptions::from_str(&database_url).unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!("chat_migration_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    for script in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_attachments.sql"),
        include_str!("../migrations/0003_mentions.sql"),
        include_str!("../migrations/0004_admin.sql"),
    ] {
        sqlx::raw_sql(script).execute(&pool).await.unwrap();
    }

    let user = Uuid::new_v4();
    let conv = Uuid::new_v4();
    let attachment = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,username,display_name,password_hash) VALUES ($1,'migration_user','Migration','unused')")
        .bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO conversations (id,type,name,owner_id,last_seq) VALUES ($1,'group','Migration',$2,3)")
        .bind(conv).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO memberships (conversation_id,user_id,role) VALUES ($1,$2,'owner')")
        .bind(conv)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO attachments (id,uploader_id,conv_id,file_name,mime,size,storage_path,created_at) VALUES ($1,$2,$3,'shared.txt','text/plain',7,'migration-shared-path','2026-01-01T00:00:00Z')")
        .bind(attachment).bind(user).bind(conv).execute(&pool).await.unwrap();
    let mut ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    ids.sort();
    for (index, id) in ids.iter().enumerate() {
        sqlx::query("INSERT INTO messages (id,conversation_id,seq,sender_id,content,kind,attachment_id,created_at) VALUES ($1,$2,$3,$4,'[file] shared.txt','file',$5,'2026-01-02T00:00:00Z')")
            .bind(id).bind(conv).bind((index + 1) as i64).bind(user).bind(attachment)
            .execute(&pool).await.unwrap();
    }
    for script in [
        include_str!("../migrations/0005_sync_invariants.sql"),
        include_str!("../migrations/0006_session_version.sql"),
        include_str!("../migrations/0007_message_lifecycle.sql"),
        include_str!("../migrations/0008_conversation_preferences.sql"),
        include_str!("../migrations/0009_polls.sql"),
        // 新迁移必须追加到这里：夹具之后调用的领域函数读取最新 message_views 列
        include_str!("../migrations/0010_server_settings_invites.sql"),
        include_str!("../migrations/0011_sessions.sql"),
        include_str!("../migrations/0012_v2_media.sql"),
        include_str!("../migrations/0015_self_conversations.sql"),
        include_str!("../migrations/0016_search_trgm.sql"),
        include_str!("../migrations/0017_announcements_pins.sql"),
        include_str!("../migrations/0018_push.sql"),
        include_str!("../migrations/0019_agents.sql"),
        include_str!("../migrations/0020_agent_settings.sql"),
        include_str!("../migrations/0021_agent_queue.sql"),
        include_str!("../migrations/0022_agent_contexts.sql"),
        include_str!("../migrations/0023_agent_config.sql"),
    ] {
        sqlx::raw_sql(script).execute(&pool).await.unwrap();
    }

    let references = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT id,attachment_id FROM messages ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(references.len(), 3);
    assert!(
        references.iter().all(|(_, att)| att.is_some()),
        "upgrade must preserve every historical attachment association"
    );
    assert_eq!(references[0], (ids[0], Some(attachment)));
    let distinct =
        sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT attachment_id) FROM messages")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(distinct, 3);
    let preserved = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM attachments WHERE uploader_id=$1 AND conv_id=$2 AND file_name='shared.txt' AND mime='text/plain' AND size=7 AND storage_path='migration-shared-path' AND created_at='2026-01-01T00:00:00Z'",
    )
    .bind(user)
    .bind(conv)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(preserved, 3);
    let visible =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM message_views WHERE att_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(visible, 3);

    for (index, id) in ids.iter().enumerate() {
        let deleted = domain::delete_message(&pool, *id, user).await.unwrap();
        if index < 2 {
            assert!(deleted.orphan_files.is_empty());
            let remaining = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM message_views WHERE att_id IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(remaining, 2 - index as i64);
        } else {
            assert_eq!(deleted.orphan_files, vec!["migration-shared-path"]);
        }
    }
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
}

/// V2 P2-4：0015 为 0015 之前已存在的用户回填 type='self' 会话与 owner 成员；
/// 重复执行幂等（uq_self_conversation 部分唯一索引 + memberships PK 兜底）。
#[tokio::test]
async fn upgrade_backfills_self_conversations() {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:5432/postgres".into());
    let options = PgConnectOptions::from_str(&database_url).unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!("chat_migration_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    for script in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_attachments.sql"),
        include_str!("../migrations/0003_mentions.sql"),
        include_str!("../migrations/0004_admin.sql"),
        include_str!("../migrations/0005_sync_invariants.sql"),
        include_str!("../migrations/0006_session_version.sql"),
        include_str!("../migrations/0007_message_lifecycle.sql"),
        include_str!("../migrations/0008_conversation_preferences.sql"),
        include_str!("../migrations/0009_polls.sql"),
        include_str!("../migrations/0010_server_settings_invites.sql"),
        include_str!("../migrations/0011_sessions.sql"),
        include_str!("../migrations/0012_v2_media.sql"),
    ] {
        sqlx::raw_sql(script).execute(&pool).await.unwrap();
    }

    // 旧 schema 上的两个既有用户
    let u1 = Uuid::new_v4();
    let u2 = Uuid::new_v4();
    for (u, name) in [(u1, "old_user_a"), (u2, "old_user_b")] {
        sqlx::query(
            "INSERT INTO users (id,username,display_name,password_hash) VALUES ($1,$2,$3,'unused')",
        )
        .bind(u)
        .bind(name)
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
    }

    // 0015 跑两遍：回填必须幂等（不重复建会话、不重复建成员）
    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../migrations/0015_self_conversations.sql"))
            .execute(&pool)
            .await
            .unwrap();
    }

    let total =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE type = 'self'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(total, 2, "每个用户恰一个 self 会话");

    for u in [u1, u2] {
        let (name_null, owners): (bool, i64) = sqlx::query_as(
            "SELECT c.name IS NULL,
                    (SELECT count(*) FROM memberships mb
                      WHERE mb.conversation_id = c.id AND mb.user_id = $1 AND mb.role = 'owner')
               FROM conversations c
              WHERE c.type = 'self' AND c.owner_id = $1",
        )
        .bind(u)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(name_null, "self 会话 name 必须为 NULL");
        assert_eq!(owners, 1, "self 会话恰一个 owner 成员");
    }

    // 注册/兜底共用的领域函数在已迁移库上同样幂等
    let again = domain::ensure_self_conversation(&pool, u1).await.unwrap();
    let total = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM conversations WHERE type = 'self' AND owner_id = $1",
    )
    .bind(u1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 1);
    let member_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memberships WHERE conversation_id = $1")
            .bind(again)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(member_count, 1);

    // 0018：push_settings 建表
    sqlx::raw_sql(include_str!("../migrations/0018_push.sql"))
        .execute(&pool)
        .await
        .unwrap();

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
}

/// V2 P3-1：0016 在已有数据上创建 pg_trgm 扩展与 messages.content 部分 GIN 索引。
#[tokio::test]
async fn upgrade_creates_search_trgm_index() {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chat:chat@localhost:5432/postgres".into());
    let options = PgConnectOptions::from_str(&database_url).unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!("chat_migration_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    for script in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_attachments.sql"),
        include_str!("../migrations/0003_mentions.sql"),
        include_str!("../migrations/0004_admin.sql"),
        include_str!("../migrations/0005_sync_invariants.sql"),
        include_str!("../migrations/0006_session_version.sql"),
        include_str!("../migrations/0007_message_lifecycle.sql"),
        include_str!("../migrations/0008_conversation_preferences.sql"),
        include_str!("../migrations/0009_polls.sql"),
        include_str!("../migrations/0010_server_settings_invites.sql"),
        include_str!("../migrations/0011_sessions.sql"),
        include_str!("../migrations/0012_v2_media.sql"),
        include_str!("../migrations/0015_self_conversations.sql"),
    ] {
        sqlx::raw_sql(script).execute(&pool).await.unwrap();
    }

    // 0016 之前已存在的数据：一条活消息 + 一条墓碑（content 已被占位文本覆写）
    let u = Uuid::new_v4();
    let conv = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,username,display_name,password_hash) VALUES ($1,'trgm_user','Trgm','unused')")
        .bind(u).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO conversations (id,type,name,owner_id) VALUES ($1,'group','Trgm',$2)")
        .bind(conv)
        .bind(u)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (conversation_id,user_id,role) VALUES ($1,$2,'owner')")
        .bind(conv)
        .bind(u)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (conversation_id,seq,sender_id,content) VALUES ($1,1,$2,'live keyword')")
        .bind(conv).bind(u).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO messages (conversation_id,seq,sender_id,content,deleted_at) VALUES ($1,2,$2,'消息已删除',now())")
        .bind(conv).bind(u).execute(&pool).await.unwrap();

    sqlx::raw_sql(include_str!("../migrations/0016_search_trgm.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let ext = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ext, "pg_trgm extension must exist");
    let idx = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_indexes
          WHERE tablename = 'messages' AND indexname = 'idx_messages_content_trgm')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(idx, "trgm index must exist on existing data");

    // 部分索引只覆盖未删除行：活消息可命中，墓碑行被 deleted_at 条件排除
    let hits = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM messages
          WHERE deleted_at IS NULL AND content ILIKE '%keyword%' ESCAPE '\\'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hits, 1);

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
}
