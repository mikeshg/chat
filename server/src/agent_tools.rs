// Agent A13：工具注入的服务端半部。
// /v1/agent-tools/* 只认 `Authorization: Invocation <tools_token>`：
// JWT 签名合法 + kind="invocation" + 调用存在且仍在 running/awaiting_approval
// + claims 与库行一致 + 主人未禁用。任一不满足一律 401（不给探测线索）。

use crate::auth;
use crate::domain;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// 校验通过的调用凭据上下文：工具操作被钉死在这一次调用上
pub struct ToolCtx {
    #[allow(dead_code)]
    pub invocation: Uuid,
    pub owner: Uuid,
    #[allow(dead_code)]
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

/// GET /v1/agent-tools/history：读发起调用的那个会话的历史。
/// 复用普通历史读模型（messages_before/messages_latest），viewer = 主人。
/// 主人必须仍是该会话成员（专属会话只有主人一人，显式校验防结构漂移）。
pub async fn tool_history(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<ToolHistoryQuery>,
) -> AppResult<Json<ToolHistoryPage>> {
    let ctx = require_invocation(&st, &headers).await?;
    domain::membership(&st.pool, ctx.conversation, ctx.owner)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let messages = if let Some(before) = q.before_seq {
        domain::messages_before(&st.pool, ctx.conversation, ctx.owner, before, limit).await?
    } else {
        domain::messages_latest(&st.pool, ctx.conversation, ctx.owner, limit).await?
    };
    let has_more =
        messages.len() as i64 == limit && messages.first().map(|m| m.seq > 1).unwrap_or(false);
    Ok(Json(ToolHistoryPage {
        messages: messages.into_iter().map(ToolMessage::from).collect(),
        has_more,
    }))
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
