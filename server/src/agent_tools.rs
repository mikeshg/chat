// Agent A13：工具注入的服务端半部。
// /v1/agent-tools/* 只认 `Authorization: Invocation <tools_token>`：
// JWT 签名合法 + kind="invocation" + 调用存在且仍在 running/awaiting_approval
// + claims 与库行一致 + 主人未禁用。任一不满足一律 401（不给探测线索）。

use crate::auth;
use crate::domain;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// 校验通过的调用凭据上下文：工具操作被钉死在这一次调用上
pub struct ToolCtx {
    pub invocation: Uuid,
    pub owner: Uuid,
    pub agent: Uuid,
    /// 发起调用的会话（历史工具钉死在这一次；互调还会用到深度和根会话）
    pub conversation: Uuid,
    pub depth: i32,
    pub agent_name: String,
    pub root_conversation: Option<Uuid>,
}

fn tool_unauthorized() -> AppError {
    AppError::unauthorized("invalid invocation credential")
}

/// `Authorization: Invocation <token>` → 调用上下文。所有失败都是 401。
pub async fn require_invocation(st: &SharedState, headers: &HeaderMap) -> AppResult<ToolCtx> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(tool_unauthorized)?;
    let token = value
        .strip_prefix("Invocation ")
        .ok_or_else(tool_unauthorized)?;
    let claims =
        auth::verify_invocation(token, &st.cfg.jwt_secret).map_err(|_| tool_unauthorized())?;

    // 查库：调用仍在进行 + claims 与库行一致 + 主人（Agent 的主人，不是会话 owner）未禁用。
    // 群 / 私聊 / 定时调用的目标会话 owner 往往不是主人，不能拿 conversations.owner_id 当主人。
    // 结束（succeeded/failed/cancelled/timed_out）即失效，无需单独的撤销表。
    let row = sqlx::query_as::<_, InvAuth>(
        "SELECT i.conversation_id, i.agent_id, i.status, a.owner_id, u.disabled_at,
                i.depth, a.name AS agent_name, i.root_conversation_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN users u ON u.id = a.owner_id
          WHERE i.id = $1",
    )
    .bind(claims.sub)
    .fetch_optional(&st.pool)
    .await?;
    let Some(row) = row else {
        return Err(tool_unauthorized());
    };
    if row.disabled_at.is_some()
        || row.owner_id != claims.owner
        || row.agent_id != claims.agent
        || !matches!(row.status.as_str(), "running" | "awaiting_approval")
    {
        return Err(tool_unauthorized());
    }
    Ok(ToolCtx {
        invocation: claims.sub,
        owner: claims.owner,
        agent: row.agent_id,
        conversation: row.conversation_id,
        depth: row.depth,
        agent_name: row.agent_name,
        root_conversation: row.root_conversation_id,
    })
}

#[derive(sqlx::FromRow)]
struct InvAuth {
    conversation_id: Uuid,
    agent_id: Uuid,
    status: String,
    owner_id: Uuid,
    disabled_at: Option<DateTime<Utc>>,
    depth: i32,
    agent_name: String,
    root_conversation_id: Option<Uuid>,
}

/// 工具视角的单条消息：发送者显示名、是否 Agent 消息、seq、时间、正文、附件占位
#[derive(Debug, Serialize)]
pub struct ToolMessage {
    pub seq: i64,
    pub sender_name: String,
    pub is_agent: bool,
    pub created_at: DateTime<Utc>,
    pub content: String,
    /// 附件占位描述（工具拿不到文件本体，只给名字/类型让人读得懂）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
}

impl From<crate::model::MessageView> for ToolMessage {
    fn from(m: crate::model::MessageView) -> Self {
        ToolMessage {
            seq: m.seq,
            sender_name: m.sender_name,
            is_agent: m.agent.is_some(),
            created_at: m.created_at,
            content: m.content,
            attachment: m.attachment.map(|a| {
                if a.mime.is_empty() {
                    a.file_name
                } else {
                    format!("{} ({})", a.file_name, a.mime)
                }
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ToolHistoryPage {
    pub messages: Vec<ToolMessage>,
    pub has_more: bool,
}

#[derive(Deserialize)]
pub struct ToolHistoryQuery {
    pub before_seq: Option<i64>,
    pub limit: Option<i64>,
}

/// 主人视角的一页历史。主人不是成员 → 403（不暴露会话是否存在）。
async fn read_page(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    q: &ToolHistoryQuery,
) -> AppResult<ToolHistoryPage> {
    domain::membership(&st.pool, conv, owner)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let messages = if let Some(before) = q.before_seq {
        domain::messages_before(&st.pool, conv, owner, before, limit).await?
    } else {
        domain::messages_latest(&st.pool, conv, owner, limit).await?
    };
    let has_more =
        messages.len() as i64 == limit && messages.first().map(|m| m.seq > 1).unwrap_or(false);
    Ok(ToolHistoryPage {
        messages: messages.into_iter().map(ToolMessage::from).collect(),
        has_more,
    })
}

/// GET /v1/agent-tools/history：读发起调用的那个会话的历史。
/// 复用普通历史读模型（messages_before/messages_latest），viewer = 主人。
/// 主人必须仍是该会话成员（专属会话只有主人一人，显式校验防结构漂移）。
pub async fn tool_history(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<ToolHistoryQuery>,
) -> AppResult<Json<ToolHistoryPage>> {
    let ctx = require_invocation(&st, &headers).await?;
    Ok(Json(read_page(&st, ctx.conversation, ctx.owner, &q).await?))
}

/// GET /v1/agent-tools/conversations/{id}/messages：读主人参与的某个会话。
pub async fn tool_conversation_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<ToolHistoryQuery>,
) -> AppResult<Json<ToolHistoryPage>> {
    let ctx = require_invocation(&st, &headers).await?;
    Ok(Json(read_page(&st, id, ctx.owner, &q).await?))
}

/// POST /v1/agent-tools/files（Agent A05）：调用产出文件上传，multipart 字段 `file`。
/// 附件记为：上传者=调用主人、会话=发起调用的会话，并打 agent_invocation_id 归属标记——
/// invocation_finish.files 只能引用本次调用上传的附件。
/// 配额/大小上限/图片缩略图与普通上传同一管线；超配额回 413 + quota_exceeded。
#[derive(Deserialize)]
pub struct ToolFileQuery {
    /// 缺省 = 发起这次调用的会话。发到其他会话前必须把附件上传到那个会话。
    conversation_id: Option<Uuid>,
}

pub async fn tool_upload_file(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<ToolFileQuery>,
    mut mp: Multipart,
) -> AppResult<Json<Value>> {
    let ctx = require_invocation(&st, &headers).await?;
    let conv = q.conversation_id.unwrap_or(ctx.conversation);
    if domain::membership(&st.pool, conv, ctx.owner)
        .await?
        .is_none()
    {
        return Err(AppError::forbidden("not a member of this conversation"));
    }
    let (quota_mb, quota_bytes, used_bytes) = crate::api::upload_quota(&st, ctx.owner).await?;
    let rd = crate::api::read_upload(
        &mut mp,
        st.cfg.upload_max_bytes,
        quota_bytes,
        used_bytes,
        &[],
    )
    .await?;
    if rd.too_large {
        return Err(AppError::bad_request("file too large"));
    }
    if rd.over_quota {
        return Err(AppError::payload_too_large(format!(
            "quota_exceeded: {}",
            crate::api::quota_message(used_bytes, quota_mb)
        )));
    }
    if !rd.got_file || rd.buf.is_empty() {
        return Err(AppError::bad_request("missing file"));
    }
    let att = crate::api::store_upload(
        &st,
        ctx.owner,
        conv,
        rd.file_name,
        rd.mime,
        rd.buf,
        None,
        Some(ctx.invocation),
    )
    .await?;
    Ok(Json(json!({"attachment_id": att.id})))
}

// ---------- Agent A14：互调 ----------

#[derive(Deserialize)]
pub struct ToolCallReq {
    agent_name: String,
    prompt: String,
    /// 缺省则新开一个上下文号
    context_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ToolCallRes {
    invocation_id: Uuid,
    context_id: Uuid,
    status: String,
}

#[derive(Deserialize)]
pub struct ToolWaitQuery {
    pub wait: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct ToolInvRes {
    invocation_id: Uuid,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "timed_out")
}

/// POST /v1/agent-tools/invocations
/// 按名字调用主人自己的另一个 Agent。深度超过个人上限 → 400 depth_exceeded。
pub(crate) async fn tool_create_invocation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ToolCallReq>,
) -> AppResult<Json<ToolCallRes>> {
    let ctx = require_invocation(&st, &headers).await?;
    let context_id = match req
        .context_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(s) => Some(Uuid::parse_str(s).map_err(|_| AppError::bad_request("bad_context_id"))?),
    };
    let created = crate::agents::start_child_call(
        &st,
        crate::agents::ChildReq {
            owner: ctx.owner,
            parent_id: ctx.invocation,
            parent_depth: ctx.depth,
            parent_conversation: ctx.conversation,
            parent_root: ctx.root_conversation,
            caller_name: ctx.agent_name,
            agent_name: req.agent_name,
            prompt: req.prompt,
            context_id,
        },
    )
    .await?;
    Ok(Json(ToolCallRes {
        invocation_id: created.invocation_id,
        context_id: created.context_id,
        status: created.status,
    }))
}

/// GET /v1/agent-tools/invocations/{id}?wait=
/// 只能看当前调用派出的子调用。wait 0–600 秒，阻塞到终态或超时。
pub(crate) async fn tool_get_invocation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<ToolWaitQuery>,
) -> AppResult<Json<ToolInvRes>> {
    let wait = q.wait.unwrap_or(0);
    if !(0..=600).contains(&wait) {
        return Err(AppError::bad_request("bad_wait"));
    }
    let ctx = require_invocation(&st, &headers).await?;
    let deadline = Instant::now() + Duration::from_secs(wait as u64);
    loop {
        let row = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT status, result_text, error FROM agent_invocations
              WHERE id = $1 AND parent_invocation_id = $2",
        )
        .bind(id)
        .bind(ctx.invocation)
        .fetch_optional(&st.pool)
        .await?;
        let Some((status, result_text, error)) = row else {
            return Err(AppError::not_found("invocation not found"));
        };
        if terminal_status(&status) || Instant::now() >= deadline {
            return Ok(Json(ToolInvRes {
                invocation_id: id,
                status,
                result_text,
                error,
            }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        // 父调用结束则凭据失效，不要一直占着连接
        require_invocation(&st, &headers).await?;
    }
}
// ---------- Agent A16：搜索 / 读取其他会话 / 以 Agent 身份发消息 ----------

#[derive(sqlx::FromRow)]
struct ToolConvRow {
    id: Uuid,
    kind: String,
    name: Option<String>,
    last_seq: i64,
    member_count: i64,
    peer_id: Option<Uuid>,
    peer_display: Option<String>,
    agent_name: Option<String>,
    agent_deleted: bool,
    is_friend: bool,
}

#[derive(Serialize)]
pub struct ToolConv {
    id: Uuid,
    kind: String,
    title: String,
    member_count: i64,
    last_seq: i64,
    readonly: bool,
}

/// 与客户端 convTitle 同一套：群名 / 私聊对方昵称 / 文件传输助手 / Agent 名。
fn tool_conv_title(row: &ToolConvRow) -> String {
    match row.kind.as_str() {
        "group" => row
            .name
            .clone()
            .unwrap_or_else(|| format!("群聊({})", row.member_count)),
        "self" => "文件传输助手".into(),
        "agent" => row.agent_name.clone().unwrap_or_else(|| "Agent".into()),
        _ => row
            .peer_display
            .clone()
            .or_else(|| row.name.clone())
            .unwrap_or_else(|| "会话".into()),
    }
}

/// 只读私聊（对方已不是好友）和已删除 Agent 的专属会话不能再发言。
fn tool_conv_readonly(row: &ToolConvRow) -> bool {
    match row.kind.as_str() {
        "dm" => row.peer_id.is_some() && !row.is_friend,
        "agent" => row.agent_deleted,
        _ => false,
    }
}

/// only = None 时取主人全部会话里最近活跃的 200 条；Some 时只取这些 id（仍要求是成员）。
async fn load_owner_convs(
    pool: &sqlx::PgPool,
    owner: Uuid,
    only: Option<Vec<Uuid>>,
) -> AppResult<Vec<ToolConvRow>> {
    let all = only.is_none();
    let ids = only.unwrap_or_default();
    let rows = sqlx::query_as::<_, ToolConvRow>(
        "SELECT c.id,
                c.type AS kind,
                c.name,
                c.last_seq,
                (SELECT count(*) FROM memberships mc WHERE mc.conversation_id = c.id) AS member_count,
                p.peer_id,
                p.peer_display,
                ag.name AS agent_name,
                COALESCE(ag.deleted_at IS NOT NULL, false) AS agent_deleted,
                EXISTS (
                    SELECT 1 FROM friendships f
                     WHERE p.peer_id IS NOT NULL
                       AND ((f.user_a = $1 AND f.user_b = p.peer_id)
                         OR (f.user_b = $1 AND f.user_a = p.peer_id))
                ) AS is_friend
           FROM memberships m
           JOIN conversations c ON c.id = m.conversation_id
           LEFT JOIN agents ag ON ag.id = c.agent_id
           LEFT JOIN LATERAL (
                SELECT u.id AS peer_id, u.display_name AS peer_display
                  FROM memberships mm
                  JOIN users u ON u.id = mm.user_id
                 WHERE mm.conversation_id = c.id AND mm.user_id <> $1
                 LIMIT 1
           ) p ON true
          WHERE m.user_id = $1
            AND ($2::bool OR c.id = ANY($3))
          ORDER BY COALESCE(
                     (SELECT max(msg.created_at) FROM messages msg
                       WHERE msg.conversation_id = c.id),
                     c.created_at
                   ) DESC, c.id
          LIMIT 200",
    )
    .bind(owner)
    .bind(all)
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn project_conv(row: ToolConvRow) -> ToolConv {
    let title = tool_conv_title(&row);
    let readonly = tool_conv_readonly(&row);
    ToolConv {
        id: row.id,
        kind: row.kind,
        title,
        member_count: row.member_count,
        last_seq: row.last_seq,
        readonly,
    }
}

/// GET /v1/agent-tools/conversations：主人参与的会话，按最近活跃排序，最多 200。
pub async fn tool_conversations(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<ToolConv>>> {
    let ctx = require_invocation(&st, &headers).await?;
    let rows = load_owner_convs(&st.pool, ctx.owner, None).await?;
    Ok(Json(rows.into_iter().map(project_conv).collect()))
}

#[derive(Deserialize)]
pub struct ToolSearchQuery {
    q: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ToolSearchHit {
    conversation_id: Uuid,
    conversation_title: String,
    seq: i64,
    sender: String,
    is_agent: bool,
    created_at: DateTime<Utc>,
    text: String,
}

/// GET /v1/agent-tools/search：复用全局搜索，范围是主人当前参与的会话。
pub async fn tool_search(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<ToolSearchQuery>,
) -> AppResult<Json<Vec<ToolSearchHit>>> {
    let ctx = require_invocation(&st, &headers).await?;
    let needle = q.q.unwrap_or_default();
    let needle = needle.trim();
    if needle.is_empty() || needle.chars().count() > 100 {
        return Err(AppError::bad_request("q must be 1-100 characters"));
    }
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let (messages, _) = domain::search_global(
        &st.pool,
        ctx.owner,
        &domain::SearchFilter {
            q: Some(needle.to_string()),
            ..Default::default()
        },
        limit,
    )
    .await?;
    let ids: Vec<Uuid> = messages.iter().map(|m| m.conversation_id).collect();
    let titles = if ids.is_empty() {
        HashMap::new()
    } else {
        load_owner_convs(&st.pool, ctx.owner, Some(ids))
            .await?
            .into_iter()
            .map(|row| (row.id, tool_conv_title(&row)))
            .collect::<HashMap<_, _>>()
    };
    let hits = messages
        .into_iter()
        .map(|m| ToolSearchHit {
            conversation_title: titles
                .get(&m.conversation_id)
                .cloned()
                .unwrap_or_else(|| "会话".into()),
            conversation_id: m.conversation_id,
            seq: m.seq,
            sender: m.sender_name,
            is_agent: m.agent.is_some(),
            created_at: m.created_at,
            text: m.content,
        })
        .collect();
    Ok(Json(hits))
}

#[derive(Deserialize)]
pub struct ToolSendReq {
    conversation_id: Uuid,
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachment_ids: Vec<Uuid>,
}

#[derive(sqlx::FromRow)]
struct ToolAttRow {
    id: Uuid,
    conv_id: Uuid,
    agent_invocation_id: Option<Uuid>,
    uploader_id: Uuid,
    used: bool,
}

fn map_post(e: domain::PostError) -> AppError {
    match e {
        domain::PostError::Deny { code, message } => match code {
            "forbidden" | "not_friends" => AppError::forbidden(message),
            "not_found" => AppError::not_found(message),
            "conflict" => AppError::conflict(message),
            _ => AppError::bad_request(message),
        },
        domain::PostError::Db(e) => e.into(),
    }
}

/// 附件必须是本次调用上传、且上传时指定了同一目标会话，还没被发出去。
async fn check_attachments(
    st: &SharedState,
    ctx: &ToolCtx,
    conv: Uuid,
    ids: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(AppError::bad_request("duplicate attachment"));
        }
    }
    let rows = sqlx::query_as::<_, ToolAttRow>(
        "SELECT a.id, a.conv_id, a.agent_invocation_id, a.uploader_id,
                EXISTS(SELECT 1 FROM messages m WHERE m.attachment_id = a.id) AS used
           FROM attachments a
          WHERE a.id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&st.pool)
    .await?;
    let by_id: HashMap<Uuid, ToolAttRow> = rows.into_iter().map(|r| (r.id, r)).collect();
    for id in ids {
        let Some(row) = by_id.get(id) else {
            return Err(AppError::bad_request("attachment not found"));
        };
        if row.uploader_id != ctx.owner || row.agent_invocation_id != Some(ctx.invocation) {
            return Err(AppError::bad_request("attachment not from this invocation"));
        }
        if row.conv_id != conv {
            return Err(AppError::bad_request(
                "attachment belongs to another conversation",
            ));
        }
        if row.used {
            return Err(AppError::bad_request("attachment already sent"));
        }
    }
    Ok(ids.to_vec())
}

async fn post_agent_one(
    st: &SharedState,
    ctx: &ToolCtx,
    conv: Uuid,
    content: &str,
    attachment_id: Option<Uuid>,
) -> AppResult<Uuid> {
    let posted = domain::post_message(
        &st.pool,
        domain::PostMessage {
            conversation_id: conv,
            sender_id: ctx.owner,
            content,
            client_msg_id: None,
            attachment_id,
            reply_to_id: None,
            poll: None,
            forwarded_from: None,
            forward_attachment: None,
            agent_id: Some(ctx.agent),
            agent_steps: None,
            approval: None,
        },
    )
    .await
    .map_err(map_post)?;
    let id = posted.message.id;
    crate::ws::broadcast_posted(st, conv, &posted).await;
    Ok(id)
}

/// POST /v1/agent-tools/messages：以当前调用的 Agent 身份发到主人参与的会话。
/// 只读私聊沿用 post_message 的拒绝。多个附件时第一条带上正文，其余各成一条文件消息；
/// 响应里的 message_id 是第一条。
pub async fn tool_send_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ToolSendReq>,
) -> AppResult<Json<Value>> {
    let ctx = require_invocation(&st, &headers).await?;
    let ids = check_attachments(&st, &ctx, req.conversation_id, &req.attachment_ids).await?;
    if req.text.trim().is_empty() && ids.is_empty() {
        return Err(AppError::bad_request("message must be 1-4000 chars"));
    }
    let first = if ids.is_empty() {
        post_agent_one(&st, &ctx, req.conversation_id, &req.text, None).await?
    } else {
        let mut first = None;
        for (i, att) in ids.iter().enumerate() {
            let content = if i == 0 { req.text.as_str() } else { "" };
            let id = post_agent_one(&st, &ctx, req.conversation_id, content, Some(*att)).await?;
            if first.is_none() {
                first = Some(id);
            }
        }
        first.expect("ids 非空")
    };
    Ok(Json(json!({"message_id": first})))
}
