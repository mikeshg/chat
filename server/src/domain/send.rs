use super::*;

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
    /// 撤回附件重发（reuse_attachment_id）：引用发送者自己已撤回消息上的附件行，
    /// 事务内校验归属/撤回期/会话后克隆出新行（与 attachment_id 互斥）。
    pub reuse_attachment_id: Option<Uuid>,
    /// 撤回附件保留期（秒）：reuse 校验用它判定 recalled_at 是否过期。
    /// WS 发送路径从 Config 注入；构造器默认取 config 模块的公共缺省。
    pub recall_attachment_ttl_secs: u64,
}

impl<'a> PostMessage<'a> {
    /// 普通发送：投票、转发字段全为空。特殊路径在此基础上改字段，
    /// 如 `PostMessage { reply_to_id: Some(r), ..PostMessage::text(conv, uid, body) }`。
    pub fn text(conversation_id: Uuid, sender_id: Uuid, content: &'a str) -> Self {
        Self {
            conversation_id,
            sender_id,
            content,
            client_msg_id: None,
            attachment_id: None,
            reply_to_id: None,
            poll: None,
            forwarded_from: None,
            forward_attachment: None,
            reuse_attachment_id: None,
            recall_attachment_ttl_secs: crate::config::DEFAULT_RECALL_ATTACHMENT_TTL_SECS,
        }
    }
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

pub(super) fn normalize_client_msg_id(raw: Option<&str>) -> Result<Option<String>, PostError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if raw.chars().count() > 80 {
        return Err(PostError::deny("bad_message", "client_msg_id too long"));
    }
    Ok(Some(raw.to_string()))
}

pub(super) async fn write_message(
    tx: &mut Transaction<'_, Postgres>,
    msg: &PostMessage<'_>,
    client_msg_id: Option<&str>,
) -> Result<Posted, WriteError> {
    let conv = sqlx::query_as::<_, (String, bool)>(
        "SELECT type, dissolved_at IS NOT NULL
           FROM conversations WHERE id = $1 FOR UPDATE",
    )
    .bind(msg.conversation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("forbidden", "not a member of this conversation"))?;
    let (kind_name, dissolved) = conv;

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
        if msg.attachment_id.is_some() || msg.reuse_attachment_id.is_some() {
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

    if msg.attachment_id.is_some() && msg.reuse_attachment_id.is_some() {
        return Err(PostError::deny(
            "bad_request",
            "attachment_id and reuse_attachment_id are mutually exclusive",
        )
        .into());
    }
    let attachment = match msg.forward_attachment {
        // 转发路径：调用方已校验源消息可读；在目标会话克隆附件行（共享物理文件）
        Some(src) => clone_attachment(tx, src, msg.conversation_id).await?,
        None => match msg.reuse_attachment_id {
            // 撤回附件重发：锁内校验归属/保留期/会话后克隆新行
            Some(src) => {
                reuse_recalled_attachment(
                    tx,
                    src,
                    msg.sender_id,
                    msg.conversation_id,
                    msg.recall_attachment_ttl_secs,
                )
                .await?
            }
            None => load_attachment(tx, msg).await?,
        },
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
             client_msg_id, reply_to_id, forwarded_from)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
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
    .fetch_one(&mut **tx)
    .await?;

    // 发言即读到这里：自己的消息不算未读（否则刷新/换设备后自己发的消息显示成未读）。
    sqlx::query(
        "UPDATE memberships SET last_read_seq = $3
          WHERE conversation_id = $1 AND user_id = $2 AND last_read_seq < $3",
    )
    .bind(msg.conversation_id)
    .bind(msg.sender_id)
    .bind(seq)
    .execute(&mut **tx)
    .await?;

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

pub(super) async fn load_attachment(
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
          WHERE id = $1 AND uploader_id = $2 AND recalled_at IS NULL
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

/// 撤回附件重发复用（write_message 事务内调用，client_msg_id 幂等判定之后）：
/// - FOR SHARE 锁源行后复查：行不存在/已被清扫 → attachment_expired；
///   uploader ≠ 发送者 → forbidden；recalled_at 为空或已过期 → attachment_expired；
/// - 目标会话由 reuse_conv_allowed 判定（当前仅同源会话），通过后 clone_attachment
///   生成新行（recalled_at NULL，不再进清扫集）；TTL 内同一源可重复利用多次。
///
/// 新行 kind/宽高/时长全部来自源行，客户端传的值不采信。
pub(super) async fn reuse_recalled_attachment(
    tx: &mut Transaction<'_, Postgres>,
    src_id: Uuid,
    sender: Uuid,
    conv: Uuid,
    ttl_secs: u64,
) -> Result<Option<AttachmentView>, WriteError> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, Option<DateTime<Utc>>, bool)>(
        "SELECT uploader_id, conv_id, recalled_at,
                recalled_at IS NOT NULL
                  AND recalled_at + make_interval(secs => $2) > now() AS fresh
           FROM attachments WHERE id = $1
          FOR SHARE",
    )
    .bind(src_id)
    .bind(ttl_secs as f64)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| PostError::deny("attachment_expired", "附件已过期或不存在"))?;
    let (uploader, src_conv, recalled_at, fresh) = row;
    if uploader != sender {
        return Err(PostError::deny("forbidden", "无权使用该附件").into());
    }
    if recalled_at.is_none() || !fresh {
        return Err(PostError::deny("attachment_expired", "附件已过期或不存在").into());
    }
    if !reuse_conv_allowed(src_conv, conv) {
        return Err(PostError::deny("forbidden", "无权在该会话使用该附件").into());
    }
    clone_attachment(tx, src_id, conv).await
}

/// 撤回附件允许重发到哪个会话：当前仅允许原会话（src_conv == 目标）。
/// 有意收成独立小函数：以后要放宽（如同用户任意会话）只改这里。
fn reuse_conv_allowed(src_conv: Uuid, target_conv: Uuid) -> bool {
    src_conv == target_conv
}

/// 转发专用附件克隆（write_message 事务内调用）：
/// - 在目标会话新 INSERT 一行（新 id），本消息的 attachment_id 指向它——每条消息仍
///   独占一个 attachment 行，「一消息一附件」不变量不破坏；
/// - storage_path/thumb_path 与源共享物理文件——孤儿清理按「还有谁引用该 path」计数，
///   源删除不影响目标、最后一个引用删除时文件才被清；
/// - uploader 保持原上传者（能力 URL 下载不校验归属，只验证行存在）。
/// 只在转发路径可达：attachment_id 的 uploader=发送者 检查拦掉了一切普通借用。
pub(super) async fn clone_attachment(
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

pub(super) fn normalize_body(
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

pub(super) async fn load_posted(
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
pub(super) async fn view_by_id(
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

pub(super) async fn load_by_client_key(
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
pub(super) async fn dm_writable(
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
/// `@` 前不要求空白；用户名只含字母数字下划线，遇到其他字符截断。
pub(super) fn parse_mentions(content: &str, members: &[(Uuid, String)], sender: Uuid) -> Vec<Uuid> {
    let mut out = Vec::new();
    for tok in content.split('@').skip(1) {
        let end = tok
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tok.len());
        if let Some((uid, _)) = members.iter().find(|(_, u)| u == &tok[..end]) {
            if *uid != sender && !out.contains(uid) {
                out.push(*uid);
            }
        }
    }
    out
}
