use super::*;

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
        return Err(PostError::deny("bad_message", "消息已撤回"));
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
            // 视图里的 content 已含附件占位文本（'[图片]'/'[文件] x'），
            // 重发同名附件时 normalize_body 不会重复拼装。
            // 转发不保留回复指向（task：strip reply_to）
            PostMessage {
                client_msg_id: Some(&key),
                forwarded_from: Some(&src.sender_name),
                forward_attachment: src.att_id,
                ..PostMessage::text(*conv, uid, &src.content)
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
