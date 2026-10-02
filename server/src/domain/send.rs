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
    /// Agent A01：Some(agent) 表示本条由该 Agent 发出（sender_id 仍是主人）。
    /// 仅服务端宿主回写路径可达。A11：只能写进主人仍是成员的会话。
    pub agent_id: Option<Uuid>,
    /// Agent A03：随消息落库的已完成步骤（jsonb 数组）；仅 Agent 回写路径用。
    /// 仅当 agent_id 非空时生效，普通消息路径恒 None。
    pub agent_steps: Option<serde_json::Value>,
    /// Agent A12：Some 时本条消息 kind='approval'，同事务写入 agent_approvals。
    pub approval: Option<ApprovalDraft<'a>>,
}

impl<'a> PostMessage<'a> {
    /// 普通发送：投票、转发、Agent、审批字段全为空。特殊路径在此基础上改字段，
    /// 如 `PostMessage { agent_id: Some(a), ..PostMessage::text(conv, owner, body) }`。
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
            agent_id: None,
            agent_steps: None,
            approval: None,
        }
    }
}

/// 审批卡片随消息一起落库的种子。request / options 由调用方整形。
pub struct ApprovalDraft<'a> {
    pub invocation_id: Uuid,
    pub request: &'a serde_json::Value,
    pub options: &'a serde_json::Value,
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
    /// A11：本条 WS 显式选中的 Agent。post_message 留空，send_message 在广播前填上。
    pub agent_targets: Vec<Uuid>,
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

/// 在调用方已经开始的事务里写消息，不提交。
/// 审批卡片必须和调用行锁落在同一个事务里，不能走自己开事务的 `post_message`。
pub async fn post_message_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    msg: PostMessage<'_>,
) -> Result<Posted, PostError> {
    let client_msg_id = normalize_client_msg_id(msg.client_msg_id)?;
    match write_message(tx, &msg, client_msg_id.as_deref()).await {
        Ok(posted) => Ok(posted),
        // 语句失败后事务已中止，调用方回滚。审批不带 client_msg_id，走不到幂等分支。
        Err(WriteError::IdempotentHit) => Err(PostError::deny("internal", "db error")),
        Err(WriteError::Fail(e)) => Err(e),
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

    // Agent 消息只能写进主人仍是成员的会话（专属会话、私聊、群）。
    // sender_id 必须是该 Agent 的主人。已删除 Agent 的专属会话在上面已拒绝。
    if let Some(aid) = msg.agent_id {
        let allowed = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM agents a
                JOIN memberships m
                  ON m.conversation_id = $2 AND m.user_id = a.owner_id
               WHERE a.id = $1 AND a.owner_id = $3)",
        )
        .bind(aid)
        .bind(msg.conversation_id)
        .bind(msg.sender_id)
        .fetch_one(&mut **tx)
        .await?;
        if !allowed {
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
    let (content, kind) = if msg.approval.is_some() {
        let content = msg.content.trim();
        if content.is_empty() || content.chars().count() > 4000 {
            return Err(PostError::deny("bad_message", "message must be 1-4000 chars").into());
        }
        (content.to_string(), "approval")
    } else {
        match &msg.poll {
            // 旧客户端 fallback：content 里始终有 '[投票] 问题' 可读文本
            Some(spec) => (format!("[投票] {}", spec.question), "poll"),
            None => normalize_body(msg.content, attachment.as_ref())?,
        }
    };

    // Agent 消息不解析 @。手打 @token 若命中发送者自己仍在用的 Agent，也不算成员提及。
    let mentions = if kind_name == "group" && msg.agent_id.is_none() {
        let members = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT m.user_id, u.username::text FROM memberships m
             JOIN users u ON u.id = m.user_id WHERE m.conversation_id = $1",
        )
        .bind(msg.conversation_id)
        .fetch_all(&mut **tx)
        .await?;
        let agent_names = live_agent_names(tx, msg.sender_id).await?;
        parse_mentions(&content, &members, msg.sender_id, &agent_names)
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
             client_msg_id, reply_to_id, forwarded_from, agent_id, agent_steps)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                 CASE WHEN $10 IS NULL THEN NULL ELSE $11 END)
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
    .bind(&msg.agent_steps)
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

    if let Some(ap) = &msg.approval {
        sqlx::query(
            "INSERT INTO agent_approvals (invocation_id, message_id, request, options, status)
             VALUES ($1, $2, $3, $4, 'pending')",
        )
        .bind(ap.invocation_id)
        .bind(message_id)
        .bind(ap.request)
        .bind(ap.options)
        .execute(&mut **tx)
        .await?;
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
        agent_targets: Vec::new(),
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
        agent_targets: Vec::new(),
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

/// 正文里的一个 @token。`name` 可含连字符（Agent 名）；`member_name` 在 `-` 处截断（用户名规则）。
pub(crate) struct AtToken {
    pub name: String,
    pub member_name: String,
    /// `@` 的字节下标
    pub start: usize,
    /// token 结束后的字节下标（不含 `@` 后的空格）
    pub end: usize,
}

/// 扫出全部 @token。`@` 前不要求空白，与原提及解析一致。
pub(crate) fn scan_at_tokens(content: &str) -> Vec<AtToken> {
    let b = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            let start_name = i + 1;
            if start_name < b.len()
                && (b[start_name].is_ascii_alphanumeric() || b[start_name] == b'_')
            {
                let mut end = start_name + 1;
                while end < b.len()
                    && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'-')
                {
                    end += 1;
                }
                let name = content[start_name..end].to_string();
                let cut = name
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(name.len());
                let member_name = name[..cut].to_string();
                out.push(AtToken {
                    name,
                    member_name,
                    start: i,
                    end,
                });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub(super) async fn live_agent_names(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM agents WHERE owner_id = $1 AND deleted_at IS NULL")
        .bind(owner)
        .fetch_all(&mut **tx)
        .await
}

/// 从消息内容提取 @username，匹配群成员（去重、不含发送者）。
/// `agent_names` 里的 token（不区分大小写，含连字符）先被当成发送者自己的 Agent，不再提及同名成员。
pub(super) fn parse_mentions(
    content: &str,
    members: &[(Uuid, String)],
    sender: Uuid,
    agent_names: &[String],
) -> Vec<Uuid> {
    let mut out = Vec::new();
    for tok in scan_at_tokens(content) {
        if agent_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&tok.name))
        {
            continue;
        }
        if tok.member_name.is_empty() {
            continue;
        }
        if let Some((uid, _)) = members.iter().find(|(_, u)| u == &tok.member_name) {
            if *uid != sender && !out.contains(uid) {
                out.push(*uid);
            }
        }
    }
    out
}
