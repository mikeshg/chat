// Agent A01：Agent / AgentHost / AgentInvocation 的 REST 与派发。
//
// - REST：GET /v1/agent-hosts、GET|POST /v1/agents、
//   GET /v1/conversations/{id}/agent-invocations —— 一律只作用于本人资源，
//   别人的宿主/Agent 按 id 访问返回 404，不泄露存在性。
// - 派发挂点在 ws::broadcast_posted：专属会话里的主人文字消息触发一次调用；
//   宿主在线 → running + invocation_start；离线 → queued（queue_deadline = now + 生效超时，
//   超时 0 则 NULL）。宿主 host_ready 后按 created_at 把排队调用改成 running 再下发。

use crate::auth;
use crate::domain::{self, PostError};
use crate::error::{AppError, AppResult};
use crate::model::{
    AgentHostView, AgentUserSettings, AgentView, CreateAgentReq, InvocationView, PatchAgentReq,
    UpdateAgentUserSettingsReq, WsOut,
};
use crate::state::SharedState;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use uuid::Uuid;

async fn uid(headers: &HeaderMap, st: &SharedState) -> AppResult<Uuid> {
    Ok(auth::require_user(st, headers).await?.id)
}

// ---------- REST ----------

pub async fn list_hosts(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentHostView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, label, platform, last_seen_at FROM agent_hosts
          WHERE user_id = $1 ORDER BY created_at, id",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, label, platform, last_seen_at)| AgentHostView {
                id,
                label,
                platform,
                online: st.host_online(id),
                last_seen_at,
            })
            .collect(),
    ))
}

/// AgentView 的查询列（list / patch 共用）
const AGENT_SELECT: &str = "SELECT a.id, a.name, a.host_id, a.runtime, a.cwd, a.created_at,
        (SELECT c.id FROM conversations c
          WHERE c.agent_id = a.id AND c.type = 'agent') AS conversation_id,
        a.output_mode, a.queue_timeout_secs
   FROM agents a";

#[derive(sqlx::FromRow)]
struct AgentDbRow {
    id: Uuid,
    name: String,
    host_id: Uuid,
    runtime: serde_json::Value,
    cwd: String,
    created_at: DateTime<Utc>,
    conversation_id: Option<Uuid>,
    output_mode: Option<String>,
    queue_timeout_secs: Option<i32>,
}

fn agent_view(st: &SharedState, r: AgentDbRow) -> AgentView {
    AgentView {
        id: r.id,
        name: r.name,
        host_id: r.host_id,
        host_online: st.host_online(r.host_id),
        runtime: r.runtime,
        cwd: r.cwd,
        conversation_id: r.conversation_id.unwrap_or_default(),
        created_at: r.created_at,
        output_mode: r.output_mode,
        queue_timeout_secs: r.queue_timeout_secs,
    }
}

pub async fn list_agents(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<_, AgentDbRow>(&format!(
        "{AGENT_SELECT} WHERE a.owner_id = $1 ORDER BY a.created_at, a.id"
    ))
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.into_iter().map(|r| agent_view(&st, r)).collect()))
}

/// Agent 名规则：1–32 字符，只允许字母/数字/`_`/`-`，首字符不能是 `-`。
/// 与 DB CHECK 同规则；REST 先拦一遍给出稳定的 bad_name。
fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=32).contains(&n)
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub async fn create_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CreateAgentReq>,
) -> AppResult<Json<AgentView>> {
    let uid = uid(&headers, &st).await?;
    if !valid_name(&req.name) {
        return Err(AppError::bad_request("bad_name"));
    }
    let command = req.runtime.command.trim();
    if command.is_empty() || command.chars().count() > 500 {
        return Err(AppError::bad_request("bad_runtime"));
    }
    if req.runtime.args.len() > 50 || req.runtime.args.iter().any(|a| a.chars().count() > 500) {
        return Err(AppError::bad_request("bad_runtime"));
    }
    let cwd = req.cwd.trim().to_string();
    if cwd.chars().count() > 500 {
        return Err(AppError::bad_request("bad_runtime"));
    }
    let runtime = serde_json::json!({"command": command, "args": req.runtime.args});

    let mut tx = st.pool.begin().await?;
    // 宿主必须属于当前用户；否则 404（不暴露他人资源存在性）
    let host_user =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1 FOR UPDATE")
            .bind(req.host_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found("host not found"))?;
    if host_user != uid {
        return Err(AppError::not_found("host not found"));
    }

    let agent_id = match sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agents (owner_id, host_id, name, runtime, cwd)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(uid)
    .bind(req.host_id)
    .bind(&req.name)
    .bind(&runtime)
    .bind(&cwd)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        Err(sqlx::Error::Database(db)) if db.constraint() == Some("uq_agents_owner_name") => {
            return Err(AppError::conflict("name_taken"));
        }
        Err(e) => return Err(e.into()),
    };
    // 专属会话：type='agent'、owner=主人、唯一成员、name 恒 NULL、agent_id 绑定
    let conv_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO conversations (type, owner_id, agent_id) VALUES ('agent', $1, $2)
         RETURNING id",
    )
    .bind(uid)
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(conv_id)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // 主人的普通连接刷新会话列表（宿主连接不订阅业务帧）
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(AgentView {
        id: agent_id,
        name: req.name,
        host_id: req.host_id,
        host_online: st.host_online(req.host_id),
        runtime,
        cwd,
        conversation_id: conv_id,
        created_at: Utc::now(),
        output_mode: None,
        queue_timeout_secs: None,
    }))
}

// ---------- 设置：个人默认 + 单个 Agent 覆盖 ----------

const OUTPUT_MODES: [&str; 3] = ["stream", "status", "steps"];
/// 排队超时上限 30 天；0 = 永不
const MAX_QUEUE_TIMEOUT_SECS: i32 = 30 * 24 * 3600;

fn valid_output_mode(m: &str) -> bool {
    OUTPUT_MODES.contains(&m)
}

fn valid_queue_timeout(secs: i32) -> bool {
    (0..=MAX_QUEUE_TIMEOUT_SECS).contains(&secs)
}

async fn user_settings(pool: &PgPool, uid: Uuid) -> Result<AgentUserSettings, sqlx::Error> {
    Ok(sqlx::query_as::<_, AgentUserSettings>(
        "SELECT output_mode, queue_timeout_secs FROM agent_user_settings WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?
    .unwrap_or(AgentUserSettings {
        output_mode: "stream".into(),
        queue_timeout_secs: 86400,
    }))
}

/// 某个 Agent 的生效设置：Agent 覆盖 → 主人的个人默认 → 系统默认
pub async fn effective_settings(
    pool: &PgPool,
    agent_id: Uuid,
) -> Result<AgentUserSettings, sqlx::Error> {
    sqlx::query_as::<_, AgentUserSettings>(
        "SELECT COALESCE(a.output_mode, s.output_mode, 'stream') AS output_mode,
                COALESCE(a.queue_timeout_secs, s.queue_timeout_secs, 86400) AS queue_timeout_secs
           FROM agents a
           LEFT JOIN agent_user_settings s ON s.user_id = a.owner_id
          WHERE a.id = $1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
}

pub async fn get_user_settings(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<AgentUserSettings>> {
    let uid = uid(&headers, &st).await?;
    Ok(Json(user_settings(&st.pool, uid).await?))
}

pub async fn put_user_settings(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<UpdateAgentUserSettingsReq>,
) -> AppResult<Json<AgentUserSettings>> {
    let uid = uid(&headers, &st).await?;
    let mut cur = user_settings(&st.pool, uid).await?;
    if let Some(m) = req.output_mode {
        if !valid_output_mode(&m) {
            return Err(AppError::bad_request("bad_output_mode"));
        }
        cur.output_mode = m;
    }
    if let Some(t) = req.queue_timeout_secs {
        if !valid_queue_timeout(t) {
            return Err(AppError::bad_request("bad_queue_timeout"));
        }
        cur.queue_timeout_secs = t;
    }
    sqlx::query(
        "INSERT INTO agent_user_settings (user_id, output_mode, queue_timeout_secs)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO UPDATE
            SET output_mode = $2, queue_timeout_secs = $3, updated_at = now()",
    )
    .bind(uid)
    .bind(&cur.output_mode)
    .bind(cur.queue_timeout_secs)
    .execute(&st.pool)
    .await?;
    Ok(Json(cur))
}

/// PATCH /v1/agents/{id}：本票只有两个覆盖项；其余配置项由后续票往 PatchAgentReq 里加。
pub async fn patch_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchAgentReq>,
) -> AppResult<Json<AgentView>> {
    let uid = uid(&headers, &st).await?;
    if let Some(Some(m)) = &req.output_mode {
        if !valid_output_mode(m) {
            return Err(AppError::bad_request("bad_output_mode"));
        }
    }
    if let Some(Some(t)) = req.queue_timeout_secs {
        if !valid_queue_timeout(t) {
            return Err(AppError::bad_request("bad_queue_timeout"));
        }
    }
    let mut tx = st.pool.begin().await?;
    let owner =
        sqlx::query_scalar::<_, Uuid>("SELECT owner_id FROM agents WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if owner != Some(uid) {
        return Err(AppError::not_found("agent not found"));
    }
    if let Some(m) = req.output_mode {
        sqlx::query("UPDATE agents SET output_mode = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(t) = req.queue_timeout_secs {
        sqlx::query("UPDATE agents SET queue_timeout_secs = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(t)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let row = sqlx::query_as::<_, AgentDbRow>(&format!("{AGENT_SELECT} WHERE a.id = $1"))
        .bind(id)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(agent_view(&st, row)))
}

#[derive(Deserialize)]
pub struct InvQuery {
    limit: Option<i64>,
}

/// GET /conversations/{id}/agent-invocations：刷新后找回「跑完了吗」。
/// 仅成员可读（专属会话成员=主人）；按 created_at DESC、limit 默认 50。
pub async fn list_invocations(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<InvQuery>,
) -> AppResult<Json<Vec<InvocationView>>> {
    let uid = uid(&headers, &st).await?;
    domain::membership(&st.pool, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    let rows = sqlx::query_as::<_, InvocationView>(
        "SELECT id, agent_id, conversation_id, trigger_message_id, status, error,
                created_at, started_at, finished_at
           FROM agent_invocations WHERE conversation_id = $1
          ORDER BY created_at DESC, id DESC LIMIT $2",
    )
    .bind(conv)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "timed_out")
}

/// POST /v1/agent-invocations/{id}/cancel
/// 只有该调用所属 Agent 的主人能取消；别人或不存在一律 404。
/// queued → cancelled；running / awaiting_approval → cancelled，并通知宿主；
/// 已是终态 → 409 already_finished。
pub async fn cancel_invocation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<InvocationView>> {
    let uid = uid(&headers, &st).await?;
    let row = sqlx::query_as::<_, (String, Uuid, Uuid, Uuid)>(
        "SELECT i.status, a.owner_id, a.host_id, i.conversation_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some((status, owner, host_id, conv)) = row else {
        return Err(AppError::not_found("invocation not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("invocation not found"));
    }
    if terminal_status(&status) {
        return Err(AppError::conflict("already_finished"));
    }
    let inv = sqlx::query_as::<_, InvocationView>(
        "UPDATE agent_invocations
            SET status = 'cancelled', error = NULL, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
            AND status IN ('queued', 'running', 'awaiting_approval')
         RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some(inv) = inv else {
        return Err(AppError::conflict("already_finished"));
    };
    if status == "running" || status == "awaiting_approval" {
        let payload = serde_json::json!({
            "type": "invocation_cancel",
            "invocation_id": id,
        })
        .to_string();
        let _ = st.send_to_host(host_id, &payload);
    }
    let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
    push_invocation(&st, &members, &inv);
    Ok(Json(inv))
}

// ---------- 调用派发（挂在 broadcast_posted 上） ----------

struct AgentRow {
    id: Uuid,
    owner: Uuid,
    host_id: Uuid,
    name: String,
    runtime: serde_json::Value,
    cwd: String,
}

async fn agent_of_conv(pool: &PgPool, conv: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, Option<Uuid>, Uuid, String, serde_json::Value, String)>(
        "SELECT a.id, c.owner_id, a.host_id, a.name, a.runtime, a.cwd
           FROM conversations c JOIN agents a ON a.id = c.agent_id
          WHERE c.id = $1 AND c.type = 'agent'",
    )
    .bind(conv)
    .fetch_optional(pool)
    .await
    .map(|r| {
        r.map(|(id, owner, host_id, name, runtime, cwd)| AgentRow {
            id,
            owner: owner.unwrap_or_default(),
            host_id,
            name,
            runtime,
            cwd,
        })
    })
}

/// 新文字消息落库后的挂点：专属会话里主人发的普通文字触发一次调用。
/// Agent 自己的回写（message.agent 非空）与幂等重放不触发。
pub async fn maybe_invoke(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    if !posted.inserted || posted.message.agent.is_some() || posted.message.kind != "text" {
        return;
    }
    let agent = match agent_of_conv(&st.pool, conv).await {
        Ok(Some(a)) => a,
        _ => return,
    };
    // 只有主人本人的消息触发；专属会话唯一成员即主人，显式校验防结构漂移
    if posted.message.sender_id != agent.owner {
        return;
    }
    dispatch(
        st,
        agent,
        conv,
        posted.message.id,
        posted.message.content.clone(),
        &posted.member_ids,
    )
    .await;
}

async fn dispatch(
    st: &SharedState,
    agent: AgentRow,
    conv: Uuid,
    trigger: Uuid,
    prompt: String,
    member_ids: &[Uuid],
) {
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    // 在线先标 running 再下发，客户端看到的第一帧就是 running（不是先闪一下排队）。
    // 发不出去则改回 queued，与离线同语义。
    let inv = if online {
        insert_running(&st.pool, &agent, conv, trigger).await
    } else {
        insert_queued(&st.pool, &agent, conv, trigger, timeout).await
    };
    let Ok(inv) = inv else {
        tracing::error!("agent_invocation insert failed");
        return;
    };
    push_invocation(st, member_ids, &inv);
    if inv.status != "running" {
        return;
    }
    let payload = invocation_start_payload(
        inv.id,
        agent.id,
        &agent.name,
        &agent.runtime,
        &agent.cwd,
        &prompt,
    );
    if !st.send_to_host(agent.host_id, &payload) {
        if let Some(q) = requeue(&st.pool, inv.id, timeout).await {
            push_invocation(st, member_ids, &q);
        }
    }
}

async fn queue_timeout_secs(pool: &PgPool, agent_id: Uuid) -> i32 {
    effective_settings(pool, agent_id)
        .await
        .map(|s| s.queue_timeout_secs)
        .unwrap_or(86400)
}

const INV_RETURNING: &str =
    "RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at";

async fn insert_running(
    pool: &PgPool,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Uuid,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, status, started_at)
         VALUES ($1, $2, $3, 'running', now())
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .fetch_one(pool)
    .await
}

async fn insert_queued(
    pool: &PgPool,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Uuid,
    timeout_secs: i32,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, status, queue_deadline)
         VALUES ($1, $2, $3, 'queued',
                 CASE WHEN $4 > 0 THEN now() + make_interval(secs => $4::double precision) END)
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(timeout_secs)
    .fetch_one(pool)
    .await
}

fn invocation_start_payload(
    inv_id: Uuid,
    agent_id: Uuid,
    name: &str,
    runtime: &serde_json::Value,
    cwd: &str,
    prompt: &str,
) -> String {
    serde_json::json!({
        "type": "invocation_start",
        "invocation_id": inv_id,
        "agent": {"id": agent_id, "name": name, "runtime": runtime, "cwd": cwd},
        "prompt": prompt,
    })
    .to_string()
}

/// 下发失败：从 running 改回 queued，重新计算截止时间。
async fn requeue(pool: &PgPool, id: Uuid, timeout_secs: i32) -> Option<InvocationView> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'queued', error = NULL, started_at = NULL, finished_at = NULL,
                queue_deadline = CASE WHEN $2 > 0 THEN now() + make_interval(secs => $2::double precision) END
          WHERE id = $1 AND status = 'running' AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .bind(timeout_secs)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

#[derive(sqlx::FromRow)]
struct QueuedStart {
    id: Uuid,
    conversation_id: Uuid,
    agent_id: Uuid,
    name: String,
    runtime: serde_json::Value,
    cwd: String,
    prompt: String,
}

/// host_ready 之后：该宿主上所有 queued 调用按 created_at 逐个改成 running 并下发。
pub async fn flush_queued(st: &SharedState, host_id: Uuid) {
    let rows = sqlx::query_as::<_, QueuedStart>(
        "SELECT i.id, i.conversation_id, a.id AS agent_id, a.name, a.runtime, a.cwd,
                m.content AS prompt
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN messages m ON m.id = i.trigger_message_id
          WHERE a.host_id = $1 AND i.status = 'queued'
          ORDER BY i.created_at ASC, i.id ASC",
    )
    .bind(host_id)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for row in rows {
        let inv = match promote_queued(&st.pool, row.id).await {
            Ok(Some(inv)) => inv,
            Ok(None) => continue, // 清扫或取消抢先改了状态
            Err(e) => {
                tracing::warn!(error = %e, "排队调用转 running 失败");
                continue;
            }
        };
        let members = domain::member_ids(&st.pool, row.conversation_id)
            .await
            .unwrap_or_default();
        push_invocation(st, &members, &inv);
        let payload = invocation_start_payload(
            inv.id,
            row.agent_id,
            &row.name,
            &row.runtime,
            &row.cwd,
            &row.prompt,
        );
        if !st.send_to_host(host_id, &payload) {
            let timeout = queue_timeout_secs(&st.pool, row.agent_id).await;
            if let Some(q) = requeue(&st.pool, inv.id, timeout).await {
                push_invocation(st, &members, &q);
            }
        }
    }
}

async fn promote_queued(pool: &PgPool, id: Uuid) -> Result<Option<InvocationView>, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'running', started_at = now(), error = NULL, queue_deadline = NULL
          WHERE id = $1 AND status = 'queued' AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 后台清扫：间隔见 `AppState::queue_sweep_interval_ms`（默认 30s）。
/// 睡眠按 250ms 切片重读间隔，测试把原子量调小后最多一个切片就能生效。
pub fn spawn_queue_sweeper(st: SharedState) {
    tokio::spawn(async move {
        loop {
            wait_sweep_interval(&st).await;
            if let Err(e) = sweep_timed_out(&st).await {
                tracing::warn!(error = %e, "排队超时清扫失败");
            }
        }
    });
}

async fn wait_sweep_interval(st: &SharedState) {
    let started = Instant::now();
    loop {
        let ms = st.queue_sweep_interval_ms.load(Ordering::Relaxed).max(50);
        let target = Duration::from_millis(ms);
        if started.elapsed() >= target {
            return;
        }
        let left = target.saturating_sub(started.elapsed());
        tokio::time::sleep(left.min(Duration::from_millis(250))).await;
    }
}

async fn sweep_timed_out(st: &SharedState) -> Result<(), sqlx::Error> {
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'timed_out', finished_at = now()
          WHERE status = 'queued'
            AND queue_deadline IS NOT NULL
            AND queue_deadline < now()
         {INV_RETURNING}"
    ))
    .fetch_all(&st.pool)
    .await?;
    for inv in invs {
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        push_invocation(st, &members, &inv);
    }
    Ok(())
}

/// 状态推进 + 广播 agent_invocation 帧给会话成员。
/// 只从非终态推进（并发完成以先到者为准）。
pub async fn finish_invocation(
    st: &SharedState,
    id: Uuid,
    status: &str,
    error: Option<&str>,
    member_ids: &[Uuid],
) -> Result<Option<InvocationView>, sqlx::Error> {
    let inv = sqlx::query_as::<_, InvocationView>(
        "UPDATE agent_invocations SET status = $2, error = $3, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
         RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .fetch_optional(&st.pool)
    .await?;
    if let Some(inv) = &inv {
        push_invocation(st, member_ids, inv);
    }
    Ok(inv)
}

fn push_invocation(st: &SharedState, member_ids: &[Uuid], inv: &InvocationView) {
    let p = serde_json::to_string(&WsOut::AgentInvocation {
        invocation: inv.clone(),
    })
    .unwrap();
    st.send_to_users(member_ids, &p);
}

/// 宿主回传的 invocation_finish：校验该调用确实派给这条宿主连接。
/// 行锁内推进终态。取消已经落库时这里读到的不是 running，直接忽略，不发回复。
/// ok:true → 先写 Agent 消息再置 succeeded；ok:false → failed + error（截 500 字符）。
pub async fn handle_finish(st: &SharedState, host_id: Uuid, v: &serde_json::Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let mut tx = match st.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "invocation_finish 开事务失败");
            return;
        }
    };
    let row = sqlx::query_as::<_, (String, Option<Uuid>, Uuid, Uuid, Uuid)>(
        "SELECT i.status, c.owner_id, i.conversation_id, a.id, a.host_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN conversations c ON c.id = i.conversation_id
          WHERE i.id = $1
          FOR UPDATE OF i",
    )
    .bind(inv_id)
    .fetch_optional(&mut *tx)
    .await;
    let Ok(Some((status, owner, conv, agent_id, real_host))) = row else {
        return;
    };
    // 已取消 / 已超时 / 已结束，或不是这条宿主的调用：不发回复
    if status != "running" || real_host != host_id {
        return;
    }
    let owner = owner.unwrap_or_default();
    let member_ids = vec![owner];

    if v["ok"].as_bool() == Some(true) {
        let text = v["text"].as_str().unwrap_or_default().to_string();
        // 持锁写回复，取消的 UPDATE 会等这把锁，提交后看到的已是终态
        post_agent_text(st, conv, owner, agent_id, &text).await;
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'succeeded', error = NULL, finished_at = now()
              WHERE id = $1 AND status = 'running'
             {INV_RETURNING}"
        ))
        .bind(inv_id)
        .fetch_optional(&mut *tx)
        .await;
        if tx.commit().await.is_err() {
            return;
        }
        if let Ok(Some(inv)) = updated {
            push_invocation(st, &member_ids, &inv);
        }
    } else {
        let err = v["error"].as_str().unwrap_or("agent error");
        let err: String = err.chars().take(500).collect();
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'failed', error = $2, finished_at = now()
              WHERE id = $1 AND status = 'running'
             {INV_RETURNING}"
        ))
        .bind(inv_id)
        .bind(&err)
        .fetch_optional(&mut *tx)
        .await;
        if tx.commit().await.is_err() {
            return;
        }
        if let Ok(Some(inv)) = updated {
            push_invocation(st, &member_ids, &inv);
        }
    }
}

/// Agent 正文拆条写库 + 逐条广播。空正文不发消息（调用仍算成功）。
pub async fn post_agent_text(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    agent_id: Uuid,
    text: &str,
) {
    let mut rest = text;
    while !rest.is_empty() {
        let take = rest
            .char_indices()
            .nth(4000)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(take);
        rest = tail;
        if head.trim().is_empty() {
            continue; // 纯空白分片跳过，免得撞上 1-4000 校验
        }
        let posted = domain::post_message(
            &st.pool,
            domain::PostMessage {
                conversation_id: conv,
                sender_id: owner,
                content: head,
                client_msg_id: None,
                attachment_id: None,
                reply_to_id: None,
                poll: None,
                forwarded_from: None,
                forward_attachment: None,
                agent_id: Some(agent_id),
            },
        )
        .await;
        match posted {
            Ok(p) => crate::ws::broadcast_posted(st, conv, &p).await,
            Err(PostError::Deny { code, message }) => {
                tracing::warn!(code, message, "agent message rejected");
                break;
            }
            Err(PostError::Db(e)) => {
                tracing::error!(error = %e, "agent message insert failed");
                break;
            }
        }
    }
}
