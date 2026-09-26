// Agent A13：工具注入的服务端半部。
// /v1/agent-tools/* 只认 `Authorization: Invocation <tools_token>`：
// JWT 签名合法 + kind="invocation" + 调用存在且仍在 running/awaiting_approval
// + claims 与库行一致 + 主人未禁用。任一不满足一律 401（不给探测线索）。

use crate::auth;
use crate::domain;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use axum::extract::{Multipart, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// 校验通过的调用凭据上下文：工具操作被钉死在这一次调用上
pub struct ToolCtx {
    #[allow(dead_code)]
    pub invocation: Uuid,
    pub owner: Uuid,
    #[allow(dead_code)]
    pub agent: Uuid,
    /// 发起调用的会话（工具操作唯一的会话范围，没有参数可改）
    pub conversation: Uuid,
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

    // 查库：调用仍在进行 + claims 与库行一致 + 主人未禁用。
    // 结束（succeeded/failed/cancelled/timed_out）即失效，无需单独的撤销表。
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, Option<Uuid>, Option<DateTime<Utc>>)>(
        "SELECT i.conversation_id, i.agent_id, i.status, c.owner_id, u.disabled_at
           FROM agent_invocations i
           JOIN conversations c ON c.id = i.conversation_id
           JOIN users u ON u.id = c.owner_id
          WHERE i.id = $1",
    )
    .bind(claims.sub)
    .fetch_optional(&st.pool)
    .await?;
    let Some((conv, agent, status, owner, disabled_at)) = row else {
        return Err(tool_unauthorized());
    };
    if disabled_at.is_some()
        || owner != Some(claims.owner)
        || agent != claims.agent
        || !matches!(status.as_str(), "running" | "awaiting_approval")
    {
        return Err(tool_unauthorized());
    }
    Ok(ToolCtx {
        invocation: claims.sub,
        owner: claims.owner,
        agent,
        conversation: conv,
    })
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

/// POST /v1/agent-tools/files（Agent A05）：调用产出文件上传，multipart 字段 `file`。
/// 附件记为：上传者=调用主人、会话=发起调用的会话，并打 agent_invocation_id 归属标记——
/// invocation_finish.files 只能引用本次调用上传的附件。
/// 配额/大小上限/图片缩略图与普通上传同一管线；超配额回 413 + quota_exceeded。
pub async fn tool_upload_file(
    State(st): State<SharedState>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> AppResult<Json<Value>> {
    let ctx = require_invocation(&st, &headers).await?;
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
        ctx.conversation,
        rd.file_name,
        rd.mime,
        rd.buf,
        None,
        Some(ctx.invocation),
    )
    .await?;
    Ok(Json(json!({"attachment_id": att.id})))
}
