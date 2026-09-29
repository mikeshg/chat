//! Agent A27：工作区（某台电脑上的一个目录）与工作区对话。
//! 对话就是 type='agent' 且 workspace_id 非空的会话：建立时选定 Agent，之后不能换；
//! 模型、模式记在会话行上，空串 = 跟随 Agent 配置。

use crate::agents::{check_token, uid};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, sqlx::FromRow)]
pub struct WorkspaceView {
    pub id: Uuid,
    pub host_id: Uuid,
    pub path: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct AddWorkspaceReq {
    pub path: String,
}

#[derive(Deserialize)]
pub struct CreateSessionReq {
    pub agent_id: Uuid,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
}

#[derive(Serialize)]
pub struct CreatedSession {
    pub conversation_id: Uuid,
}

/// 缺省字段不改
#[derive(Deserialize)]
pub struct PatchSessionReq {
    pub name: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
}

async fn own_host(st: &SharedState, uid: Uuid, host: Uuid) -> AppResult<()> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM agent_hosts WHERE id = $1")
        .bind(host)
        .fetch_optional(&st.pool)
        .await?;
    match owner {
        Some(o) if o == uid => Ok(()),
        _ => Err(AppError::not_found("host not found")),
    }
}

fn check_name(name: &str) -> AppResult<String> {
    let n = name.trim();
    if n.chars().count() > 60 {
        return Err(AppError::bad_request("bad_name"));
    }
    Ok(n.to_string())
}

/// GET /agent-hosts/{id}/workspaces
pub async fn list_workspaces(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(host): Path<Uuid>,
) -> AppResult<Json<Vec<WorkspaceView>>> {
    let uid = uid(&headers, &st).await?;
    own_host(&st, uid, host).await?;
    // 「无工作区」分组（路径空串）每台电脑都有，第一次列出时补上，排在最前
    sqlx::query(
        "INSERT INTO agent_workspaces (host_id, path) VALUES ($1, '')
         ON CONFLICT (host_id, path) DO UPDATE SET deleted_at = NULL",
    )
    .bind(host)
    .execute(&st.pool)
    .await?;
    let rows = sqlx::query_as::<_, WorkspaceView>(
        "SELECT id, host_id, path, created_at FROM agent_workspaces
          WHERE host_id = $1 AND deleted_at IS NULL
          ORDER BY path = '' DESC, created_at, id",
    )
    .bind(host)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

/// POST /agent-hosts/{id}/workspaces：同一路径移除过就恢复（连同它的对话）。
/// 任意客户端都能加；路径存不存在由电脑在对话第一次运行时报告。
pub async fn add_workspace(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(host): Path<Uuid>,
    Json(req): Json<AddWorkspaceReq>,
) -> AppResult<Json<WorkspaceView>> {
    let uid = uid(&headers, &st).await?;
    own_host(&st, uid, host).await?;
    let path = req.path.trim();
    if path.is_empty() || path.chars().count() > 500 || path.chars().any(char::is_control) {
        return Err(AppError::bad_request("bad_path"));
    }
    let row = sqlx::query_as::<_, WorkspaceView>(
        "INSERT INTO agent_workspaces (host_id, path) VALUES ($1, $2)
         ON CONFLICT (host_id, path) DO UPDATE SET deleted_at = NULL
         RETURNING id, host_id, path, created_at",
    )
    .bind(host)
    .bind(path)
    .fetch_one(&st.pool)
    .await?;
    crate::api::notify(&st, &[uid], "agent_workspaces");
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(row))
}

/// DELETE /agent-workspaces/{id}：软删除，它的对话跟着从列表里消失。「无工作区」分组不能移除（404）。
pub async fn remove_workspace(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    let done = sqlx::query(
        "UPDATE agent_workspaces w SET deleted_at = now()
           FROM agent_hosts h
          WHERE w.id = $1 AND h.id = w.host_id AND h.user_id = $2 AND w.deleted_at IS NULL
            AND w.path <> ''",
    )
    .bind(id)
    .bind(uid)
    .execute(&st.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("workspace not found"));
    }
    crate::api::notify(&st, &[uid], "agent_workspaces");
    crate::api::notify(&st, &[uid], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /agent-workspaces/{id}/sessions：Agent 必须是自己的、未删除、和工作区在同一台电脑上。
pub async fn create_session(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(ws): Path<Uuid>,
    Json(req): Json<CreateSessionReq>,
) -> AppResult<Json<CreatedSession>> {
    let uid = uid(&headers, &st).await?;
    let name = check_name(&req.name)?;
    let model = check_token(&req.model, "bad_model")?;
    let mode = check_token(&req.mode, "bad_mode")?;
    let ws_host: Option<Uuid> = sqlx::query_scalar(
        "SELECT w.host_id FROM agent_workspaces w JOIN agent_hosts h ON h.id = w.host_id
          WHERE w.id = $1 AND h.user_id = $2 AND w.deleted_at IS NULL",
    )
    .bind(ws)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    let ws_host = ws_host.ok_or_else(|| AppError::not_found("workspace not found"))?;
    let agent_host: Option<Uuid> = sqlx::query_scalar(
        "SELECT host_id FROM agents WHERE id = $1 AND owner_id = $2 AND deleted_at IS NULL",
    )
    .bind(req.agent_id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    let agent_host = agent_host.ok_or_else(|| AppError::not_found("agent not found"))?;
    if agent_host != ws_host {
        return Err(AppError::bad_request("agent_on_other_host"));
    }
    let mut tx = st.pool.begin().await?;
    let conv: Uuid = sqlx::query_scalar(
        "INSERT INTO conversations (type, owner_id, agent_id, workspace_id, name, agent_model, agent_mode)
         VALUES ('agent', $1, $2, $3, NULLIF($4, ''), $5, $6)
         RETURNING id",
    )
    .bind(uid)
    .bind(req.agent_id)
    .bind(ws)
    .bind(&name)
    .bind(&model)
    .bind(&mode)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(conv)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(CreatedSession {
        conversation_id: conv,
    }))
}

/// 自己的 Agent 对话 → 是否工作区对话。别人的或不是 Agent 对话一律 404。
async fn own_agent_conv(st: &SharedState, uid: Uuid, conv: Uuid) -> AppResult<bool> {
    let row: Option<bool> = sqlx::query_scalar(
        "SELECT workspace_id IS NOT NULL FROM conversations
          WHERE id = $1 AND type = 'agent' AND owner_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    row.ok_or_else(|| AppError::not_found("conversation not found"))
}

/// PATCH /agent-sessions/{id}：改名（只限工作区对话）、模型、模式。
/// 专属会话也能改模型 / 模式；下一条消息起生效。
pub async fn patch_session(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<PatchSessionReq>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    let in_workspace = own_agent_conv(&st, uid, conv).await?;
    let name = req.name.as_deref().map(check_name).transpose()?;
    if name.is_some() && !in_workspace {
        return Err(AppError::bad_request("cannot_rename"));
    }
    let model = req
        .model
        .as_deref()
        .map(|m| check_token(m, "bad_model"))
        .transpose()?;
    let mode = req
        .mode
        .as_deref()
        .map(|m| check_token(m, "bad_mode"))
        .transpose()?;
    sqlx::query(
        "UPDATE conversations
            SET name = CASE WHEN $2::text IS NULL THEN name ELSE NULLIF($2, '') END,
                agent_model = COALESCE($3, agent_model),
                agent_mode = COALESCE($4, agent_mode)
          WHERE id = $1",
    )
    .bind(conv)
    .bind(name)
    .bind(model)
    .bind(mode)
    .execute(&st.pool)
    .await?;
    crate::api::notify(&st, &[uid], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /agent-sessions/{id}：只删工作区对话；还有进行中的调用时 409。
pub async fn delete_session(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    if !own_agent_conv(&st, uid, conv).await? {
        return Err(AppError::bad_request("cannot_delete"));
    }
    let done = sqlx::query(
        "DELETE FROM conversations c
          WHERE c.id = $1
            AND NOT EXISTS (SELECT 1 FROM agent_invocations i
                             WHERE i.conversation_id = c.id AND i.finished_at IS NULL)",
    )
    .bind(conv)
    .execute(&st.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::conflict("invocation_running"));
    }
    crate::api::notify(&st, &[uid], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DirsQuery {
    #[serde(default)]
    pub path: String,
}

/// GET /agent-hosts/{id}/dirs?path=：请那台电脑列出 path 下的子目录（空 = 盘符 / 主目录）。
/// 离线 409 host_offline，10 秒无响应 504。
pub async fn list_dirs(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(host): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<DirsQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let uid = uid(&headers, &st).await?;
    own_host(&st, uid, host).await?;
    if q.path.chars().count() > 500 {
        return Err(AppError::bad_request("bad_path"));
    }
    let request_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.registry_wait.insert(request_id, tx);
    let payload =
        serde_json::json!({"type": "list_dirs", "request_id": request_id, "path": q.path})
            .to_string();
    if !st.host_online(host) || !st.send_to_host(host, &payload) {
        st.registry_wait.remove(&request_id);
        return Err(AppError::conflict("host_offline"));
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(mut v)) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("type");
                o.remove("request_id");
            }
            Ok(Json(v))
        }
        _ => {
            st.registry_wait.remove(&request_id);
            Err(AppError::gateway_timeout("dirs_timeout"))
        }
    }
}
