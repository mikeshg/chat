use crate::error::{AppError, AppResult};
use crate::model::{
    AttachmentView, ConversationPref, CreatePollReq, MessageView, PollOptionView, PollView,
    PrefUpdateReq, ReactionView, ReplyPreview,
};
use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
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
    // Agent A01（0019）：非空 = 该消息由 Agent 发出
    pub agent_id: Option<Uuid>,
    pub agent_name: Option<String>,
    // Agent A06：消息上的 Agent 头像版本（无 Agent 时为 NULL）
    pub agent_avatar_version: Option<i32>,
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
            agent: self.agent_id.map(|id| crate::model::AgentRef {
                id,
                name: self.agent_name.unwrap_or_default(),
                avatar_version: self.agent_avatar_version.unwrap_or(0),
            }),
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

// ---------- 发消息（一个事务守住成员、私聊可写、附件、seq、幂等键） ----------

pub struct PostMessage<'a> {
    pub conversation_id: Uuid,
    pub sender_id: Uuid,
    pub content: &'a str,
    pub client_msg_id: Option<&'a str>,
    pub attachment_id: Option<Uuid>,
    /// 回复目标：必须为同会话消息（事务内校验）
    pub reply_to_id: Option<Uuid>,
    /// 投票负载：Some 时本条消息为 kind='poll'（仅群聊、无附件）；
    /// 形状已由 validate_poll_draft 整形（question/options 去首尾空白、去重）
    pub poll: Option<PollSpec>,
    /// 转发来源（仅服务端转发路径）：原发送者展示名快照，写入 messages.forwarded_from
    pub forwarded_from: Option<&'a str>,
    /// 转发专用「已校验附件」：源消息上的 attachment id，事务内在目标会话克隆一行。
    /// 与 attachment_id 互斥——attachment_id 仍要求 uploader=发送者，此路径不可达普通发送。
    pub forward_attachment: Option<Uuid>,
    /// Agent A01：Some(agent) 表示本条由该 Agent 发出（sender_id 仍是主人）。
    /// 仅服务端宿主回写路径可达；写入时校验该 Agent 恰属于目标会话。
    pub agent_id: Option<Uuid>,
}

/// 已整形的投票定义（随消息同事务落库）
pub struct PollSpec {
    pub question: String,
    pub options: Vec<String>,
    pub allow_multiple: bool,
    pub anonymous: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct Posted {
    pub message: MessageView,
    pub inserted: bool,
    pub member_ids: Vec<Uuid>,
    /// 本条消息 @到且原已归档的用户：其 archived_at 已在事务内清除，
    /// 广播方负责给这些用户本人推送 preference_updated（仅本人可见）
    pub unarchived: Vec<Uuid>,
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

pub async fn post_message(pool: &PgPool, msg: PostMessage<'_>) -> Result<Posted, PostError> {
    let client_msg_id = normalize_client_msg_id(msg.client_msg_id)?;
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match write_message(&mut tx, &msg, client_msg_id.as_deref()).await {
        Ok(posted) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(posted)
        }
        Err(WriteError::IdempotentHit) => {
            let _ = tx.rollback().await;
            let key = client_msg_id
                .as_deref()
                .ok_or_else(|| PostError::deny("internal", "db error"))?;
            load_by_client_key(pool, msg.conversation_id, msg.sender_id, key).await
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

fn normalize_client_msg_id(raw: Option<&str>) -> Result<Option<String>, PostError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if raw.chars().count() > 80 {
        return Err(PostError::deny("bad_message", "client_msg_id too long"));
    }
    Ok(Some(raw.to_string()))
}

async fn write_message(
    tx: &mut Transaction<'_, Postgres>,
    msg: &PostMessage<'_>,
    client_msg_id: Option<&str>,
) -> Result<Posted, WriteError> {
    let conv = sqlx::query_as::<_, (String, bool, Option<Uuid>)>(
        "SELECT type, dissolved_at IS NOT NULL, agent_id
           FROM conversations WHERE id = $1 FOR UPDATE",
    )
    .bind(msg.conversation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    let (kind_name, dissolved, conv_agent) = conv;

    let disabled =
        sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NOT NULL FROM users WHERE id = $1")
            .bind(msg.sender_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    if disabled {
        return Err(PostError::deny("forbidden", "账号已被禁用").into());
    }
    if dissolved {
        return Err(PostError::deny("forbidden", "not a member of this conversation").into());
    }

    let is_member = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(msg.conversation_id)
    .bind(msg.sender_id)
    .fetch_one(&mut **tx)
    .await?;
    if !is_member {
        return Err(PostError::deny("forbidden", "not a member of this conversation").into());
    }

    if kind_name == "dm" && !dm_writable(tx, msg.conversation_id, msg.sender_id).await? {
        return Err(PostError::deny("not_friends", "好友关系已解除，会话只读").into());
    }

    // 已删除的 Agent：专属会话只读。用户消息与宿主回写都不落库，也就不会再触发调用。
    if kind_name == "agent" {
        let aid = conv_agent.ok_or_else(|| PostError::deny("agent_deleted", "agent_deleted"))?;
        let gone = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NOT NULL FROM agents WHERE id = $1",
        )
        .bind(aid)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(true);
        if gone {
            return Err(PostError::deny("agent_deleted", "agent_deleted").into());
        }
        // 用户自己发的消息：没使用权就不落库。Agent 回写（带 agent_id）不在这里拦。
        if msg.agent_id.is_none() {
            let allowed = crate::agent_access::user_allows(&mut **tx, msg.sender_id)
                .await
                .map_err(PostError::Db)?;
            if !allowed {
                return Err(PostError::deny("agent_access_denied", "agent_access_denied").into());
            }
        }
    }

    // Agent 消息只能写进该 Agent 的专属会话（A01：宿主回写路径在锁外已验主，
    // 这里再锁内核对会话绑定关系，防跨会话/伪造 agent_id）
    if let Some(aid) = msg.agent_id {
        if kind_name != "agent" || conv_agent != Some(aid) {
            return Err(
                PostError::deny("bad_request", "agent message outside its conversation").into(),
            );
        }
    }

    // 权限通过后再认幂等键，避免被移出或禁用的连接靠重放把旧消息再广播一遍。
    if let Some(key) = client_msg_id {
        if let Some(id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM messages
              WHERE conversation_id = $1 AND sender_id = $2 AND client_msg_id = $3",
        )
        .bind(msg.conversation_id)
        .bind(msg.sender_id)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?
        {
            // 幂等命中：归档解除在首个事务已生效，不再重复推送
            return Ok(load_posted(
                tx,
                msg.conversation_id,
                msg.sender_id,
                id,
                false,
                Vec::new(),
            )
            .await?);
        }
    }

    // 回复目标必须是同会话内的消息（行锁内校验，防跨会话引用）
    if let Some(rt) = msg.reply_to_id {
        let ok = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1 AND conversation_id = $2)",
        )
        .bind(rt)
        .bind(msg.conversation_id)
        .fetch_one(&mut **tx)
        .await?;
        if !ok {
            return Err(PostError::deny("bad_message", "reply target not found").into());
        }
    }

    // 投票前置校验（会话行锁内）：仅群聊、不带附件；截止时间必须在库时间的未来。
    // kind='poll' 只能从这里产生——普通 send_message 的 kind 由正文/附件推导，无法伪造。
    if let Some(spec) = &msg.poll {
        if kind_name != "group" {
            return Err(
                PostError::deny("bad_request", "polls are only supported in groups").into(),
            );
        }
        if msg.attachment_id.is_some() {
            return Err(PostError::deny("bad_request", "poll cannot carry attachments").into());
        }
        if let Some(exp) = spec.expires_at {
            let past = sqlx::query_scalar::<_, bool>("SELECT $1::timestamptz <= now()")
                .bind(exp)
                .fetch_one(&mut **tx)
                .await?;
            if past {
                return Err(PostError::deny(
                    "bad_request",
                    "poll expires_at must be in the future",
                )
                .into());
            }
        }
    }

    let attachment = match msg.forward_attachment {
        // 转发路径：调用方已校验源消息可读；在目标会话克隆附件行（共享物理文件）
        Some(src) => clone_attachment(tx, src, msg.conversation_id).await?,
        None => load_attachment(tx, msg).await?,
    };
    let (content, kind) = match &msg.poll {
        // 旧客户端 fallback：content 里始终有 '[投票] 问题' 可读文本
        Some(spec) => (format!("[投票] {}", spec.question), "poll"),
        None => normalize_body(msg.content, attachment.as_ref())?,
    };

    let mentions = if kind_name == "group" {
        let members = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT m.user_id, u.username::text FROM memberships m
             JOIN users u ON u.id = m.user_id WHERE m.conversation_id = $1",
        )
        .bind(msg.conversation_id)
        .fetch_all(&mut **tx)
        .await?;
        parse_mentions(&content, &members, msg.sender_id)
    } else {
        Vec::new()
    };

    let seq = sqlx::query_scalar::<_, i64>(
        "UPDATE conversations SET last_seq = last_seq + 1 WHERE id = $1 RETURNING last_seq",
    )
    .bind(msg.conversation_id)
    .fetch_one(&mut **tx)
    .await?;

    let message_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO messages
            (conversation_id, seq, sender_id, content, kind, attachment_id,
             client_msg_id, reply_to_id, forwarded_from, agent_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         RETURNING id",
    )
    .bind(msg.conversation_id)
    .bind(seq)
    .bind(msg.sender_id)
    .bind(&content)
    .bind(kind)
    .bind(attachment.as_ref().map(|a| a.id))
    .bind(client_msg_id)
    .bind(msg.reply_to_id)
    .bind(msg.forwarded_from)
    .bind(msg.agent_id)
    .fetch_one(&mut **tx)
    .await?;

    // 发言即读到这里：自己的消息不算未读（否则刷新/换设备后自己发的消息显示成未读）。
    // Agent 消息例外：sender_id 是主人但作者是 Agent——按「别人发来的消息」计未读。
    if msg.agent_id.is_none() {
        sqlx::query(
            "UPDATE memberships SET last_read_seq = $3
              WHERE conversation_id = $1 AND user_id = $2 AND last_read_seq < $3",
        )
        .bind(msg.conversation_id)
        .bind(msg.sender_id)
        .bind(seq)
        .execute(&mut **tx)
        .await?;
    }

    for uid in &mentions {
        sqlx::query(
            "INSERT INTO message_mentions (message_id, user_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(message_id)
        .bind(uid)
        .execute(&mut **tx)
        .await?;
    }

    // 新的 @本人消息解除其归档（§8.3）；普通消息不解除。同事务完成，提交后广播方才读得到。
    let unarchived = if mentions.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar::<_, Uuid>(
            "UPDATE conversation_preferences
                SET archived_at = NULL,
                    updated_at = GREATEST(clock_timestamp(), updated_at + interval '1 microsecond')
              WHERE conversation_id = $1 AND user_id = ANY($2) AND archived_at IS NOT NULL
              RETURNING user_id",
        )
        .bind(msg.conversation_id)
        .bind(&mentions)
        .fetch_all(&mut **tx)
        .await?
    };

    // 投票明细与消息同事务落库（polls 以 message_id 为 PK，1:1）
    if let Some(spec) = &msg.poll {
        sqlx::query(
            "INSERT INTO polls (message_id, question, allow_multiple, anonymous, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(message_id)
        .bind(&spec.question)
        .bind(spec.allow_multiple)
        .bind(spec.anonymous)
        .bind(spec.expires_at)
        .execute(&mut **tx)
        .await?;
        for (i, label) in spec.options.iter().enumerate() {
            sqlx::query(
                "INSERT INTO poll_options (poll_id, option_index, label) VALUES ($1, $2, $3)",
            )
            .bind(message_id)
            .bind(i as i32)
            .bind(label)
            .execute(&mut **tx)
            .await?;
        }
    }

    Ok(load_posted(
        tx,
        msg.conversation_id,
        msg.sender_id,
        message_id,
        true,
        unarchived,
    )
    .await?)
}

async fn load_attachment(
    tx: &mut Transaction<'_, Postgres>,
    msg: &PostMessage<'_>,
) -> Result<Option<AttachmentView>, WriteError> {
    let Some(att_id) = msg.attachment_id else {
        return Ok(None);
    };
    #[allow(clippy::type_complexity)]
    let row = sqlx::query_as::<
        _,
        (Uuid, String, String, i64, Uuid, Option<i32>, Option<i32>, bool, Option<i32>),
    >(
        "SELECT id, file_name, mime, size, conv_id, width, height, thumb_path IS NOT NULL, duration_ms
           FROM attachments
          WHERE id = $1 AND uploader_id = $2
          FOR UPDATE",
    )
    .bind(att_id)
    .bind(msg.sender_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("not_found", "attachment not found"))?;
    if row.4 != msg.conversation_id {
        return Err(PostError::deny(
            "bad_attachment",
            "attachment belongs to another conversation",
        )
        .into());
    }
    let used = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE attachment_id = $1)",
    )
    .bind(att_id)
    .fetch_one(&mut **tx)
    .await?;
    if used {
        return Err(PostError::deny("bad_attachment", "attachment already sent").into());
    }
    Ok(Some(AttachmentView {
        id: row.0,
        file_name: row.1,
        mime: row.2,
        size: row.3,
        width: row.5,
        height: row.6,
        has_thumb: row.7,
        duration_ms: row.8,
    }))
}

/// 转发专用附件克隆（write_message 事务内调用）：
/// - 在目标会话新 INSERT 一行（新 id），本消息的 attachment_id 指向它——每条消息仍
///   独占一个 attachment 行，「一消息一附件」不变量不破坏；
/// - storage_path/thumb_path 与源共享物理文件——孤儿清理按「还有谁引用该 path」计数，
///   源删除不影响目标、最后一个引用删除时文件才被清；
/// - uploader 保持原上传者（能力 URL 下载不校验归属，只验证行存在）。
/// 只在转发路径可达：attachment_id 的 uploader=发送者 检查拦掉了一切普通借用。
async fn clone_attachment(
    tx: &mut Transaction<'_, Postgres>,
    src_id: Uuid,
    conv: Uuid,
) -> Result<Option<AttachmentView>, WriteError> {
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            i64,
            Option<i32>,
            Option<i32>,
            bool,
            Option<i32>,
        ),
    >(
        "INSERT INTO attachments
            (conv_id, uploader_id, file_name, mime, size, storage_path,
             width, height, thumb_path, duration_ms)
         SELECT $2, uploader_id, file_name, mime, size, storage_path,
                width, height, thumb_path, duration_ms
           FROM attachments WHERE id = $1
         RETURNING id, file_name, mime, size, width, height,
                   thumb_path IS NOT NULL, duration_ms",
    )
    .bind(src_id)
    .bind(conv)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("bad_message", "source attachment gone"))?;
    Ok(Some(AttachmentView {
        id: row.0,
        file_name: row.1,
        mime: row.2,
        size: row.3,
        width: row.4,
        height: row.5,
        has_thumb: row.6,
        duration_ms: row.7,
    }))
}

fn normalize_body(
    raw: &str,
    attachment: Option<&AttachmentView>,
) -> Result<(String, &'static str), PostError> {
    let content = raw.trim();
    if content.chars().count() > 4000 {
        return Err(PostError::deny(
            "bad_message",
            "message too long (max 4000 chars)",
        ));
    }
    let Some(attachment) = attachment else {
        if content.is_empty() {
            return Err(PostError::deny(
                "bad_message",
                "message must be 1-4000 chars",
            ));
        }
        return Ok((content.to_string(), "text"));
    };
    // 有 duration_ms 才是语音；普通音频文件仍走 file，避免把用户上传的歌曲当成语音条。
    let kind = if attachment.duration_ms.is_some() {
        "audio"
    } else if attachment.mime.starts_with("image/") {
        "image"
    } else {
        "file"
    };
    let stored = if content.is_empty() {
        match kind {
            "image" => "[图片]".to_string(),
            "audio" => "[语音]".to_string(),
            _ => format!("[文件] {}", attachment.file_name),
        }
    } else {
        content.to_string()
    };
    Ok((stored, kind))
}

async fn load_posted(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    viewer: Uuid,
    message_id: Uuid,
    inserted: bool,
    unarchived: Vec<Uuid>,
) -> Result<Posted, sqlx::Error> {
    let message = view_by_id(&mut **tx, viewer, message_id).await?;
    let member_ids =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM memberships WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(&mut **tx)
            .await?;
    Ok(Posted {
        message,
        inserted,
        member_ids,
        unarchived,
    })
}

/// 新消息的 reactions 恒为空，view 对任何 viewer 等价；viewer 只影响 reacted_by_me。
async fn view_by_id(
    executor: impl PgExecutor<'_>,
    viewer: Uuid,
    id: Uuid,
) -> Result<MessageView, sqlx::Error> {
    let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(id)
        .fetch_one(executor)
        .await?;
    Ok(row.into_view(viewer))
}

async fn load_by_client_key(
    pool: &PgPool,
    conv: Uuid,
    sender: Uuid,
    client_msg_id: &str,
) -> Result<Posted, PostError> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM messages
          WHERE conversation_id = $1 AND sender_id = $2 AND client_msg_id = $3",
    )
    .bind(conv)
    .bind(sender)
    .bind(client_msg_id)
    .fetch_optional(pool)
    .await
    .map_err(PostError::Db)?
    .ok_or_else(|| PostError::deny("internal", "db error"))?;
    let message = view_by_id(pool, sender, id).await.map_err(PostError::Db)?;
    let member_ids =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM memberships WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(pool)
            .await
            .map_err(PostError::Db)?;
    Ok(Posted {
        message,
        inserted: false,
        member_ids,
        // 幂等回放：归档解除已在首个事务生效，不重复推送偏好帧
        unarchived: Vec::new(),
    })
}

/// DM 可写性：对方仍是好友则 true。非 DM / 空 DM 视为可写（由调用方先判类型）。
async fn dm_writable(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    uid: Uuid,
) -> Result<bool, sqlx::Error> {
    let peer = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM memberships
          WHERE conversation_id = $1 AND user_id <> $2
          LIMIT 1",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(peer) = peer else { return Ok(true) };
    let (lo, hi) = canonical_pair(uid, peer);
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM friendships WHERE user_a = $1 AND user_b = $2)",
    )
    .bind(lo)
    .bind(hi)
    .fetch_one(&mut **tx)
    .await
}

/// 从消息内容提取 @username，匹配群成员（去重、不含发送者）。
fn parse_mentions(content: &str, members: &[(Uuid, String)], sender: Uuid) -> Vec<Uuid> {
    let mut out = Vec::new();
    let b = content.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
                end += 1;
            }
            if end > start {
                let name = &content[start..end];
                if let Some((uid, _)) = members.iter().find(|(_, u)| u == name) {
                    if *uid != sender && !out.contains(uid) {
                        out.push(*uid);
                    }
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

// ---------- 消息生命周期 mutation：编辑 / 软删除 / reaction ----------
//
// 统一锁顺序：会话行锁 → 消息行锁（与 post_message、群管理、后续投票一致）。
// 所有 mutation 在会话行锁内重检成员资格；只读 DM（好友已解除）一律拒写。
// edit_version 是消息级单调变更版本：编辑、软删除、reaction 实际变更均 +1，
// 客户端按 (id, edit_version) 合并变更帧，旧版本不得覆盖新版本。

/// 编辑窗口：发送后 15 分钟（以数据库时间判定，测试可回拨 created_at 覆盖）
const EDIT_WINDOW: &str = "15 minutes";
/// 同一用户对单条消息的不同 emoji 上限
const MAX_REACTION_KINDS: i64 = 20;

struct LockedMessage {
    conversation_id: Uuid,
    conv_type: String,
    my_role: String,
    sender_id: Uuid,
    kind: String,
    deleted_at: Option<DateTime<Utc>>,
    edit_version: i32,
    edit_op_id: Option<String>,
    attachment_id: Option<Uuid>,
    /// Agent A01：非空 = Agent 所写（sender_id 是主人，但主人也不能编辑）
    agent_id: Option<Uuid>,
    edit_expired: bool,
}

/// 定位消息 → 锁会话行 → 校验成员/可写 → 锁消息行。
/// 非成员对不存在与存在但无权的消息统一 403，不探测存在性。
async fn lock_message_for_mutation(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
) -> Result<LockedMessage, WriteError> {
    let conv_id =
        sqlx::query_scalar::<_, Uuid>("SELECT conversation_id FROM messages WHERE id = $1")
            .bind(message_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| PostError::deny("not_found", "message not found"))?;

    let conv = lock_conversation(tx, conv_id)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    let (conv_type, _owner, dissolved) = conv;
    let me = membership(&mut **tx, conv_id, uid)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    if dissolved {
        return Err(PostError::deny("forbidden", "not a member of this conversation").into());
    }
    if conv_type == "dm" && !dm_writable(tx, conv_id, uid).await? {
        return Err(PostError::deny("not_friends", "好友关系已解除，会话只读").into());
    }

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<DateTime<Utc>>,
            i32,
            Option<String>,
            Option<Uuid>,
            Option<Uuid>,
            bool,
        ),
    >(
        "SELECT sender_id, kind, deleted_at, edit_version, edit_op_id, attachment_id,
                agent_id, created_at + $2::interval <= now() AS edit_expired
           FROM messages WHERE id = $1 FOR UPDATE",
    )
    .bind(message_id)
    .bind(EDIT_WINDOW)
    .fetch_one(&mut **tx)
    .await?;

    Ok(LockedMessage {
        conversation_id: conv_id,
        conv_type,
        my_role: me.role,
        sender_id: row.0,
        kind: row.1,
        deleted_at: row.2,
        edit_version: row.3,
        edit_op_id: row.4,
        attachment_id: row.5,
        agent_id: row.6,
        edit_expired: row.7,
    })
}

pub struct Edited {
    pub conversation_id: Uuid,
    /// 变更后的读模型行；广播方须按成员逐一再 into_view
    pub row: MsgViewRow,
    pub member_ids: Vec<Uuid>,
    pub edit_version: i32,
}

/// 编辑：仅发送者、仅普通文本、15 分钟窗口内、expected_version 必须匹配当前版本。
/// 同一 client_op_id 重试返回现状（幂等），不二次递增版本。
pub async fn edit_message(
    pool: &PgPool,
    message_id: Uuid,
    editor: Uuid,
    content: &str,
    expected_version: i32,
    op_id: Option<Uuid>,
) -> Result<Edited, PostError> {
    let content = content.trim();
    if content.is_empty() || content.chars().count() > 4000 {
        return Err(PostError::deny(
            "bad_message",
            "message must be 1-4000 chars",
        ));
    }
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match edit_in_tx(
        &mut tx,
        message_id,
        editor,
        content,
        expected_version,
        op_id,
    )
    .await
    {
        Ok(ed) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(ed)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        // client_msg_id 冲突只发生在发消息路径
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn edit_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    editor: Uuid,
    content: &str,
    expected_version: i32,
    op_id: Option<Uuid>,
) -> Result<Edited, WriteError> {
    let m = lock_message_for_mutation(tx, message_id, editor).await?;
    let op_key = op_id.map(|u| u.to_string());

    // 幂等回放：该 op 已应用过 → 返回现状，不重复递增版本
    if op_key.is_some() && m.edit_op_id.as_deref() == op_key.as_deref() {
        return finish_edit(tx, m.conversation_id, message_id).await;
    }
    // Agent 消息不可编辑：作者栏是 Agent，sender_id 只是承载用的主人 id
    if m.agent_id.is_some() {
        return Err(PostError::deny("bad_request", "agent messages cannot be edited").into());
    }
    if m.sender_id != editor {
        return Err(PostError::deny("forbidden", "only the author can edit").into());
    }
    if m.deleted_at.is_some() {
        return Err(PostError::deny("conflict", "message already deleted").into());
    }
    if m.kind != "text" {
        return Err(PostError::deny("bad_request", "only text messages can be edited").into());
    }
    if m.edit_expired {
        return Err(PostError::deny("bad_request", "edit window expired").into());
    }
    if m.edit_version != expected_version {
        return Err(PostError::deny("conflict", "edit_version conflict").into());
    }

    sqlx::query(
        "UPDATE messages
            SET content = $2, edited_at = now(),
                edit_version = edit_version + 1, edit_op_id = $3
          WHERE id = $1",
    )
    .bind(message_id)
    .bind(content)
    .bind(&op_key)
    .execute(&mut **tx)
    .await?;

    // 群聊重新解析 mentions：提及关系跟随正文（seq 不变，不产生新 unread）
    if m.conv_type == "group" {
        sqlx::query("DELETE FROM message_mentions WHERE message_id = $1")
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
        let members = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT m.user_id, u.username::text FROM memberships m
             JOIN users u ON u.id = m.user_id WHERE m.conversation_id = $1",
        )
        .bind(m.conversation_id)
        .fetch_all(&mut **tx)
        .await?;
        for uid in parse_mentions(content, &members, editor) {
            sqlx::query(
                "INSERT INTO message_mentions (message_id, user_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(message_id)
            .bind(uid)
            .execute(&mut **tx)
            .await?;
        }
    }

    finish_edit(tx, m.conversation_id, message_id).await
}

async fn finish_edit(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    message_id: Uuid,
) -> Result<Edited, WriteError> {
    let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(&mut **tx)
        .await?;
    let edit_version = row.edit_version;
    let member_ids = member_ids(&mut **tx, conv).await?;
    Ok(Edited {
        conversation_id: conv,
        row,
        member_ids,
        edit_version,
    })
}

/// 用户已用附件存储（V2 P2-1）：其上传的附件按 storage_path 去重求和，
/// 转发产生的副本共享同一 storage_path，不重复计。
pub async fn storage_used(pool: &PgPool, uid: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(SUM(size), 0)::bigint
           FROM (SELECT DISTINCT storage_path, size
                   FROM attachments WHERE uploader_id = $1) d",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
}

pub struct Deleted {
    pub conversation_id: Uuid,
    /// 墓碑完整读模型行：广播/响应方按成员逐一再 into_view
    pub row: MsgViewRow,
    pub member_ids: Vec<Uuid>,
    /// 已无其他 attachments 行引用的物理文件（storage_path），提交后由调用方异步删除
    pub orphan_files: Vec<String>,
    /// 这次删除摘掉了置顶行。软删除不删 messages 行，外键不会级联，调用方据此发 pins_updated。
    pub pins_dropped: bool,
}

/// 软删除：本人可删自己的；群主可删群内任意消息；DM 只能删自己的。
/// 重复删除幂等返回现状。seq/last_seq/read cursor 不动；reactions、mentions 随删除清空。
/// 附件行删除使下载立即失效；物理文件只在无其他 attachment 共享同一 storage_path 时清理。
pub async fn delete_message(
    pool: &PgPool,
    message_id: Uuid,
    uid: Uuid,
) -> Result<Deleted, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match delete_in_tx(&mut tx, message_id, uid).await {
        Ok(d) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(d)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn delete_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
) -> Result<Deleted, WriteError> {
    let m = lock_message_for_mutation(tx, message_id, uid).await?;
    let member_ids = member_ids(&mut **tx, m.conversation_id).await?;

    // 幂等：已删再删返回现状（重试/重复帧安全）
    if m.deleted_at.is_some() {
        let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
            .bind(message_id)
            .fetch_one(&mut **tx)
            .await?;
        return Ok(Deleted {
            conversation_id: m.conversation_id,
            row,
            member_ids,
            orphan_files: vec![],
            pins_dropped: false,
        });
    }

    let allowed = m.sender_id == uid || (m.conv_type == "group" && m.my_role == "owner");
    if !allowed {
        return Err(PostError::deny("forbidden", "no permission to delete this message").into());
    }

    // content 物理清空为占位文本（视图同样掩去，双保险；CHECK 要求非空）
    sqlx::query(
        "UPDATE messages
            SET deleted_at = now(), deleted_by = $2, edit_version = edit_version + 1,
                content = '消息已删除'
          WHERE id = $1",
    )
    .bind(message_id)
    .bind(uid)
    .execute(&mut **tx)
    .await?;

    // 墓碑级联：reactions 与由正文派生的 mentions 一并清空
    sqlx::query("DELETE FROM message_reactions WHERE message_id = $1")
        .bind(message_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM message_mentions WHERE message_id = $1")
        .bind(message_id)
        .execute(&mut **tx)
        .await?;
    // 软删除不删 messages 行，外键不会级联。摘掉 pin 行，列表查询再滤一次 deleted_at。
    let dropped = sqlx::query("DELETE FROM conversation_pins WHERE message_id = $1")
        .bind(message_id)
        .execute(&mut **tx)
        .await?;
    let pins_dropped = dropped.rows_affected() > 0;

    // 附件：删行使下载立即 404。历史重复引用可能共享同一 storage_path，
    // 仍有其他有效 attachment 引用时不得删物理文件（转发副本共享 storage_path
    // 与 thumb_path，同一引用计数规则覆盖两者，不会误删）。
    let mut orphan_files = Vec::new();
    if let Some(att) = m.attachment_id {
        let row = sqlx::query_as::<_, (String, Option<String>)>(
            "DELETE FROM attachments WHERE id = $1 RETURNING storage_path, thumb_path",
        )
        .bind(att)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((path, thumb)) = row {
            let still_used = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM attachments WHERE storage_path = $1)",
            )
            .bind(&path)
            .fetch_one(&mut **tx)
            .await?;
            if !still_used {
                orphan_files.push(path);
                // 清理原文件时同名的缩略图一并删（共享 storage_path 时上面已跳过）
                if let Some(t) = thumb {
                    orphan_files.push(t);
                }
            }
        }
    }

    let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Deleted {
        conversation_id: m.conversation_id,
        row,
        member_ids,
        orphan_files,
        pins_dropped,
    })
}

const PIN_CAP: i64 = 20;

/// 置顶结果：无变化（重复置顶 / 取消不存在的行）不广播。
pub struct PinChange {
    pub conversation_id: Uuid,
    pub member_ids: Vec<Uuid>,
    pub changed: bool,
}

/// 置顶本会话未删除的消息。任何成员可操作；只读私聊拒绝。
/// 已置顶再置顶幂等，不改 pinned_at、不广播。
pub async fn pin_message(
    pool: &PgPool,
    conv: Uuid,
    message_id: Uuid,
    uid: Uuid,
) -> Result<PinChange, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    let result = pin_in_tx(&mut tx, conv, message_id, uid).await;
    finish_pin(tx, conv, result).await
}

async fn pin_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    message_id: Uuid,
    uid: Uuid,
) -> Result<bool, WriteError> {
    ensure_pin_writable(tx, conv, uid).await?;
    let msg = sqlx::query_as::<_, (Uuid, Option<DateTime<Utc>>)>(
        "SELECT conversation_id, deleted_at FROM messages WHERE id = $1",
    )
    .bind(message_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("not_found", "message not found"))?;
    if msg.0 != conv {
        return Err(PostError::deny("bad_request", "message is not in this conversation").into());
    }
    if msg.1.is_some() {
        return Err(PostError::deny("bad_request", "已删除的消息不能置顶").into());
    }
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM conversation_pins WHERE conversation_id = $1 AND message_id = $2)",
    )
    .bind(conv)
    .bind(message_id)
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(false);
    }
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM conversation_pins WHERE conversation_id = $1",
    )
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    if n >= PIN_CAP {
        return Err(PostError::deny("bad_request", "最多置顶 20 条").into());
    }
    sqlx::query(
        "INSERT INTO conversation_pins (conversation_id, message_id, pinned_by)
         VALUES ($1, $2, $3)",
    )
    .bind(conv)
    .bind(message_id)
    .bind(uid)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// 取消置顶。行不存在也成功，且不广播。
pub async fn unpin_message(
    pool: &PgPool,
    conv: Uuid,
    message_id: Uuid,
    uid: Uuid,
) -> Result<PinChange, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    let result = unpin_in_tx(&mut tx, conv, message_id, uid).await;
    finish_pin(tx, conv, result).await
}

async fn unpin_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    message_id: Uuid,
    uid: Uuid,
) -> Result<bool, WriteError> {
    ensure_pin_writable(tx, conv, uid).await?;
    let res =
        sqlx::query("DELETE FROM conversation_pins WHERE conversation_id = $1 AND message_id = $2")
            .bind(conv)
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
    Ok(res.rows_affected() > 0)
}

async fn ensure_pin_writable(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    uid: Uuid,
) -> Result<(), WriteError> {
    let Some((kind, _, dissolved)) = lock_conversation(tx, conv).await? else {
        return Err(PostError::deny("forbidden", "not a member of this conversation").into());
    };
    let member = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(conv)
    .bind(uid)
    .fetch_one(&mut **tx)
    .await?;
    if !member || dissolved {
        return Err(PostError::deny("forbidden", "not a member of this conversation").into());
    }
    if kind == "dm" && !dm_writable(tx, conv, uid).await? {
        return Err(PostError::deny("not_friends", "好友关系已解除，会话只读").into());
    }
    Ok(())
}

/// 置顶列表的消息视图：过滤已删除，顺序由调用方按 pinned_at 保持。
pub async fn pinned_message_views(
    pool: &PgPool,
    conv: Uuid,
    viewer: Uuid,
    ids: &[Uuid],
) -> AppResult<Vec<MessageView>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, MsgViewRow>(
        "SELECT * FROM message_views
          WHERE conversation_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
    )
    .bind(conv)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.into_view(viewer)).collect())
}

async fn finish_pin(
    mut tx: Transaction<'_, Postgres>,
    conv: Uuid,
    result: Result<bool, WriteError>,
) -> Result<PinChange, PostError> {
    match result {
        Ok(changed) => {
            let member_ids = member_ids(&mut *tx, conv).await.map_err(PostError::Db)?;
            tx.commit().await.map_err(PostError::Db)?;
            Ok(PinChange {
                conversation_id: conv,
                member_ids,
                changed,
            })
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

pub struct ReactionChange {
    pub conversation_id: Uuid,
    /// 变更后完整读模型行（含 reactions 聚合）；广播方按接收者 into_view 个性化
    pub row: MsgViewRow,
    pub member_ids: Vec<Uuid>,
}

/// reaction 增删：PK 天然幂等；单用户每消息最多 20 种 emoji；墓碑不可再反应。
/// 真实变更时递增消息 edit_version，驱动客户端 (id, version) 合并。
pub async fn set_reaction(
    pool: &PgPool,
    message_id: Uuid,
    uid: Uuid,
    emoji: &str,
    add: bool,
) -> Result<ReactionChange, PostError> {
    if emoji.is_empty() || emoji.len() > 32 {
        return Err(PostError::deny("bad_request", "invalid emoji"));
    }
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match reaction_in_tx(&mut tx, message_id, uid, emoji, add).await {
        Ok(r) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(r)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn reaction_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
    emoji: &str,
    add: bool,
) -> Result<ReactionChange, WriteError> {
    let m = lock_message_for_mutation(tx, message_id, uid).await?;
    if m.deleted_at.is_some() {
        return Err(PostError::deny("conflict", "message already deleted").into());
    }

    let changed = if add {
        let dup = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM message_reactions
              WHERE message_id = $1 AND user_id = $2 AND emoji = $3)",
        )
        .bind(message_id)
        .bind(uid)
        .bind(emoji)
        .fetch_one(&mut **tx)
        .await?;
        if dup {
            false
        } else {
            // 消息行锁串行化并发 reaction：锁内计数，上限判断准确
            let kinds = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM message_reactions
                  WHERE message_id = $1 AND user_id = $2",
            )
            .bind(message_id)
            .bind(uid)
            .fetch_one(&mut **tx)
            .await?;
            if kinds >= MAX_REACTION_KINDS {
                return Err(PostError::deny(
                    "bad_request",
                    "too many reactions on one message (max 20)",
                )
                .into());
            }
            sqlx::query(
                "INSERT INTO message_reactions (message_id, user_id, emoji)
                 VALUES ($1, $2, $3)",
            )
            .bind(message_id)
            .bind(uid)
            .bind(emoji)
            .execute(&mut **tx)
            .await?;
            true
        }
    } else {
        let res = sqlx::query(
            "DELETE FROM message_reactions
              WHERE message_id = $1 AND user_id = $2 AND emoji = $3",
        )
        .bind(message_id)
        .bind(uid)
        .bind(emoji)
        .execute(&mut **tx)
        .await?;
        res.rows_affected() > 0
    };

    // 实际增删时才递增版本；幂等重试保持原版本
    if changed {
        sqlx::query("UPDATE messages SET edit_version = edit_version + 1 WHERE id = $1")
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
    }

    let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(&mut **tx)
        .await?;
    let member_ids = member_ids(&mut **tx, m.conversation_id).await?;

    Ok(ReactionChange {
        conversation_id: m.conversation_id,
        row,
        member_ids,
    })
}

// ---------- 会话偏好：置顶 / 静音 / 归档 ----------
//
// 偏好是用户维度数据，不改变会话本身；仅本人可见、仅同步本人全部设备。
// 会话被解散或本人被移出后不可再改（在会话行锁内重检成员资格与解散标记）。

/// 读取 (conv, uid) 的偏好行；无记录时返回全默认（updated_at = None）。
pub async fn pref_of(
    executor: impl PgExecutor<'_>,
    conv: Uuid,
    uid: Uuid,
) -> Result<ConversationPref, sqlx::Error> {
    let row = sqlx::query_as::<
        _,
        (
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            bool,
            DateTime<Utc>,
        ),
    >(
        "SELECT pinned_at, archived_at, muted_until, mute_forever, updated_at
           FROM conversation_preferences
          WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(executor)
    .await?;
    Ok(row
        .map(
            |(pinned_at, archived_at, muted_until, mute_forever, t)| ConversationPref {
                pinned_at,
                archived_at,
                muted_until,
                mute_forever,
                updated_at: Some(t),
            },
        )
        .unwrap_or_default())
}

/// 静音写入目标（mode 已校验）。
enum MuteSet {
    None,
    Forever,
    Until(DateTime<Utc>),
}

/// PUT preferences：缺省字段不改；mute.until 必须在未来；会话解散/非成员拒绝。
/// 状态覆写天然幂等（重复 PUT 同值收敛）；updated_at 每次写入刷新。
pub async fn set_preferences(
    pool: &PgPool,
    conv: Uuid,
    uid: Uuid,
    req: &PrefUpdateReq,
) -> Result<ConversationPref, PostError> {
    // 先校验 mute 形状再进事务：未知 mode / until 用在非 until 模式 / until 缺失都是 400
    let mute = match &req.mute {
        None => None,
        Some(m) => {
            let set = match m.mode.as_str() {
                "none" => MuteSet::None,
                "forever" => MuteSet::Forever,
                "until" => MuteSet::Until(m.until.ok_or_else(|| {
                    PostError::deny("bad_request", "mute.until is required for mode 'until'")
                })?),
                _ => return Err(PostError::deny("bad_request", "unknown mute mode")),
            };
            if !matches!(set, MuteSet::Until(_)) && m.until.is_some() {
                return Err(PostError::deny(
                    "bad_request",
                    "mute.until is only valid with mode 'until'",
                ));
            }
            Some(set)
        }
    };

    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    let res = set_preferences_in_tx(&mut tx, conv, uid, req, mute).await;
    match res {
        Ok(pref) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(pref)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn set_preferences_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    uid: Uuid,
    req: &PrefUpdateReq,
    mute: Option<MuteSet>,
) -> Result<ConversationPref, WriteError> {
    let conv_row = lock_conversation(tx, conv)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    if conv_row.2 {
        // 已解散（成员已被清空，membership 检查也会拦，但显式拒绝语义更准）
        return Err(PostError::deny("forbidden", "conversation dissolved").into());
    }
    membership(&mut **tx, conv, uid)
        .await?
        .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;

    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let cur = pref_of(&mut **tx, conv, uid).await?;

    let pinned_at = match req.pinned {
        Some(true) => cur.pinned_at.or(Some(now)),
        Some(false) => None,
        None => cur.pinned_at,
    };
    let archived_at = match req.archived {
        Some(true) => Some(now),
        Some(false) => None,
        None => cur.archived_at,
    };
    let (mute_forever, muted_until) = match mute {
        None => (cur.mute_forever, cur.muted_until),
        Some(MuteSet::None) => (false, None),
        Some(MuteSet::Forever) => (true, None),
        Some(MuteSet::Until(t)) => {
            if t <= now {
                return Err(
                    PostError::deny("bad_request", "mute.until must be in the future").into(),
                );
            }
            (false, Some(t))
        }
    };

    sqlx::query(
        "INSERT INTO conversation_preferences
            (conversation_id, user_id, pinned_at, archived_at, mute_forever, muted_until, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp())
         ON CONFLICT (conversation_id, user_id) DO UPDATE SET
            pinned_at = EXCLUDED.pinned_at,
            archived_at = EXCLUDED.archived_at,
            mute_forever = EXCLUDED.mute_forever,
            muted_until = EXCLUDED.muted_until,
            updated_at = GREATEST(clock_timestamp(), conversation_preferences.updated_at + interval '1 microsecond')",
    )
    .bind(conv)
    .bind(uid)
    .bind(pinned_at)
    .bind(archived_at)
    .bind(mute_forever)
    .bind(muted_until)
    .execute(&mut **tx)
    .await?;

    let pref = pref_of(&mut **tx, conv, uid).await?;
    Ok(pref)
}

// ---------- 轻量投票（§3.6/§8.4） ----------
//
// 创建走 post_message 单事务（seq/client_msg_id 幂等一致）；投票/关闭复用
// "会话行锁 → 消息行锁 → poll 行锁"顺序（与生命周期 mutation 一致）。
// 投票是"替换本人选择集合"：空集合 = 撤票；相同集合 = 无变更不升版本。
// 闭票/过期/软删除后拒绝投票；真实变更同时升 polls.version 与 messages.edit_version，
// 广播复用 message_updated 完整视图帧（客户端唯一合并契约）。

/// 投票形状校验：问题 1–200 字符；2–10 个去首尾空白后不同的选项，每项 1–100。
fn validate_poll_draft(
    question: &str,
    options: &[String],
) -> Result<(String, Vec<String>), PostError> {
    let q = question.trim();
    if q.is_empty() || q.chars().count() > 200 {
        return Err(PostError::deny(
            "bad_request",
            "poll question must be 1-200 chars",
        ));
    }
    let mut opts: Vec<String> = Vec::new();
    for o in options {
        let t = o.trim();
        if t.is_empty() || t.chars().count() > 100 {
            return Err(PostError::deny(
                "bad_request",
                "poll option must be 1-100 chars",
            ));
        }
        if !opts.iter().any(|x| x == t) {
            opts.push(t.to_string());
        }
    }
    if !(2..=10).contains(&opts.len()) {
        return Err(PostError::deny(
            "bad_request",
            "poll needs 2-10 distinct options",
        ));
    }
    Ok((q.to_string(), opts))
}

/// 创建投票：仅群聊（write_message 锁内强制）；消息 seq/幂等键与普通发送一致。
/// REST 层把 req.client_msg_id 透传进同一幂等键空间。
pub async fn create_poll(
    pool: &PgPool,
    conv: Uuid,
    uid: Uuid,
    req: &CreatePollReq,
) -> Result<Posted, PostError> {
    let (question, options) = validate_poll_draft(&req.question, &req.options)?;
    post_message(
        pool,
        PostMessage {
            conversation_id: conv,
            sender_id: uid,
            // poll 路径的正文由 write_message 用 '[投票] question' 组装，content 不用
            content: "",
            client_msg_id: req.client_msg_id.as_deref(),
            attachment_id: None,
            reply_to_id: None,
            forwarded_from: None,
            forward_attachment: None,
            agent_id: None,
            poll: Some(PollSpec {
                question,
                options,
                allow_multiple: req.allow_multiple,
                anonymous: req.anonymous,
                expires_at: req.expires_at,
            }),
        },
    )
    .await
}

/// 锁消息（含会话锁/成员校验）后取 poll 行锁，返回 (allow_multiple, closed_at, expires_at)。
/// kind!='poll' 或无 poll 行 → not_found；锁顺序固定：conversation → message → poll。
async fn lock_poll_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
) -> Result<
    (
        LockedMessage,
        bool,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ),
    WriteError,
> {
    let m = lock_message_for_mutation(tx, message_id, uid).await?;
    if m.kind != "poll" {
        return Err(PostError::deny("not_found", "poll not found").into());
    }
    let row = sqlx::query_as::<_, (bool, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>(
        "SELECT allow_multiple, closed_at, expires_at FROM polls WHERE message_id = $1 FOR UPDATE",
    )
    .bind(message_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("not_found", "poll not found"))?;
    Ok((m, row.0, row.1, row.2))
}

/// 读回完整读模型行并组装广播载荷（与编辑同形，走 message_updated 帧）。
async fn finish_poll_change(
    tx: &mut Transaction<'_, Postgres>,
    conv: Uuid,
    message_id: Uuid,
) -> Result<Edited, WriteError> {
    let row = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(&mut **tx)
        .await?;
    let edit_version = row.edit_version;
    let member_ids = member_ids(&mut **tx, conv).await?;
    Ok(Edited {
        conversation_id: conv,
        row,
        member_ids,
        edit_version,
    })
}

/// 投票：替换本人选择集合。单选至多 1 项、多选至多选项数；空集合撤票；
/// 相同集合幂等（不升版本）。闭票/过期/软删除一律拒绝；非成员在锁内 403。
pub async fn vote_poll(
    pool: &PgPool,
    message_id: Uuid,
    uid: Uuid,
    option_ids: &[Uuid],
) -> Result<Edited, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match vote_in_tx(&mut tx, message_id, uid, option_ids).await {
        Ok(ed) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(ed)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn vote_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
    option_ids: &[Uuid],
) -> Result<Edited, WriteError> {
    let (m, allow_multiple, closed_at, expires_at) = lock_poll_in_tx(tx, message_id, uid).await?;
    if m.deleted_at.is_some() {
        return Err(PostError::deny("conflict", "message already deleted").into());
    }
    // 闭票/过期按数据库 now() 判定
    let open = sqlx::query_scalar::<_, bool>(
        "SELECT $1::timestamptz IS NULL AND ($2::timestamptz IS NULL OR $2::timestamptz > now())",
    )
    .bind(closed_at)
    .bind(expires_at)
    .fetch_one(&mut **tx)
    .await?;
    if !open {
        return Err(PostError::deny("conflict", "poll is closed").into());
    }

    // 选项必须全部属于本 poll（应用层先校验可读错误；复合外键兜底归属）
    let mut picked: Vec<Uuid> = Vec::new();
    for id in option_ids {
        if !picked.contains(id) {
            picked.push(*id);
        }
    }
    let valid = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM poll_options WHERE poll_id = $1 AND id = ANY($2)",
    )
    .bind(message_id)
    .bind(&picked)
    .fetch_one(&mut **tx)
    .await?;
    if valid != picked.len() as i64 {
        return Err(PostError::deny("bad_request", "option does not belong to this poll").into());
    }
    if !allow_multiple && picked.len() > 1 {
        return Err(PostError::deny("bad_request", "poll is single choice").into());
    }

    // 与本人当前选择集合比对：相同则无变更（幂等收敛，不升版本）
    let mut old = sqlx::query_scalar::<_, Uuid>(
        "SELECT option_id FROM poll_votes WHERE poll_id = $1 AND user_id = $2",
    )
    .bind(message_id)
    .bind(uid)
    .fetch_all(&mut **tx)
    .await?;
    old.sort();
    let mut new = picked.clone();
    new.sort();
    if old != new {
        sqlx::query("DELETE FROM poll_votes WHERE poll_id = $1 AND user_id = $2")
            .bind(message_id)
            .bind(uid)
            .execute(&mut **tx)
            .await?;
        for oid in &picked {
            sqlx::query("INSERT INTO poll_votes (poll_id, option_id, user_id) VALUES ($1, $2, $3)")
                .bind(message_id)
                .bind(oid)
                .bind(uid)
                .execute(&mut **tx)
                .await?;
        }
        // 真实变更：polls.version 与 messages.edit_version 同步 +1（消息 seq 不动）
        sqlx::query("UPDATE polls SET version = version + 1 WHERE message_id = $1")
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE messages SET edit_version = edit_version + 1 WHERE id = $1")
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
    }

    finish_poll_change(tx, m.conversation_id, message_id).await
}

/// 关闭投票：创建者或群主；重复关票幂等返回现状（不升版本）。
pub async fn close_poll(pool: &PgPool, message_id: Uuid, uid: Uuid) -> Result<Edited, PostError> {
    let mut tx = pool.begin().await.map_err(PostError::Db)?;
    match close_in_tx(&mut tx, message_id, uid).await {
        Ok(ed) => {
            tx.commit().await.map_err(PostError::Db)?;
            Ok(ed)
        }
        Err(WriteError::Fail(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(WriteError::IdempotentHit) => unreachable!(),
    }
}

async fn close_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    uid: Uuid,
) -> Result<Edited, WriteError> {
    let (m, _allow_multiple, closed_at, _expires_at) = lock_poll_in_tx(tx, message_id, uid).await?;
    if m.deleted_at.is_some() {
        return Err(PostError::deny("conflict", "message already deleted").into());
    }
    let allowed = m.sender_id == uid || (m.conv_type == "group" && m.my_role == "owner");
    if !allowed {
        return Err(PostError::deny(
            "forbidden",
            "only the creator or group owner can close the poll",
        )
        .into());
    }
    // 幂等：已关闭再关返回现状
    if closed_at.is_none() {
        sqlx::query(
            "UPDATE polls SET closed_at = now(), version = version + 1 WHERE message_id = $1",
        )
        .bind(message_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query("UPDATE messages SET edit_version = edit_version + 1 WHERE id = $1")
            .bind(message_id)
            .execute(&mut **tx)
            .await?;
    }
    finish_poll_change(tx, m.conversation_id, message_id).await
}

// ---------- 转发 / 收藏（V2 P2-4） ----------

/// 逐目标结果：成功时携带完整 Posted（广播方按成员个性化 + unarchive 推送），
/// 失败带稳定错误码与用户可读原因（forbidden / not_friends / bad_request）。
pub enum Forwarded {
    Ok {
        conv: Uuid,
        posted: Posted,
    },
    Fail {
        conv: Uuid,
        code: &'static str,
        message: &'static str,
    },
}

/// POST /v1/messages/{id}/forward 的领域实现。
///
/// - 源消息：先成员校验（非成员 403），墓碑/投票 400；
///   forwarded_from = 源发送者展示名快照（改名/删除后标签不变）。
/// - 目标逐条独立 post_message：单目标成员/好友校验失败只进该条结果，不拖垮其它；
///   DM 写限制、群成员、self 归 own 校验全部复用 write_message 事务内检查。
/// - client_msg_id = `fwd:{op}:{目标会话id}`：同 op+同目标重试命中
///   uq_messages_client_msg 幂等键返回同一条消息，不重复广播。
/// - 源附件经 forward_attachment 在目标事务内克隆（共享 storage_path，不破坏
///   一消息一附件不变量，不占 uploader 配额）；附件删除语义见 clone_attachment。
pub async fn forward_message(
    pool: &PgPool,
    uid: Uuid,
    message_id: Uuid,
    targets: &[Uuid],
    op: Uuid,
) -> Result<Vec<Forwarded>, PostError> {
    let member = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM memberships mb
             JOIN messages m ON m.conversation_id = mb.conversation_id
              WHERE mb.user_id = $1 AND m.id = $2)",
    )
    .bind(uid)
    .bind(message_id)
    .fetch_one(pool)
    .await
    .map_err(PostError::Db)?;
    if !member {
        return Err(PostError::deny(
            "forbidden",
            "not a member of this conversation",
        ));
    }
    let src = sqlx::query_as::<_, MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_one(pool)
        .await
        .map_err(PostError::Db)?;
    if src.deleted_at.is_some() {
        return Err(PostError::deny("bad_message", "消息已删除"));
    }
    if src.kind == "poll" {
        return Err(PostError::deny("bad_request", "投票不能转发"));
    }

    let mut results = Vec::with_capacity(targets.len());
    for conv in targets {
        // 幂等键含目标 id：同 op 投往不同会话互不撞键；同目标重试收敛同一行
        let key = format!("fwd:{op}:{conv}");
        let posted = post_message(
            pool,
            PostMessage {
                conversation_id: *conv,
                sender_id: uid,
                // 视图里的 content 已含附件占位文本（'[图片]'/'[文件] x'），
                // 重发同名附件时 normalize_body 不会重复拼装。
                content: &src.content,
                client_msg_id: Some(&key),
                attachment_id: None,
                // 转发不保留回复指向（task：strip reply_to）
                reply_to_id: None,
                poll: None,
                forwarded_from: Some(&src.sender_name),
                forward_attachment: src.att_id,
                agent_id: None,
            },
        )
        .await;
        results.push(match posted {
            Ok(posted) => Forwarded::Ok {
                conv: *conv,
                posted,
            },
            Err(PostError::Deny { code, message }) => Forwarded::Fail {
                conv: *conv,
                code,
                message,
            },
            Err(PostError::Db(e)) => return Err(PostError::Db(e)),
        });
    }
    Ok(results)
}
