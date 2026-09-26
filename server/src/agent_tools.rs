// Agent A13：工具注入的服务端半部。
// /v1/agent-tools/* 只认 `Authorization: Invocation <tools_token>`：
// JWT 签名合法 + kind="invocation" + 调用存在且仍在 running/awaiting_approval
// + claims 与库行一致 + 主人未禁用。任一不满足一律 401（不给探测线索）。

use crate::auth;
use crate::domain;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

    // 查库：调用仍在进行 + claims 与库行一致 + 主人（Agent 的主人，不是会话 owner）未禁用。
    // 群 / 私聊 / 定时调用的目标会话 owner 往往不是主人，不能拿 conversations.owner_id 当主人。
    // 结束（succeeded/failed/cancelled/timed_out）即失效，无需单独的撤销表。
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, Uuid, Option<DateTime<Utc>>)>(
        "SELECT i.conversation_id, i.agent_id, i.status, a.owner_id, u.disabled_at
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN users u ON u.id = a.owner_id
          WHERE i.id = $1",
    )
    .bind(claims.sub)
    .fetch_optional(&st.pool)
    .await?;
    let Some((conv, agent, status, owner, disabled_at)) = row else {
        return Err(tool_unauthorized());
    };
    if disabled_at.is_some()
        || owner != claims.owner
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
