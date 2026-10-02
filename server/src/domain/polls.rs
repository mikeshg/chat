use super::*;

// ---------- 轻量投票（§3.6/§8.4） ----------
//
// 创建走 post_message 单事务（seq/client_msg_id 幂等一致）；投票/关闭复用
// "会话行锁 → 消息行锁 → poll 行锁"顺序（与生命周期 mutation 一致）。
// 投票是"替换本人选择集合"：空集合 = 撤票；相同集合 = 无变更不升版本。
// 闭票/过期/软删除后拒绝投票；真实变更同时升 polls.version 与 messages.edit_version，
// 广播复用 message_updated 完整视图帧（客户端唯一合并契约）。

/// 投票形状校验：问题 1–200 字符；2–10 个去首尾空白后不同的选项，每项 1–100。
pub(super) fn validate_poll_draft(
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
            client_msg_id: req.client_msg_id.as_deref(),
            poll: Some(PollSpec {
                question,
                options,
                allow_multiple: req.allow_multiple,
                anonymous: req.anonymous,
                expires_at: req.expires_at,
            }),
            // poll 路径的正文由 write_message 用 '[投票] question' 组装，content 不用
            ..PostMessage::text(conv, uid, "")
        },
    )
    .await
}

/// 锁消息（含会话锁/成员校验）后取 poll 行锁，返回 (allow_multiple, closed_at, expires_at)。
/// kind!='poll' 或无 poll 行 → not_found；锁顺序固定：conversation → message → poll。
pub(super) async fn lock_poll_in_tx(
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
pub(super) async fn finish_poll_change(
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

pub(super) async fn vote_in_tx(
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

pub(super) async fn close_in_tx(
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
