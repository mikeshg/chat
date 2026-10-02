use super::*;

// ---------- 消息生命周期 mutation：编辑 / 软删除 / reaction ----------
//
// 统一锁顺序：会话行锁 → 消息行锁（与 post_message、群管理、后续投票一致）。
// 所有 mutation 在会话行锁内重检成员资格；只读 DM（好友已解除）一律拒写。
// edit_version 是消息级单调变更版本：编辑、软删除、reaction 实际变更均 +1，
// 客户端按 (id, edit_version) 合并变更帧，旧版本不得覆盖新版本。

/// 编辑窗口：发送后 15 分钟（以数据库时间判定，测试可回拨 created_at 覆盖）
pub(super) const EDIT_WINDOW: &str = "15 minutes";
/// 同一用户对单条消息的不同 emoji 上限
pub(super) const MAX_REACTION_KINDS: i64 = 20;

pub(super) struct LockedMessage {
    pub(super) conversation_id: Uuid,
    pub(super) conv_type: String,
    pub(super) my_role: String,
    pub(super) sender_id: Uuid,
    pub(super) kind: String,
    pub(super) deleted_at: Option<DateTime<Utc>>,
    pub(super) edit_version: i32,
    pub(super) edit_op_id: Option<String>,
    pub(super) attachment_id: Option<Uuid>,
    pub(super) edit_expired: bool,
}

/// 定位消息 → 锁会话行 → 校验成员/可写 → 锁消息行。
/// 非成员对不存在与存在但无权的消息统一 403，不探测存在性。
pub(super) async fn lock_message_for_mutation(
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
            bool,
        ),
    >(
        "SELECT sender_id, kind, deleted_at, edit_version, edit_op_id, attachment_id,
                created_at + $2::interval <= now() AS edit_expired
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
        edit_expired: row.6,
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

pub(super) async fn edit_in_tx(
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

pub(super) async fn finish_edit(
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

pub(super) async fn delete_in_tx(
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

pub(super) const PIN_CAP: i64 = 20;

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

pub(super) async fn pin_in_tx(
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

pub(super) async fn unpin_in_tx(
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

pub(super) async fn ensure_pin_writable(
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

pub(super) async fn finish_pin(
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

pub(super) async fn reaction_in_tx(
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
