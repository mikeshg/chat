use crate::error::AppResult;
use crate::model::MessageView;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub fn canonical_pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

pub async fn is_friend(pool: &PgPool, a: Uuid, b: Uuid) -> AppResult<bool> {
    let (lo, hi) = canonical_pair(a, b);
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM friendships WHERE user_a = $1 AND user_b = $2)",
    )
    .bind(lo)
    .bind(hi)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

pub struct MemberInfo {
    pub role: String,
    pub last_read_seq: i64,
}

pub async fn membership(pool: &PgPool, conv: Uuid, uid: Uuid) -> AppResult<Option<MemberInfo>> {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT role, last_read_seq FROM memberships
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(role, last_read_seq)| MemberInfo {
        role,
        last_read_seq,
    }))
}

pub async fn member_ids(pool: &PgPool, conv: Uuid) -> AppResult<Vec<Uuid>> {
    let ids =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM memberships WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(pool)
            .await?;
    Ok(ids)
}

pub async fn conv_kind(pool: &PgPool, conv: Uuid) -> AppResult<Option<String>> {
    let kind = sqlx::query_scalar::<_, String>("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(pool)
        .await?;
    Ok(kind)
}

/// 事务内分配 seq 并写入消息，返回完整视图。conversations.last_seq 行锁保证群内有序无空洞。
pub async fn append_message(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    sender: Uuid,
    content: &str,
) -> AppResult<MessageView> {
    let seq = sqlx::query_scalar::<_, i64>(
        "UPDATE conversations SET last_seq = last_seq + 1 WHERE id = $1 RETURNING last_seq",
    )
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;

    let msg = sqlx::query_as::<_, MessageRow>(
        "INSERT INTO messages (conversation_id, seq, sender_id, content)
         VALUES ($1, $2, $3, $4)
         RETURNING id, conversation_id, seq, sender_id, content, created_at",
    )
    .bind(conv)
    .bind(seq)
    .bind(sender)
    .bind(content)
    .fetch_one(&mut **tx)
    .await?;

    let (sender_username, sender_name) = sqlx::query_as::<_, (String, String)>(
        "SELECT username::text, display_name FROM users WHERE id = $1",
    )
    .bind(sender)
    .fetch_one(&mut **tx)
    .await?;

    Ok(MessageView {
        id: msg.id,
        conversation_id: msg.conversation_id,
        seq: msg.seq,
        sender_id: msg.sender_id,
        sender_username,
        sender_name,
        content: msg.content,
        created_at: msg.created_at,
    })
}

#[derive(sqlx::FromRow)]
pub struct MessageRow {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub sender_id: Uuid,
    pub content: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 带发送者展示信息的消息行（JOIN users）
#[derive(sqlx::FromRow)]
pub struct MsgViewRow {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub sender_id: Uuid,
    pub sender_username: String,
    pub sender_name: String,
    pub content: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl MsgViewRow {
    pub fn into_view(self) -> MessageView {
        MessageView {
            id: self.id,
            conversation_id: self.conversation_id,
            seq: self.seq,
            sender_id: self.sender_id,
            sender_username: self.sender_username,
            sender_name: self.sender_name,
            content: self.content,
            created_at: self.created_at,
        }
    }
}

/// 好友成立时确保 DM 会话存在（dm_key 幂等），双方为 member。返回会话 id。
pub async fn ensure_dm(tx: &mut Transaction<'_, Postgres>, a: Uuid, b: Uuid) -> AppResult<Uuid> {
    let (lo, hi) = canonical_pair(a, b);
    let key = format!("{lo}:{hi}");

    sqlx::query(
        "INSERT INTO conversations (type, dm_key) VALUES ('dm', $1)
         ON CONFLICT (dm_key) WHERE type = 'dm' DO NOTHING",
    )
    .bind(&key)
    .execute(&mut **tx)
    .await?;

    let conv_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM conversations WHERE dm_key = $1")
        .bind(&key)
        .fetch_one(&mut **tx)
        .await?;

    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role)
         VALUES ($1, $2, 'member'), ($1, $3, 'member')
         ON CONFLICT DO NOTHING",
    )
    .bind(conv_id)
    .bind(a)
    .bind(b)
    .execute(&mut **tx)
    .await?;

    Ok(conv_id)
}
