use super::*;

#[derive(Deserialize)]
pub(super) struct MsgQuery {
    after_seq: Option<i64>,
    before_seq: Option<i64>,
    limit: Option<i64>,
}

pub(super) async fn list_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<MsgQuery>,
) -> AppResult<Json<MessagesPage>> {
    let uid = uid_of(&headers, &st).await?;
    require_member(&st, conv, uid).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let messages = if let Some(after) = q.after_seq {
        domain::messages_after(&st.pool, conv, uid, after, limit).await?
    } else if let Some(before) = q.before_seq {
        domain::messages_before(&st.pool, conv, uid, before, limit).await?
    } else {
        domain::messages_latest(&st.pool, conv, uid, limit).await?
    };

    let has_more = if q.after_seq.is_some() {
        messages.len() as i64 == limit
    } else {
        messages.len() as i64 == limit && messages.first().map(|m| m.seq > 1).unwrap_or(false)
    };
    Ok(Json(MessagesPage { messages, has_more }))
}

#[derive(Deserialize)]
pub(super) struct MsgSearchQuery {
    q: String,
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct GlobalSearchQuery {
    q: Option<String>,
    conversation_id: Option<Uuid>,
    sender_id: Option<Uuid>,
    kind: Option<String>,
    /// RFC3339；含 from、不含 to
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// 翻页游标：上一页末条的 created_at + id（同毫秒时 id 兜底）
    before: Option<DateTime<Utc>>,
    before_id: Option<Uuid>,
    limit: Option<i64>,
}

/// 全局消息搜索（V2 P3-1）：只在当前是成员的会话里搜未删除消息。
/// q 或至少一个筛选条件必填；conversation_id 定向搜索要求成员身份。
pub(super) async fn global_search(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<GlobalSearchQuery>,
) -> AppResult<Json<MessagesPage>> {
    let uid = uid_of(&headers, &st).await?;
    let needle = q.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(n) = &needle {
        if n.chars().count() > 100 {
            return Err(AppError::bad_request("q must be 1-100 characters"));
        }
    }
    if let Some(k) = &q.kind {
        if !matches!(k.as_str(), "text" | "image" | "file" | "audio") {
            return Err(AppError::bad_request("kind must be text/image/file/audio"));
        }
    }
    if needle.is_none()
        && q.conversation_id.is_none()
        && q.sender_id.is_none()
        && q.kind.is_none()
        && q.from.is_none()
        && q.to.is_none()
    {
        return Err(AppError::bad_request(
            "q or at least one filter is required",
        ));
    }
    if let Some(conv) = q.conversation_id {
        require_member(&st, conv, uid).await?;
    }
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let (messages, has_more) = domain::search_global(
        &st.pool,
        uid,
        &domain::SearchFilter {
            q: needle,
            conversation_id: q.conversation_id,
            sender_id: q.sender_id,
            kind: q.kind,
            from: q.from,
            to: q.to,
            before: q.before,
            before_id: q.before_id,
        },
        limit,
    )
    .await?;
    Ok(Json(MessagesPage { messages, has_more }))
}

/// 会话内消息搜索：content ILIKE 子串匹配（转义通配符），按 seq 倒序
pub(super) async fn search_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<MsgSearchQuery>,
) -> AppResult<Json<Vec<MessageView>>> {
    let uid = uid_of(&headers, &st).await?;
    require_member(&st, conv, uid).await?;
    let needle = q.q.trim();
    if needle.is_empty() {
        return Ok(Json(vec![]));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    Ok(Json(
        domain::search_messages(&st.pool, conv, uid, needle, limit).await?,
    ))
}

// ---------- 消息生命周期 REST ----------

/// 重连批量刷新：按 id 回读最新状态（正文/墓碑/编辑/reaction/回复预览）。
/// 未授权会话直接 403，客户端据此丢弃本地缓存。每批上限 100。
pub(super) async fn refresh_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<RefreshMessagesReq>,
) -> AppResult<Json<MessagesPage>> {
    let uid = uid_of(&headers, &st).await?;
    if req.ids.len() > 100 {
        return Err(AppError::bad_request("too many ids (max 100)"));
    }
    require_member(&st, conv, uid).await?;
    let messages = domain::messages_by_ids(&st.pool, conv, uid, &req.ids).await?;
    Ok(Json(MessagesPage {
        messages,
        has_more: false,
    }))
}

/// 编辑消息：仅发送者、文本、15 分钟窗口、expected_version 乐观并发。
/// 成功后按成员个性化广播 message_updated；REST 返回发起者视角视图。
pub(super) async fn edit_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<EditMessageReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::edit_message(
        &st.pool,
        message_id,
        uid,
        &req.content,
        req.expected_version,
        req.client_op_id,
    )
    .await
    .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

/// 撤回消息（原软删除通道改名，语义收紧）：
/// POST /messages/{id}/recall 与旧 DELETE /messages/{id} 共用本处理——旧客户端无感。
/// 权限在领域层锁内判定：发送者限 24h（超时 recall_expired）、群主可撤群内他人消息；
/// 重复撤回幂等。client_op_id 仅作契约参数（撤回本身已状态幂等）。
/// 返回发起者视角的完整墓碑视图（与 message_deleted 广播同形），发起端立即合并；
/// 仅当发起者是被撤附件的上传者时，响应多带 recalled_attachment_id（取回/重发用）。
pub(super) async fn recall_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let d = domain::recall_message(
        &st.pool,
        message_id,
        uid,
        st.cfg.recall_attachment_ttl_secs,
    )
    .await
    .map_err(post_err)?;
    ws::broadcast_message_deleted(&st, &d);
    if d.pins_dropped {
        notify_pins(
            &st,
            &domain::PinChange {
                conversation_id: d.conversation_id,
                member_ids: d.member_ids.clone(),
                changed: true,
            },
        );
    }
    // 附件不删：置 recalled_at 隐藏公开下载，物理文件由后台清扫按引用计数处理。
    let mut view = d.row.into_view(uid);
    view.recalled_attachment_id = d.recalled_attachment_id;
    Ok(Json(view))
}

pub(super) async fn reaction_write(
    st: &SharedState,
    headers: &HeaderMap,
    message_id: Uuid,
    emoji: &str,
    add: bool,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(headers, st).await?;
    let ch = domain::set_reaction(&st.pool, message_id, uid, emoji, add)
        .await
        .map_err(post_err)?;
    ws::broadcast_reaction_update(st, &ch);
    // REST 响应为发起者视角的完整视图（客户端按 id+version 整视图合并）
    Ok(Json(ch.row.into_view(uid)))
}

pub(super) async fn reaction_add(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((message_id, emoji)): Path<(Uuid, String)>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    reaction_write(&st, &headers, message_id, &emoji, true).await
}

pub(super) async fn reaction_remove(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((message_id, emoji)): Path<(Uuid, String)>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    reaction_write(&st, &headers, message_id, &emoji, false).await
}

// ---------- 轻量投票 REST ----------
// 投票 mutation 只走 REST；同步复用现有 WS 广播（创建→message 帧，
// 投票/关闭→message_updated 完整个性化视图），不新增 WS mutation/事件类型。

/// 创建投票（仅群聊成员）：复用 post_message 单事务（seq + client_msg_id 幂等），
/// 成功即按普通消息广播 message 帧（视图已含 poll 卡片数据），响应为发起者视角视图。
pub(super) async fn create_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<CreatePollReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let posted = domain::create_poll(&st.pool, conv, uid, &req)
        .await
        .map_err(post_err)?;
    ws::broadcast_posted(&st, conv, &posted).await;
    Ok(Json(posted.message))
}

/// 投票：PUT 替换本人选择集合（空数组撤票）；广播 message_updated 逐成员个性化视图。
pub(super) async fn vote_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<VotePollReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::vote_poll(&st.pool, message_id, uid, &req.option_ids)
        .await
        .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

/// 关闭投票：创建者或群主；重复关票幂等。client_op_id 仅作契约参数。
pub(super) async fn close_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::close_poll(&st.pool, message_id, uid)
        .await
        .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

// ---------- 转发 / 收藏（V2 P2-4） ----------

/// POST /messages/{id}/forward：复制一条消息到至多 10 个目标会话。
/// 收藏 = 客户端把目标填本人的文件传输助手。
/// 源侧统一拒绝（非成员 403 / 墓碑或投票 400 / 目标列表形状 400）；目标侧逐条
/// 独立成败：HTTP 恒 200，results 与去重后的请求目标顺序一一对应。
/// 每条成功目标走与 WS 发送相同的 message 帧广播（含 unarchive 偏好推送）。
pub(super) async fn forward_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<ForwardReq>,
) -> AppResult<Json<ForwardResp>> {
    let uid = uid_of(&headers, &st).await?;
    // 整个请求按 uid 限流（与注册/登录同一滑动窗口参数）
    rate_limit(&st, format!("fwd:{uid}"))?;
    if req.conversation_ids.is_empty() || req.conversation_ids.len() > 10 {
        return Err(AppError::bad_request(
            "conversation_ids must be 1-10 targets",
        ));
    }
    // 去重保持首见顺序（results 按此顺序逐条对应）
    let mut seen = std::collections::HashSet::new();
    let targets: Vec<Uuid> = req
        .conversation_ids
        .iter()
        .copied()
        .filter(|c| seen.insert(*c))
        .collect();

    let results = domain::forward_message(&st.pool, uid, message_id, &targets, req.client_op_id)
        .await
        .map_err(post_err)?;
    let mut items = Vec::with_capacity(results.len());
    for r in results {
        match r {
            domain::Forwarded::Ok { conv, posted } => {
                ws::broadcast_posted(&st, conv, &posted).await;
                items.push(ForwardResultItem {
                    conversation_id: conv,
                    ok: true,
                    message: Some(posted.message),
                    error: None,
                    message_text: None,
                });
            }
            domain::Forwarded::Fail {
                conv,
                code,
                message,
            } => items.push(ForwardResultItem {
                conversation_id: conv,
                ok: false,
                message: None,
                error: Some(code.to_string()),
                message_text: Some(message.to_string()),
            }),
        }
    }
    Ok(Json(ForwardResp { results: items }))
}
