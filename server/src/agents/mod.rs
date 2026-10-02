// Agent A01：Agent / AgentHost / AgentInvocation 的 REST 与派发。
//
// - REST：GET /v1/agent-hosts、PATCH /v1/agent-hosts/{id}、GET|POST /v1/agents、
//   GET /v1/conversations/{id}/agent-invocations —— 一律只作用于本人资源，
//   别人的宿主/Agent 按 id 访问返回 404，不泄露存在性。
//   POST /v1/conversations/{id}/agent-context/reset —— 成员清空本会话上下文。
// - 派发挂点在 ws::broadcast_posted：专属会话里主人的文字消息触发该 Agent；
//   私聊 / 群里则按 agent_targets 与手打 @ 解析出发送者自己未删除的 Agent（A11）。
//   宿主在线 → running + invocation_start；离线 → queued（queue_deadline = now + 生效超时，
//   超时 0 则 NULL）。宿主 host_ready 后按 created_at 把排队调用改成 running 再下发。

use crate::auth;
use crate::domain::{self, PostError};
use crate::error::{AppError, AppResult};
use crate::model::{
    AgentHostView, AgentRuntimeReq, AgentStepView, AgentUserSettings, AgentView, CreateAgentReq,
    InvocationView, PatchAgentReq, StreamStepView, UpdateAgentUserSettingsReq, WsOut,
};
use crate::state::{InvRun, SharedState};
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod dispatch;
mod events;
mod registry;
mod revoke;
mod routes;
mod settings;

pub use dispatch::*;
pub use events::*;
pub use registry::*;
pub use revoke::*;
pub use routes::*;
pub use settings::*;

pub(crate) async fn uid(headers: &HeaderMap, st: &SharedState) -> AppResult<Uuid> {
    let id = auth::require_user(st, headers).await?.id;
    // 头像 GET 不走这里。其余 Agent REST 无权一律 403。
    if !crate::agent_access::user_allows(&st.pool, id).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    Ok(id)
}
