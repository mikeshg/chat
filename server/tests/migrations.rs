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
