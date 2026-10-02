use crate::error::{AppError, AppResult};
use crate::model::{
    AttachmentView, ConversationPref, CreatePollReq, MessageView, PollOptionView, PollView,
    PrefUpdateReq, ReactionView, ReplyPreview,
};
use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use uuid::Uuid;

mod forward;
mod lifecycle;
mod polls;
mod preferences;
mod send;

pub use forward::*;
pub use lifecycle::*;
pub use polls::*;
pub use preferences::*;
pub use send::*;

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

/// 这些用户必须全部是 uid 的好友。建群和拉人共用。
pub async fn ensure_friends(pool: &PgPool, uid: Uuid, ids: &[Uuid]) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM friendships
         WHERE (user_a = $1 AND user_b = ANY($2)) OR (user_b = $1 AND user_a = ANY($2))",
    )
    .bind(uid)
    .bind(ids)
    .fetch_one(pool)
    .await?;
    if n != ids.len() as i64 {
        return Err(AppError::forbidden("can only invite your friends"));
    }
    Ok(())
}

pub struct MemberInfo {
    pub role: String,
    pub last_read_seq: i64,
}

pub async fn membership(
    executor: impl PgExecutor<'_>,
    conv: Uuid,
    uid: Uuid,
) -> Result<Option<MemberInfo>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT role, last_read_seq FROM memberships
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|(role, last_read_seq)| MemberInfo {
        role,
        last_read_seq,
    }))
}

/// 会话行锁：成员变更（转让/退群/踢人/拉人/解散）与发消息共用的串行化点。
/// 返回 (type, owner_id, 已解散)；会话不存在返回 None。
pub async fn lock_conversation(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
) -> Result<Option<(String, Option<Uuid>, bool)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, Option<Uuid>, bool)>(
        "SELECT type, owner_id, dissolved_at IS NOT NULL
           FROM conversations WHERE id = $1 FOR UPDATE",
    )
    .bind(conv)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row)
}

pub async fn member_ids(
    executor: impl PgExecutor<'_>,
    conv: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let ids =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM memberships WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(executor)
            .await?;
    Ok(ids)
}

pub async fn conv_kind(executor: impl PgExecutor<'_>, conv: Uuid) -> AppResult<Option<String>> {
    let kind = sqlx::query_scalar::<_, String>("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(executor)
        .await?;
    Ok(kind)
}

/// reactions 聚合原始行（jsonb / 直接查询两用）：emoji -> 全部反应者。
/// reacted_by_me 由 into_view(viewer) 或调用方按接收者计算。
#[derive(Debug, Clone, serde::Deserialize, sqlx::FromRow)]
pub struct ReactionAgg {
    pub emoji: String,
    pub users: Vec<Uuid>,
}

/// message_views.poll jsonb 的反序列化形状（库内聚合，含全部投票者 id）。
/// 匿名掩蔽与 my_option_ids 在 into_view(viewer) 逐人完成——聚合行本身不出库。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PollOptionAgg {
    pub id: Uuid,
    pub index: i32,
    pub label: String,
    pub count: i64,
    #[serde(default)]
    pub voters: Vec<Uuid>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PollAgg {
    pub question: String,
    pub allow_multiple: bool,
    pub anonymous: bool,
    pub closed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expired: bool,
    pub version: i64,
    pub total_voters: i64,
    #[serde(default)]
    pub options: Vec<PollOptionAgg>,
}

impl PollAgg {
    /// 按观看者个性化：匿名投票剥离他人 voters，但 my_option_ids 仍含本人选择。
    pub fn into_view(self, viewer: Uuid) -> PollView {
        let my_option_ids = self
            .options
            .iter()
            .filter(|o| o.voters.contains(&viewer))
            .map(|o| o.id)
            .collect();
        let anonymous = self.anonymous;
        PollView {
            question: self.question,
            allow_multiple: self.allow_multiple,
            anonymous,
            closed_at: self.closed_at,
            expires_at: self.expires_at,
            expired: self.expired,
            version: self.version,
            total_voters: self.total_voters,
            options: self
                .options
                .into_iter()
                .map(|o| PollOptionView {
                    id: o.id,
                    index: o.index,
                    label: o.label,
                    count: o.count,
                    voters: if anonymous { Vec::new() } else { o.voters },
                })
                .collect(),
            my_option_ids,
        }
    }
}

/// 带发送者展示信息的消息行，列与 message_views 一致。
#[derive(sqlx::FromRow, Clone)]
pub struct MsgViewRow {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub sender_id: Uuid,
    pub sender_username: String,
    pub sender_name: String,
    pub content: String,
    pub kind: String,
    pub att_id: Option<Uuid>,
    pub att_name: Option<String>,
    pub att_mime: Option<String>,
    pub att_size: Option<i64>,
    pub mentions: Option<Vec<Uuid>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub reply_to_id: Option<Uuid>,
    pub reply_to: Option<sqlx::types::Json<ReplyPreview>>,
    pub edited_at: Option<chrono::DateTime<chrono::Utc>>,
    pub edit_version: i32,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reactions: sqlx::types::Json<Vec<ReactionAgg>>,
    /// kind='poll' 消息的投票聚合（墓碑已掩去）；into_view 按 viewer 个性化
    pub poll: Option<sqlx::types::Json<PollAgg>>,
    // V2 0012：附件尺寸/缩略图/语音时长（墓碑为 NULL）、发送者头像、转发来源
    pub att_width: Option<i32>,
    pub att_height: Option<i32>,
    pub att_has_thumb: Option<bool>,
    pub att_duration_ms: Option<i32>,
    pub sender_avatar_version: i32,
    pub sender_has_avatar: bool,
    pub forwarded_from: Option<String>,
}

impl MsgViewRow {
    /// viewer 决定 reactions 里的 reacted_by_me；广播时必须逐成员分别构造。
    pub fn into_view(self, viewer: Uuid) -> MessageView {
        MessageView {
            id: self.id,
            conversation_id: self.conversation_id,
            seq: self.seq,
            sender_id: self.sender_id,
            sender_username: self.sender_username,
            sender_name: self.sender_name,
            content: self.content,
            kind: self.kind,
            attachment: self.att_id.map(|id| AttachmentView {
                id,
                file_name: self.att_name.unwrap_or_default(),
                mime: self.att_mime.unwrap_or_default(),
                size: self.att_size.unwrap_or_default(),
                width: self.att_width,
                height: self.att_height,
                has_thumb: self.att_has_thumb.unwrap_or(false),
                duration_ms: self.att_duration_ms,
            }),
            mentions: self.mentions.unwrap_or_default(),
            reply_to_id: self.reply_to_id,
            reply_to_preview: self.reply_to.map(|j| j.0),
            edited_at: self.edited_at,
            edit_version: self.edit_version,
            deleted_at: self.deleted_at,
            reactions: self
                .reactions
                .0
                .iter()
                .map(|r| ReactionView {
                    emoji: r.emoji.clone(),
                    count: r.users.len() as i64,
                    reacted_by_me: r.users.contains(&viewer),
                })
                .collect(),
            poll: self.poll.map(|j| j.0.into_view(viewer)),
            created_at: self.created_at,
            sender_avatar_version: self.sender_avatar_version,
            sender_has_avatar: self.sender_has_avatar,
            forwarded_from: self.forwarded_from,
        }
    }
}

pub async fn messages_after(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    after_seq: i64,
    limit: i64,
) -> AppResult<Vec<MessageView>> {
    let rows = sqlx::query_as::<_, MsgViewRow>(
        "SELECT * FROM message_views
          WHERE conversation_id = $1 AND seq > $2
          ORDER BY seq ASC
          LIMIT $3",
    )
    .bind(conv)
    .bind(after_seq)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.into_view(viewer)).collect())
}

pub async fn messages_before(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    before_seq: i64,
    limit: i64,
) -> AppResult<Vec<MessageView>> {
    let mut rows = sqlx::query_as::<_, MsgViewRow>(
        "SELECT * FROM message_views
          WHERE conversation_id = $1 AND seq < $2
          ORDER BY seq DESC
          LIMIT $3",
    )
    .bind(conv)
    .bind(before_seq)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(rows.into_iter().map(|r| r.into_view(viewer)).collect())
}

pub async fn messages_latest(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    limit: i64,
) -> AppResult<Vec<MessageView>> {
    let mut rows = sqlx::query_as::<_, MsgViewRow>(
        "SELECT * FROM message_views
          WHERE conversation_id = $1
          ORDER BY seq DESC
          LIMIT $2",
    )
    .bind(conv)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(rows.into_iter().map(|r| r.into_view(viewer)).collect())
}

/// ILIKE 子串匹配的字面量转义（% _ \ 不当通配符），两处搜索共用
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 会话内搜索（V2 P3-1 改为两阶段）：先在 messages 表取候选 id
/// （deleted_at IS NULL 排除墓碑——搜「消息已删除」不再命中占位文本；
/// 正文上的 trgm 部分索引可用），再按 id 回读 message_views 组装视图。
pub async fn search_messages(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    needle: &str,
    limit: i64,
) -> AppResult<Vec<MessageView>> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM messages
          WHERE conversation_id = $1 AND deleted_at IS NULL
            AND content ILIKE '%' || $2 || '%' ESCAPE '\\'
          ORDER BY seq DESC
          LIMIT $3",
    )
    .bind(conv)
    .bind(like_escape(needle))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // messages_by_ids 按 seq ASC 返回，倒序恢复候选顺序
    let mut msgs = messages_by_ids(pool, conv, viewer, &ids).await?;
    msgs.reverse();
    Ok(msgs)
}

/// GET /v1/search 的筛选条件；api 层已校验形状，q 与筛选至少有一个才会走到这里。
#[derive(Default)]
pub struct SearchFilter {
    /// 已 trim 的非空关键词
    pub q: Option<String>,
    pub conversation_id: Option<Uuid>,
    pub sender_id: Option<Uuid>,
    pub kind: Option<String>,
    /// 含 from、不含 to
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// 翻页游标：(created_at, id) 严格小于该点；before_id 缺省按最小 uuid
    pub before: Option<DateTime<Utc>>,
    pub before_id: Option<Uuid>,
}

/// 全局消息搜索：只在 viewer 当前是成员的会话里搜未删除消息，按 created_at DESC, id DESC。
/// 先在 messages 表取候选 id（成员 JOIN + 筛选 + 游标），多取一条探测 has_more；
/// 再回读 message_views 逐条按 viewer 个性化（成员身份已在候选查询中强制）。
pub async fn search_global(
    pool: &PgPool,
    viewer: Uuid,
    f: &SearchFilter,
    limit: i64,
) -> AppResult<(Vec<MessageView>, bool)> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT m.id FROM messages m
          JOIN memberships mb
            ON mb.conversation_id = m.conversation_id AND mb.user_id = $1
          WHERE m.deleted_at IS NULL
            AND ($2::text IS NULL OR m.content ILIKE '%' || $2 || '%' ESCAPE '\\')
            AND ($3::uuid IS NULL OR m.conversation_id = $3)
            AND ($4::uuid IS NULL OR m.sender_id = $4)
            AND ($5::text IS NULL OR m.kind = $5)
            AND ($6::timestamptz IS NULL OR m.created_at >= $6)
            AND ($7::timestamptz IS NULL OR m.created_at < $7)
            AND ($8::timestamptz IS NULL OR (m.created_at, m.id) < ($8, $9::uuid))
          ORDER BY m.created_at DESC, m.id DESC
          LIMIT $10",
    )
    .bind(viewer)
    .bind(f.q.as_deref().map(like_escape))
    .bind(f.conversation_id)
    .bind(f.sender_id)
    .bind(f.kind.as_deref())
    .bind(f.from)
    .bind(f.to)
    .bind(f.before)
    .bind(f.before_id.unwrap_or(Uuid::nil()))
    .bind(limit + 1)
    .fetch_all(pool)
    .await?;
    let has_more = ids.len() as i64 > limit;
    let ids = &ids[..ids.len().min(limit.max(0) as usize)];
    if ids.is_empty() {
        return Ok((vec![], false));
    }
    let mut rows =
        sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?;
    // ANY 不保序：按候选口径重排（created_at DESC, id DESC）
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok((
        rows.into_iter().map(|r| r.into_view(viewer)).collect(),
        has_more,
    ))
}

/// 重连批量刷新：按 id 回读当前读模型（墓碑/正文/reaction/回复预览均以库为准）。
/// 只返回属于该会话的行；会话外/不存在的 id 静默忽略。
pub async fn messages_by_ids(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    ids: &[Uuid],
) -> AppResult<Vec<MessageView>> {
    let rows = sqlx::query_as::<_, MsgViewRow>(
        "SELECT * FROM message_views
          WHERE conversation_id = $1 AND id = ANY($2)
          ORDER BY seq ASC",
    )
    .bind(conv)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.into_view(viewer)).collect())
}

/// 文件传输助手：每人一个 type='self' 单人会话（owner_id=本人、唯一成员 owner）。
/// 注册事务、迁移回填与调用方兜底共用；uq_self_conversation 部分唯一索引 +
/// memberships PK 使并发/重复调用幂等。单条 CTE 原子完成。返回会话 id。
pub async fn ensure_self_conversation(
    executor: impl PgExecutor<'_>,
    uid: Uuid,
) -> Result<Uuid, sqlx::Error> {
    let conv = sqlx::query_scalar::<_, Uuid>(
        "WITH ins AS (
             INSERT INTO conversations (type, owner_id) VALUES ('self', $1)
             ON CONFLICT (owner_id) WHERE type = 'self' DO NOTHING
             RETURNING id
         ),
         conv AS (
             SELECT id FROM ins
             UNION ALL
             SELECT id FROM conversations WHERE owner_id = $1 AND type = 'self'
             LIMIT 1
         ),
         mem AS (
             INSERT INTO memberships (conversation_id, user_id, role)
             SELECT id, $1, 'owner' FROM conv
             ON CONFLICT DO NOTHING
         )
         SELECT id FROM conv",
    )
    .bind(uid)
    .fetch_one(executor)
    .await?;
    Ok(conv)
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

#[derive(Debug)]
pub enum PostError {
    Deny {
        code: &'static str,
        message: &'static str,
    },
    Db(sqlx::Error),
}

impl PostError {
    fn deny(code: &'static str, message: &'static str) -> Self {
        Self::Deny { code, message }
    }

    pub fn into_ws(self) -> (&'static str, String) {
        match self {
            Self::Deny { code, message } => (code, message.to_string()),
            Self::Db(e) => {
                tracing::error!(error = %e, "post_message failed");
                ("internal", "db error".into())
            }
        }
    }
}

enum WriteError {
    Fail(PostError),
    /// 并发插入撞上同一 client_msg_id，事务已失败，需要回滚后读取已提交行。
    IdempotentHit,
}

impl From<sqlx::Error> for WriteError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(ref db) = e {
            match db.constraint() {
                Some("uq_messages_client_msg") => return Self::IdempotentHit,
                Some("uq_messages_attachment") => {
                    return Self::Fail(PostError::deny(
                        "bad_attachment",
                        "attachment already sent",
                    ));
                }
                _ => {}
            }
            if db.code().as_deref() == Some("23514") {
                return Self::Fail(PostError::deny(
                    "bad_message",
                    "message must be 1-4000 chars",
                ));
            }
        }
        Self::Fail(PostError::Db(e))
    }
}

impl From<PostError> for WriteError {
    fn from(e: PostError) -> Self {
        Self::Fail(e)
    }
}
