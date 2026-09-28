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

async fn uid(headers: &HeaderMap, st: &SharedState) -> AppResult<Uuid> {
    let id = auth::require_user(st, headers).await?.id;
    // 头像 GET 不走这里。其余 Agent REST 无权一律 403。
    if !crate::agent_access::user_allows(&st.pool, id).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    Ok(id)
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

#[derive(Deserialize)]
pub struct PatchHostReq {
    label: String,
}

/// 主人给自己的宿主改名。去首尾空白，1–60 字；别人的或不存在的宿主 404。
/// 成功后给主人的普通连接推 agent_host_status（带新 label）。
pub async fn patch_host(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchHostReq>,
) -> AppResult<Json<AgentHostView>> {
    let uid = uid(&headers, &st).await?;
    let label = req.label.trim();
    let n = label.chars().count();
    if !(1..=60).contains(&n) {
        return Err(AppError::bad_request("bad_label"));
    }
    let label = label.to_string();
    let row = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "UPDATE agent_hosts SET label = $1
          WHERE id = $2 AND user_id = $3
          RETURNING id, label, platform, last_seen_at",
    )
    .bind(&label)
    .bind(id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    let Some((id, label, platform, last_seen_at)) = row else {
        return Err(AppError::not_found("host not found"));
    };
    let online = st.host_online(id);
    let payload = serde_json::to_string(&WsOut::AgentHostStatus {
        host_id: id,
        online,
        label: Some(label.clone()),
    })
    .unwrap();
    st.send_to_user(uid, &payload);
    Ok(Json(AgentHostView {
        id,
        label,
        platform,
        online,
        last_seen_at,
    }))
}

/// AgentView 的查询列（list / patch 共用）
const AGENT_SELECT: &str = "SELECT a.id, a.name, a.host_id, a.runtime, a.cwd, a.created_at,
        (SELECT c.id FROM conversations c
          WHERE c.agent_id = a.id AND c.type = 'agent') AS conversation_id,
        a.output_mode, a.queue_timeout_secs,
        a.extra_dirs, a.model, a.mode, a.instructions, a.env,
        a.avatar_version, a.reported_options,
        (a.deleted_at IS NOT NULL) AS deleted,
        a.permission_tier
   FROM agents a";

#[derive(sqlx::FromRow)]
struct AgentDbRow {
    id: Uuid,
    name: String,
    host_id: Uuid,
    runtime: Value,
    cwd: String,
    created_at: DateTime<Utc>,
    conversation_id: Option<Uuid>,
    output_mode: Option<String>,
    queue_timeout_secs: Option<i32>,
    extra_dirs: Vec<String>,
    model: String,
    mode: String,
    instructions: String,
    env: Value,
    avatar_version: i32,
    reported_options: Value,
    deleted: bool,
    permission_tier: String,
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
        extra_dirs: r.extra_dirs,
        model: r.model,
        mode: r.mode,
        instructions: r.instructions,
        env: r.env,
        reported_options: r.reported_options,
        avatar_version: r.avatar_version,
        deleted: r.deleted,
        permission_tier: r.permission_tier,
    }
}

pub async fn list_agents(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<_, AgentDbRow>(&format!(
        "{AGENT_SELECT} WHERE a.owner_id = $1 AND a.deleted_at IS NULL ORDER BY a.created_at, a.id"
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
    let (runtime, cwd) = normalize_runtime(&req.runtime, &req.cwd)?;
    let permission_tier = normalize_tier(req.permission_tier.as_deref())?;

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
        "INSERT INTO agents (owner_id, host_id, name, runtime, cwd, permission_tier)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(uid)
    .bind(req.host_id)
    .bind(&req.name)
    .bind(&runtime)
    .bind(&cwd)
    .bind(&permission_tier)
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
        extra_dirs: Vec::new(),
        model: String::new(),
        mode: String::new(),
        instructions: String::new(),
        env: json!({}),
        reported_options: json!({}),
        avatar_version: 0,
        deleted: false,
        permission_tier,
    }))
}

/// command / args / cwd 与登记表 id / version 的格式校验。登记表不联网。
fn normalize_runtime(rt: &AgentRuntimeReq, cwd: &str) -> AppResult<(Value, String)> {
    let cwd = cwd.trim().to_string();
    if cwd.chars().count() > 500 {
        return Err(AppError::bad_request("bad_runtime"));
    }
    let value = match rt {
        AgentRuntimeReq::Registry { registry } => {
            let id = check_pin(&registry.id, 128)?;
            let version = check_pin(&registry.version, 64)?;
            json!({"registry": {"id": id, "version": version}})
        }
        AgentRuntimeReq::Command { command, args } => {
            let command = command.trim();
            if command.is_empty() || command.chars().count() > 500 {
                return Err(AppError::bad_request("bad_runtime"));
            }
            if args.len() > 50 || args.iter().any(|a| a.chars().count() > 500) {
                return Err(AppError::bad_request("bad_runtime"));
            }
            json!({"command": command, "args": args})
        }
    };
    Ok((value, cwd))
}

/// 登记表 id / version：非空、不带路径，避免安装目录被写成别的地方。
fn check_pin(s: &str, max: usize) -> AppResult<String> {
    let s = s.trim();
    if s.is_empty()
        || s.chars().count() > max
        || s.chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\' || c == '\0')
        || s.contains("..")
    {
        return Err(AppError::bad_request("bad_runtime"));
    }
    Ok(s.to_string())
}

fn valid_tier(tier: &str) -> bool {
    matches!(tier, "read_only" | "workspace" | "full")
}

/// 缺省 = 只读。非法值 400。
fn normalize_tier(tier: Option<&str>) -> AppResult<String> {
    match tier.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("read_only".into()),
        Some(t) if valid_tier(t) => Ok(t.to_string()),
        Some(_) => Err(AppError::bad_request("bad_permission_tier")),
    }
}

fn check_instructions(s: &str) -> AppResult<()> {
    if s.chars().count() > 8000 {
        return Err(AppError::bad_request("bad_instructions"));
    }
    Ok(())
}

/// 空串 = 用 agent 默认。过长拒绝。
fn check_token(s: &str, code: &'static str) -> AppResult<String> {
    let t = s.trim();
    if t.chars().count() > 200 {
        return Err(AppError::bad_request(code));
    }
    Ok(t.to_string())
}

/// 去掉空行后 ≤ 10 个，每个 ≤ 500 字符。
fn check_extra_dirs(dirs: &[String]) -> AppResult<Vec<String>> {
    let dirs: Vec<String> = dirs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if dirs.len() > 10 || dirs.iter().any(|s| s.chars().count() > 500) {
        return Err(AppError::bad_request("bad_extra_dirs"));
    }
    Ok(dirs)
}

/// 字符串到字符串，≤ 50 项，键 ≤ 100、值 ≤ 4000。空键拒绝。
fn check_env(v: &Value) -> AppResult<Value> {
    let obj = v
        .as_object()
        .ok_or_else(|| AppError::bad_request("bad_env"))?;
    if obj.len() > 50 {
        return Err(AppError::bad_request("bad_env"));
    }
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        if k.is_empty() || k.chars().count() > 100 {
            return Err(AppError::bad_request("bad_env"));
        }
        let s = val
            .as_str()
            .ok_or_else(|| AppError::bad_request("bad_env"))?;
        if s.chars().count() > 4000 {
            return Err(AppError::bad_request("bad_env"));
        }
        out.insert(k.clone(), Value::String(s.to_string()));
    }
    Ok(Value::Object(out))
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
        "SELECT output_mode, queue_timeout_secs, max_call_depth
           FROM agent_user_settings WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?
    .unwrap_or(AgentUserSettings {
        output_mode: "stream".into(),
        queue_timeout_secs: 86400,
        max_call_depth: 3,
    }))
}

/// 某个 Agent 的生效设置：Agent 覆盖 → 主人的个人默认 → 系统默认
pub async fn effective_settings(
    pool: &PgPool,
    agent_id: Uuid,
) -> Result<AgentUserSettings, sqlx::Error> {
    sqlx::query_as::<_, AgentUserSettings>(
        "SELECT COALESCE(a.output_mode, s.output_mode, 'stream') AS output_mode,
                COALESCE(a.queue_timeout_secs, s.queue_timeout_secs, 86400) AS queue_timeout_secs,
                COALESCE(s.max_call_depth, 3) AS max_call_depth
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
    if let Some(d) = req.max_call_depth {
        if !(1..=10).contains(&d) {
            return Err(AppError::bad_request("bad_max_call_depth"));
        }
        cur.max_call_depth = d;
    }
    sqlx::query(
        "INSERT INTO agent_user_settings (user_id, output_mode, queue_timeout_secs, max_call_depth)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id) DO UPDATE
            SET output_mode = $2, queue_timeout_secs = $3, max_call_depth = $4, updated_at = now()",
    )
    .bind(uid)
    .bind(&cur.output_mode)
    .bind(cur.queue_timeout_secs)
    .bind(cur.max_call_depth)
    .execute(&st.pool)
    .await?;
    Ok(Json(cur))
}

/// PATCH /v1/agents/{id}：缺省字段不改。已删除 / 别人的 Agent → 404。
/// 带 host_id → 400 host_immutable（宿主在添加时选定，不能换）。
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
    let name = match &req.name {
        Some(n) if !valid_name(n) => return Err(AppError::bad_request("bad_name")),
        Some(n) => Some(n.clone()),
        None => None,
    };
    // runtime 与 cwd 各自缺省 = 不改，避免只改命令时把工作目录清掉
    let new_runtime = match &req.runtime {
        Some(rt) => Some(normalize_runtime(rt, "")?.0),
        None => None,
    };
    let new_cwd = match &req.cwd {
        Some(cwd) => {
            let cwd = cwd.trim().to_string();
            if cwd.chars().count() > 500 {
                return Err(AppError::bad_request("bad_runtime"));
            }
            Some(cwd)
        }
        None => None,
    };
    let extra_dirs = match &req.extra_dirs {
        Some(d) => Some(check_extra_dirs(d)?),
        None => None,
    };
    let model = match &req.model {
        Some(m) => Some(check_token(m, "bad_model")?),
        None => None,
    };
    let mode = match &req.mode {
        Some(m) => Some(check_token(m, "bad_mode")?),
        None => None,
    };
    if let Some(s) = &req.instructions {
        check_instructions(s)?;
    }
    let env = match &req.env {
        Some(v) => Some(check_env(v)?),
        None => None,
    };
    let permission_tier = match req.permission_tier.as_deref() {
        Some(t) => Some(normalize_tier(Some(t))?),
        None => None,
    };

    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, (Uuid, Option<DateTime<Utc>>)>(
        "SELECT owner_id, deleted_at FROM agents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    // 已删除与不存在、别人的 Agent 一样 404，不暴露存在性
    if !matches!(&row, Some((owner, None)) if *owner == uid) {
        return Err(AppError::not_found("agent not found"));
    }
    if req.host_id {
        return Err(AppError::bad_request("host_immutable"));
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
    if let Some(n) = name {
        match sqlx::query("UPDATE agents SET name = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(n)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.constraint() == Some("uq_agents_owner_name") => {
                return Err(AppError::conflict("name_taken"));
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(runtime) = new_runtime {
        sqlx::query("UPDATE agents SET runtime = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(runtime)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(cwd) = new_cwd {
        sqlx::query("UPDATE agents SET cwd = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(cwd)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(dirs) = extra_dirs {
        sqlx::query("UPDATE agents SET extra_dirs = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(dirs)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(m) = model {
        sqlx::query("UPDATE agents SET model = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(m) = mode {
        sqlx::query("UPDATE agents SET mode = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(s) = req.instructions {
        sqlx::query("UPDATE agents SET instructions = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(s)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(env) = env {
        sqlx::query("UPDATE agents SET env = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(env)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(tier) = permission_tier {
        sqlx::query("UPDATE agents SET permission_tier = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(tier)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(load_agent_view(&st, id).await?))
}

async fn load_agent_view(st: &SharedState, id: Uuid) -> AppResult<AgentView> {
    let row = sqlx::query_as::<_, AgentDbRow>(&format!("{AGENT_SELECT} WHERE a.id = $1"))
        .bind(id)
        .fetch_one(&st.pool)
        .await?;
    Ok(agent_view(st, row))
}

/// DELETE /v1/agents/{id}：软删除。进行中的调用改 failed/agent_deleted，并通知宿主取消。
pub async fn delete_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, (Uuid, Uuid, Option<DateTime<Utc>>)>(
        "SELECT owner_id, host_id, deleted_at FROM agents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner, host_id, None)) = row else {
        return Err(AppError::not_found("agent not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("agent not found"));
    }
    sqlx::query("UPDATE agents SET deleted_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = 'failed', error = 'agent_deleted',
                finished_at = now()
          WHERE agent_id = $1 AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    for inv in &invs {
        let cancel = json!({"type": "invocation_cancel", "invocation_id": inv.id}).to_string();
        st.send_to_host(host_id, &cancel);
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_else(|_| vec![owner]);
        let members = with_root_members(&st, &members, inv).await;
        push_invocation(&st, &members, inv);
        crate::approvals::expire_invocation(&st, inv.id).await;
    }
    // 删掉的 Agent 自己的调用已失败；它派出的未结束子调用一并取消
    let roots: Vec<Uuid> = invs.iter().map(|i| i.id).collect();
    cascade_cancel(&st, &roots, None).await;
    // 会话列表要带上 deleted；Agent 列表面板重拉
    crate::api::notify(&st, &[owner], "conversations");
    crate::api::notify(&st, &[owner], "agents");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /v1/agents/{id}/probe：宿主在线则下发 agent_probe，202；离线 409。
pub async fn probe_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let uid = uid(&headers, &st).await?;
    let cfg = load_cfg(&st.pool, id)
        .await?
        .filter(|a| a.owner == uid && a.deleted_at.is_none())
        .ok_or_else(|| AppError::not_found("agent not found"))?;
    let payload = json!({
        "type": "agent_probe",
        "agent": agent_snapshot(&cfg),
    })
    .to_string();
    if !st.host_online(cfg.host_id) || !st.send_to_host(cfg.host_id, &payload) {
        return Err(AppError::conflict("host_offline"));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true}))))
}

/// 宿主上报 agent_options：写入 reported_options，并让主人重拉 Agent 列表。
pub async fn handle_options(st: &SharedState, host_id: Uuid, v: &Value) {
    let Some(agent_id) = v["agent_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
        return;
    };
    let stored = clean_reported(v);
    let owner = sqlx::query_scalar::<_, Uuid>(
        "UPDATE agents SET reported_options = $2, updated_at = now()
          WHERE id = $1 AND host_id = $3 AND deleted_at IS NULL
         RETURNING owner_id",
    )
    .bind(agent_id)
    .bind(&stored)
    .bind(host_id)
    .fetch_optional(&st.pool)
    .await;
    if let Ok(Some(owner)) = owner {
        crate::api::notify(st, &[owner], "agents");
    }
}

fn clean_reported(v: &Value) -> Value {
    json!({
        "modes": clean_option_list(&v["modes"]),
        "models": clean_option_list(&v["models"]),
    })
}

fn clean_option_list(v: &Value) -> Vec<Value> {
    v.as_array()
        .map(|a| {
            a.iter()
                .take(100)
                .filter_map(|item| {
                    let id: String = item["id"].as_str()?.chars().take(200).collect();
                    if id.is_empty() {
                        return None;
                    }
                    let name: String = item["name"]
                        .as_str()
                        .unwrap_or(&id)
                        .chars()
                        .take(200)
                        .collect();
                    let name = if name.is_empty() { id.clone() } else { name };
                    Some(json!({"id": id, "name": name}))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
pub(crate) struct AvatarQuery {
    v: Option<i32>,
}

/// PUT /v1/agents/{id}/avatar：PNG / JPEG / WebP，规则与用户头像相同。成功版本 +1。
pub async fn put_agent_avatar(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    mut mp: Multipart,
) -> AppResult<Json<AgentView>> {
    let uid = uid(&headers, &st).await?;
    let mut buf: Vec<u8> = Vec::new();
    let mut got_file = false;
    let mut too_large = false;
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("bad multipart: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        got_file = true;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::bad_request(format!("read error: {e}")))?
        {
            if too_large {
                continue;
            }
            buf.extend_from_slice(&chunk);
            if buf.len() > crate::api::AVATAR_MAX_BYTES {
                too_large = true;
                buf.clear();
            }
        }
    }
    if too_large {
        return Err(AppError::bad_request("avatar too large"));
    }
    if !got_file || buf.is_empty() {
        return Err(AppError::bad_request("missing file"));
    }
    let img =
        crate::api::sniff_avatar(&buf).ok_or_else(|| AppError::bad_request("unsupported image"))?;

    let cur = sqlx::query_as::<_, (Uuid, i32, Option<String>, Option<DateTime<Utc>>)>(
        "SELECT owner_id, avatar_version, avatar_path, deleted_at FROM agents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some((owner, version, old_path, None)) = cur else {
        return Err(AppError::not_found("agent not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("agent not found"));
    }
    let next = version + 1;
    let stored = format!(
        "{id}-{next}-{}.{}",
        &Uuid::new_v4().simple().to_string()[..8],
        img.ext
    );
    let dir = std::path::Path::new(&st.cfg.files_dir).join("avatars");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("create avatar dir: {e}")))?;
    let path = dir.join(&stored);
    if let Err(e) = tokio::fs::write(&path, &buf).await {
        return Err(AppError::internal(format!("store avatar: {e}")));
    }
    let updated = sqlx::query(
        "UPDATE agents SET avatar_path = $2, avatar_version = avatar_version + 1, updated_at = now()
          WHERE id = $1 AND avatar_version = $3 AND deleted_at IS NULL AND owner_id = $4",
    )
    .bind(id)
    .bind(&stored)
    .bind(version)
    .bind(uid)
    .execute(&st.pool)
    .await?;
    if updated.rows_affected() == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::conflict("avatar changed, retry"));
    }
    crate::api::spawn_avatar_cleanup(&st, old_path);
    crate::api::notify(&st, &[uid], "agents");
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(load_agent_view(&st, id).await?))
}

/// GET /v1/agents/{id}/avatar：无需鉴权。v 等于当前版本才长期缓存。已删除的 Agent 头像仍可取。
pub(crate) async fn get_agent_avatar(
    State(st): State<SharedState>,
    Path(id): Path<Uuid>,
    Query(q): Query<AvatarQuery>,
) -> AppResult<impl IntoResponse> {
    let row = sqlx::query_as::<_, (Option<String>, i32)>(
        "SELECT avatar_path, avatar_version FROM agents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("agent not found"))?;
    let (path, version) = row;
    let stored = path.ok_or_else(|| AppError::not_found("no avatar"))?;
    if stored.contains('/') || stored.contains('\\') || stored.contains("..") {
        return Err(AppError::not_found("no avatar"));
    }
    let data = tokio::fs::read(crate::api::avatar_file_path(&st.cfg.files_dir, &stored))
        .await
        .map_err(|_| AppError::not_found("no avatar"))?;
    let mime = crate::api::sniff_avatar(&data)
        .map(|img| img.mime)
        .unwrap_or("application/octet-stream");
    let cache = if q.v == Some(version) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        data,
    ))
}

#[derive(Deserialize)]
pub struct InvQuery {
    limit: Option<i64>,
}

/// 子调用成功帧 / 历史里带的回复摘要。按 Unicode 标量截断，和库里的 left() 对齐。
const RESULT_PREVIEW_CHARS: usize = 2000;

fn result_preview_text(parent: Option<Uuid>, status: &str, text: &str) -> Option<String> {
    if parent.is_none() || status != "succeeded" {
        return None;
    }
    Some(text.chars().take(RESULT_PREVIEW_CHARS).collect())
}

/// GET /conversations/{id}/agent-invocations：刷新后找回「跑完了吗」。
/// 仅成员可读（专属会话成员=主人）；按 created_at DESC、limit 默认 50。
/// 同时返回 root_conversation_id 等于该会话的子调用，刷新后能把嵌套卡片补回来。
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
    let mut rows = sqlx::query_as::<_, InvocationView>(
        "SELECT i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                i.status, i.error, i.attachment_ids, i.created_at, i.started_at, i.finished_at,
                i.parent_invocation_id, i.root_conversation_id, a.name AS agent_name,
                CASE
                  WHEN i.parent_invocation_id IS NOT NULL AND i.status = 'succeeded'
                  THEN left(i.result_text, 2000)
                  ELSE NULL
                END AS result_preview
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.conversation_id = $1 OR i.root_conversation_id = $1
          ORDER BY i.created_at DESC, i.id DESC LIMIT $2",
    )
    .bind(conv)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&st.pool)
    .await?;
    // A03：running 调用的 current_step 只存内存，历史读取时回填
    for inv in &mut rows {
        if inv.finished_at.is_none() {
            inv.current_step = st
                .inv_runs
                .get(&inv.id)
                .and_then(|r| r.open.last().map(|(_, t, _)| t.clone()));
        }
    }
    Ok(Json(rows))
}

/// POST /conversations/{id}/agent-context/reset（专属会话，或请求者自己的 Agent 在私聊 / 群里的上下文）
/// 把对应上下文的运行时会话清掉，代次加一。仅成员；非 agent 会话 400；没有 Agent 也 204。
pub async fn reset_context(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    domain::membership(&st.pool, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    let row = sqlx::query_as::<_, (String, Option<Uuid>)>(
        "SELECT type, agent_id FROM conversations WHERE id = $1",
    )
    .bind(conv)
    .fetch_optional(&st.pool)
    .await?;
    let Some((ty, agent_id)) = row else {
        return Err(AppError::bad_request("not an agent conversation"));
    };
    match (ty.as_str(), agent_id) {
        // 专属会话：只动它那个 Agent、键为本会话的上下文。schedule:<id> 不在这里。
        ("agent", Some(agent_id)) => {
            clear_context(&st.pool, agent_id, &conv.to_string()).await?;
        }
        // 私聊 / 群：只清请求者自己的 Agent 在本会话的上下文，别人的 Agent 不受影响
        ("dm" | "group", _) => {
            clear_own_contexts(&st.pool, uid, &conv.to_string()).await?;
        }
        _ => return Err(AppError::bad_request("not an agent conversation")),
    }
    Ok(StatusCode::NO_CONTENT)
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
    let inv = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'cancelled', error = NULL, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
            AND status IN ('queued', 'running', 'awaiting_approval')
         {INV_RETURNING}"
    ))
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
        st.inv_runs.remove(&id);
    }
    let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
    let members = with_root_members(&st, &members, &inv).await;
    push_invocation(&st, &members, &inv);
    if status == "running" || status == "awaiting_approval" {
        crate::approvals::expire_invocation(&st, id).await;
    }
    // 未结束的后代一并取消（排队的也算）
    cascade_cancel(&st, &[id], None).await;
    Ok(Json(inv))
}

// ---------- 使用权收回（A08） ----------

#[derive(sqlx::FromRow)]
struct RevokedInv {
    id: Uuid,
    agent_id: Uuid,
    conversation_id: Uuid,
    trigger_message_id: Option<Uuid>,
    schedule_id: Option<Uuid>,
    status: String,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    parent_invocation_id: Option<Uuid>,
    root_conversation_id: Option<Uuid>,
    agent_name: String,
    host_id: Uuid,
}

/// 进行中的调用改为 cancelled/agent_access_revoked，推帧，通知宿主取消，并断开这些用户的宿主。
pub async fn revoke_access(st: &SharedState, user_ids: &[Uuid]) {
    if user_ids.is_empty() {
        return;
    }
    let invs = sqlx::query_as::<_, RevokedInv>(
        "UPDATE agent_invocations i
            SET status = 'cancelled', error = 'agent_access_revoked', finished_at = now()
           FROM agents a
          WHERE i.agent_id = a.id
            AND a.owner_id = ANY($1)
            AND i.finished_at IS NULL
            AND i.status IN ('queued', 'running', 'awaiting_approval')
         RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                   i.status, i.error, i.created_at, i.started_at, i.finished_at,
                   i.parent_invocation_id, i.root_conversation_id, a.name AS agent_name, a.host_id",
    )
    .bind(user_ids)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "revoke agent access failed");
        Vec::new()
    });
    for inv in &invs {
        let payload = json!({
            "type": "invocation_cancel",
            "invocation_id": inv.id,
        })
        .to_string();
        st.send_to_host(inv.host_id, &payload);
        let view = InvocationView {
            id: inv.id,
            agent_id: inv.agent_id,
            conversation_id: inv.conversation_id,
            trigger_message_id: inv.trigger_message_id,
            schedule_id: inv.schedule_id,
            attachment_ids: Vec::new(),
            status: inv.status.clone(),
            error: inv.error.clone(),
            created_at: inv.created_at,
            started_at: inv.started_at,
            finished_at: inv.finished_at,
            current_step: None,
            parent_invocation_id: inv.parent_invocation_id,
            root_conversation_id: inv.root_conversation_id,
            agent_name: inv.agent_name.clone(),
            result_preview: None,
        };
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &view).await;
        push_invocation(st, &members, &view);
        crate::approvals::expire_invocation(st, inv.id).await;
    }
    let roots: Vec<Uuid> = invs.iter().map(|i| i.id).collect();
    cascade_cancel(st, &roots, Some("agent_access_revoked")).await;
    for uid in user_ids {
        st.disconnect_user_hosts(*uid);
    }
}

/// 全局开关或默认变了：谁的生效值从有到无，就收回；变了的人重拉 /users/me。
pub async fn on_settings_saved(
    st: &SharedState,
    before: &crate::settings::ServerSettings,
    after: &crate::settings::ServerSettings,
) {
    if before.agent_enabled == after.agent_enabled
        && before.agent_default_allowed == after.agent_default_allowed
    {
        return;
    }
    let rows = match sqlx::query_as::<_, (Uuid, String, Option<bool>)>(
        "SELECT id, role, agent_access FROM users",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "agent access policy diff failed");
            return;
        }
    };
    let mut lost = Vec::new();
    let mut changed = Vec::new();
    for (id, role, ov) in rows {
        let old = crate::agent_access::effective(
            &role,
            before.agent_enabled,
            before.agent_default_allowed,
            ov,
        );
        let new = crate::agent_access::effective(
            &role,
            after.agent_enabled,
            after.agent_default_allowed,
            ov,
        );
        if old != new {
            changed.push(id);
        }
        if old && !new {
            lost.push(id);
        }
    }
    revoke_access(st, &lost).await;
    if !changed.is_empty() {
        crate::api::notify(st, &changed, "me");
    }
}

// ---------- 调用派发（挂在 broadcast_posted 上） ----------

#[derive(Clone)]
struct AgentRow {
    id: Uuid,
    owner: Uuid,
    host_id: Uuid,
    name: String,
    runtime: Value,
    cwd: String,
    extra_dirs: Vec<String>,
    model: String,
    mode: String,
    instructions: String,
    env: Value,
    permission_tier: String,
    deleted_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct AgentCfgRow {
    id: Uuid,
    owner_id: Option<Uuid>,
    host_id: Uuid,
    name: String,
    runtime: Value,
    cwd: String,
    extra_dirs: Vec<String>,
    model: String,
    mode: String,
    instructions: String,
    env: Value,
    permission_tier: String,
    deleted_at: Option<DateTime<Utc>>,
}

fn row_to_agent(r: AgentCfgRow) -> AgentRow {
    AgentRow {
        id: r.id,
        owner: r.owner_id.unwrap_or_default(),
        host_id: r.host_id,
        name: r.name,
        runtime: r.runtime,
        cwd: r.cwd,
        extra_dirs: r.extra_dirs,
        model: r.model,
        mode: r.mode,
        instructions: r.instructions,
        env: r.env,
        permission_tier: r.permission_tier,
        deleted_at: r.deleted_at,
    }
}

const AGENT_CFG_SQL: &str = "SELECT a.id, a.owner_id, a.host_id, a.name, a.runtime, a.cwd,
        a.extra_dirs, a.model, a.mode, a.instructions, a.env, a.permission_tier, a.deleted_at";

async fn load_cfg(pool: &PgPool, id: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, AgentCfgRow>(&format!("{AGENT_CFG_SQL} FROM agents a WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|r| r.map(row_to_agent))
}

/// 一条语句读出运行时会话和代次。没有行 = 新会话、代次 0。空串会话也当没有。
/// 查询失败同样当新会话，避免把读失败变成「续上一个没拿到的旧会话」。
async fn read_context_epoch(
    pool: &PgPool,
    agent_id: Uuid,
    context_key: &str,
) -> (Option<String>, i64) {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT runtime_session_id, epoch FROM agent_contexts
          WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(agent_id)
    .bind(context_key)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some((sid, epoch))) => {
            let session = if sid.is_empty() { None } else { Some(sid) };
            (session, epoch)
        }
        Ok(None) => (None, 0),
        Err(e) => {
            tracing::error!(error = %e, "读取 Agent 上下文失败");
            (None, 0)
        }
    }
}

/// 下发 invocation_start 之前把这次读到的代次记在调用行上。完成时只认这一列。
async fn stamp_context_epoch(pool: &PgPool, inv_id: Uuid, epoch: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE agent_invocations SET context_epoch = $2 WHERE id = $1")
        .bind(inv_id)
        .bind(epoch)
        .execute(pool)
        .await?;
    Ok(())
}

/// 开新话题：没有行就插入空会话、代次 1；有行则清空会话并把代次加一。
async fn clear_context(pool: &PgPool, agent_id: Uuid, context_key: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO agent_contexts (agent_id, context_key, runtime_session_id, epoch, updated_at)
         VALUES ($1, $2, '', 1, now())
         ON CONFLICT (agent_id, context_key)
         DO UPDATE SET runtime_session_id = '',
                       epoch = agent_contexts.epoch + 1,
                       updated_at = now()",
    )
    .bind(agent_id)
    .bind(context_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// 私聊 / 群：请求者名下每个 Agent 各清一条，同一条语句完成。
async fn clear_own_contexts(pool: &PgPool, owner: Uuid, context_key: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO agent_contexts (agent_id, context_key, runtime_session_id, epoch, updated_at)
         SELECT id, $2, '', 1, now() FROM agents WHERE owner_id = $1
         ON CONFLICT (agent_id, context_key)
         DO UPDATE SET runtime_session_id = '',
                       epoch = agent_contexts.epoch + 1,
                       updated_at = now()",
    )
    .bind(owner)
    .bind(context_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// 成功的 finish 才写回。代次来自调用行（不是内存）。
/// NULL 是迁移前派发的老调用，照常覆盖。有代次则只在上下文代次仍相等时覆盖，且不改代次本身。
async fn persist_runtime_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: Uuid,
    context_key: &str,
    session_id: &str,
    context_epoch: Option<i64>,
) -> Result<(), sqlx::Error> {
    match context_epoch {
        Some(epoch) => {
            sqlx::query(
                "INSERT INTO agent_contexts
                     (agent_id, context_key, runtime_session_id, epoch, updated_at)
                 VALUES ($1, $2, $3, $4, now())
                 ON CONFLICT (agent_id, context_key)
                 DO UPDATE SET runtime_session_id = EXCLUDED.runtime_session_id,
                               updated_at = now()
                 WHERE agent_contexts.epoch = $4",
            )
            .bind(agent_id)
            .bind(context_key)
            .bind(session_id)
            .bind(epoch)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO agent_contexts
                     (agent_id, context_key, runtime_session_id, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (agent_id, context_key)
                 DO UPDATE SET runtime_session_id = EXCLUDED.runtime_session_id,
                               updated_at = now()",
            )
            .bind(agent_id)
            .bind(context_key)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

async fn agent_of_conv(pool: &PgPool, conv: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, AgentCfgRow>(&format!(
        "{AGENT_CFG_SQL}
           FROM conversations c JOIN agents a ON a.id = c.agent_id
          WHERE c.id = $1 AND c.type = 'agent'"
    ))
    .bind(conv)
    .fetch_optional(pool)
    .await
    .map(|r| r.map(row_to_agent))
}

/// invocation_start / agent_probe 共用的配置快照。改配置在下一次调用生效。
fn agent_snapshot(a: &AgentRow) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "runtime": a.runtime,
        "cwd": a.cwd,
        "extra_dirs": a.extra_dirs,
        "model": a.model,
        "mode": a.mode,
        "instructions": a.instructions,
        "env": a.env,
        "permission_tier": a.permission_tier,
    })
}

/// 新文字消息落库后的唯一挂点。
/// 专属会话：主人发的普通文字调用它的 Agent（prompt 用全文）。
/// 私聊 / 群：显式 agent_targets（属于发送者且未删除）∪ 手打 @token
/// （先匹配发送者自己的 Agent，不区分大小写）。别人的 id 与非主人的同名都不触发。
/// Agent 自己的回写与幂等重放不触发。
pub async fn maybe_invoke(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    if !posted.inserted || posted.message.agent.is_some() || posted.message.kind != "text" {
        return;
    }
    match agent_of_conv(&st.pool, conv).await {
        Ok(Some(agent)) => {
            // 只有主人本人的消息触发；专属会话唯一成员即主人，显式校验防结构漂移
            if agent.deleted_at.is_some() || posted.message.sender_id != agent.owner {
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
        Ok(None) => {
            invoke_in_place(st, conv, posted).await;
        }
        Err(e) => tracing::error!(error = %e, "读取会话 Agent 失败"),
    }
}

/// 私聊 / 群：为每个解析出的 Agent 建一次调用。prompt 去掉用来点名的 @token。
async fn invoke_in_place(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    let kind: Option<String> = sqlx::query_scalar("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
    if kind.as_deref() != Some("dm") && kind.as_deref() != Some("group") {
        return;
    }
    let mine = sender_agents(&st.pool, posted.message.sender_id).await;
    let targets = resolve_agent_targets(&mine, &posted.message.content, &posted.agent_targets);
    if targets.is_empty() {
        return;
    }
    let names: Vec<String> = targets.iter().map(|a| a.name.clone()).collect();
    let prompt = prompt_without_agent_tokens(&posted.message.content, &names);
    for agent in targets {
        dispatch(
            st,
            agent,
            conv,
            posted.message.id,
            prompt.clone(),
            &posted.member_ids,
        )
        .await;
    }
}

async fn sender_agents(pool: &PgPool, owner: Uuid) -> Vec<AgentRow> {
    sqlx::query_as::<_, AgentCfgRow>(&format!(
        "{AGENT_CFG_SQL}
           FROM agents a
          WHERE a.owner_id = $1 AND a.deleted_at IS NULL
          ORDER BY a.created_at, a.id"
    ))
    .bind(owner)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(row_to_agent)
    .collect()
}

/// 显式 id 在前，手打且尚未指定的 @token 在后。同 id 只留一次。
fn resolve_agent_targets(mine: &[AgentRow], content: &str, explicit: &[Uuid]) -> Vec<AgentRow> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in explicit {
        if let Some(agent) = mine.iter().find(|a| a.id == *id) {
            if seen.insert(agent.id) {
                out.push(agent.clone());
            }
        }
    }
    let taken: std::collections::HashSet<String> =
        out.iter().map(|a| a.name.to_ascii_lowercase()).collect();
    for tok in domain::scan_at_tokens(content) {
        if taken.contains(&tok.name.to_ascii_lowercase()) {
            continue;
        }
        if let Some(agent) = mine.iter().find(|a| a.name.eq_ignore_ascii_case(&tok.name)) {
            if seen.insert(agent.id) {
                out.push(agent.clone());
            }
        }
    }
    out
}

/// 去掉点名用的 @Agent（及紧跟的一个空格）。剥完只剩空白时退回原文。
fn prompt_without_agent_tokens(content: &str, names: &[String]) -> String {
    let tokens = domain::scan_at_tokens(content);
    let mut out = String::new();
    let mut cursor = 0;
    for tok in &tokens {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&tok.name)) {
            continue;
        }
        out.push_str(&content[cursor..tok.start]);
        let mut next = tok.end;
        if content.as_bytes().get(next) == Some(&b' ') {
            next += 1;
        }
        cursor = next;
    }
    out.push_str(&content[cursor..]);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        content.to_string()
    } else {
        trimmed.to_string()
    }
}

async fn dispatch(
    st: &SharedState,
    agent: AgentRow,
    conv: Uuid,
    trigger: Uuid,
    prompt: String,
    member_ids: &[Uuid],
) {
    // 专属会话在落库前就拒绝了。群 / 私聊里的 @ 走到这里时只是不建调用。
    match crate::agent_access::user_allows(&st.pool, agent.owner).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::error!(error = %e, "agent access check failed");
            return;
        }
    }
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    // A05：待交付附件的收集与调用入库放进同一事务、同一把会话行锁——
    // 并发触发的两条文字消息各自派发时，附件按 seq 唯一地分给其中一次。
    let inv: Result<InvocationView, sqlx::Error> = async {
        let mut tx = st.pool.begin().await?;
        let _ = domain::lock_conversation(&mut tx, conv).await?;
        let atts = collect_pending_attachments(&mut tx, conv, agent.owner, trigger).await?;
        let inv = if online {
            insert_running(
                &mut *tx,
                &agent,
                conv,
                Some(trigger),
                None,
                &atts,
                &InvLink::default(),
            )
            .await?
        } else {
            insert_queued(
                &mut *tx,
                &agent,
                conv,
                Some(trigger),
                None,
                timeout,
                &atts,
                &InvLink::default(),
            )
            .await?
        };
        tx.commit().await?;
        Ok(inv)
    }
    .await;
    // 在线先标 running 再下发，客户端看到的第一帧就是 running（不是先闪一下排队）。
    // 发不出去则改回 queued，与离线同语义。
    let Ok(inv) = inv else {
        tracing::error!("agent_invocation insert failed");
        return;
    };
    let key = conv.to_string();
    deliver_invocation(st, &agent, inv, &prompt, &key, member_ids).await;
}

/// 定时调用到点后的派发。context_key = schedule:<id>，没有触发消息。
pub(crate) struct ScheduleFire {
    pub schedule_id: Uuid,
    pub agent_id: Uuid,
    pub conversation_id: Uuid,
    pub prompt: String,
}

pub(crate) async fn fire_schedule(st: &SharedState, job: ScheduleFire) {
    let agent = match load_cfg(&st.pool, job.agent_id).await {
        Ok(Some(a)) if a.deleted_at.is_none() => a,
        _ => {
            tracing::info!(schedule_id = %job.schedule_id, "定时调用跳过：Agent 已删除");
            return;
        }
    };
    let members = domain::member_ids(&st.pool, job.conversation_id)
        .await
        .unwrap_or_default();
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    let link = InvLink::default();
    let inv = if online {
        insert_running(
            &st.pool,
            &agent,
            job.conversation_id,
            None,
            Some(job.schedule_id),
            &[],
            &link,
        )
        .await
    } else {
        insert_queued(
            &st.pool,
            &agent,
            job.conversation_id,
            None,
            Some(job.schedule_id),
            timeout,
            &[],
            &link,
        )
        .await
    };
    let Ok(inv) = inv else {
        tracing::error!(schedule_id = %job.schedule_id, "定时调用插入失败");
        return;
    };
    let key = format!("schedule:{}", job.schedule_id);
    deliver_invocation(st, &agent, inv, &job.prompt, &key, &members).await;
}

async fn deliver_invocation(
    st: &SharedState,
    agent: &AgentRow,
    inv: InvocationView,
    prompt: &str,
    context_key: &str,
    member_ids: &[Uuid],
) {
    // 子调用的状态帧和流式帧都要带到根会话成员（被调方专属会话之外的人）
    let member_ids = with_root_members(st, member_ids, &inv).await;
    push_invocation(st, &member_ids, &inv);
    if inv.status != "running" {
        return;
    }
    // 一条 SELECT 读会话和代次，先写进调用行，再下发 invocation_start。
    let (session_id, epoch) = read_context_epoch(&st.pool, agent.id, context_key).await;
    if let Err(e) = stamp_context_epoch(&st.pool, inv.id, epoch).await {
        tracing::error!(error = %e, "记下调用上下文代次失败");
    }
    register_run(st, agent, &inv, &member_ids).await;
    let payload = invocation_start_payload(st, &inv, agent, context_key, prompt, session_id).await;
    if !st.send_to_host(agent.host_id, &payload) {
        st.inv_runs.remove(&inv.id);
        let timeout = queue_timeout_secs(&st.pool, agent.id).await;
        if let Some(q) = requeue(&st.pool, inv.id, timeout).await {
            push_invocation(st, &member_ids, &q);
        }
    }
}

/// A03：登记进行中调用的运行时状态（输出方式、进行中步骤、流式缓冲）。
/// 必须在 invocation_start 下发前就绪——宿主事件随时可能先回来。
async fn register_run(
    st: &SharedState,
    agent: &AgentRow,
    inv: &InvocationView,
    member_ids: &[Uuid],
) {
    let mode = effective_settings(&st.pool, agent.id)
        .await
        .map(|s| s.output_mode)
        .unwrap_or_else(|_| "stream".into());
    st.inv_runs.insert(
        inv.id,
        InvRun {
            host_id: agent.host_id,
            conversation_id: inv.conversation_id,
            member_ids: member_ids.to_vec(),
            owner: agent.owner,
            agent_id: agent.id,
            inv: inv.clone(),
            mode,
            open: Vec::new(),
            steps: Vec::new(),
            stream_buf: String::new(),
            // 首个增量立即可推（窗口从派发前起算）
            last_push: Instant::now()
                .checked_sub(Duration::from_millis(STREAM_FLUSH_MS))
                .unwrap_or_else(Instant::now),
            flushing: false,
        },
    );
}

/// A05：收集主人的待交付 image/file 附件，上限 10。
/// 「待交付」= 触发消息之前落库、且未被本会话任何调用带走（attachment_ids 不含它）——
/// 超上限留下的附件自然留给下一次调用。在会话行锁内跑，并发触发各自拿到不重叠清单。
async fn collect_pending_attachments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    conv: Uuid,
    owner: Uuid,
    trigger: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT a.id
           FROM messages m
           JOIN attachments a ON a.id = m.attachment_id
          WHERE m.conversation_id = $1
            AND m.sender_id = $2
            AND m.agent_id IS NULL
            AND m.kind IN ('image', 'file')
            AND m.deleted_at IS NULL
            AND m.seq < (SELECT seq FROM messages WHERE id = $3)
            AND NOT EXISTS (
                  SELECT 1 FROM agent_invocations i
                   WHERE i.conversation_id = $1
                     AND a.id = ANY (i.attachment_ids)
                )
          ORDER BY m.seq
          LIMIT 10",
    )
    .bind(conv)
    .bind(owner)
    .bind(trigger)
    .fetch_all(&mut **tx)
    .await
}

/// invocation_start.attachments：按 attachment_ids 的顺序还原元数据
/// （附件行中途被删的条目直接缺席，宿主按拿到的清单下载）。
async fn attachment_meta(pool: &PgPool, ids: &[Uuid]) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    sqlx::query_as::<_, (Uuid, String, String, i64)>(
        "SELECT a.id, a.file_name, a.mime, a.size
           FROM unnest($1::uuid[]) WITH ORDINALITY u(id, ord)
           JOIN attachments a ON a.id = u.id
          ORDER BY u.ord",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, name, mime, size)| json!({"id": id, "name": name, "mime": mime, "size": size}))
    .collect()
}

async fn queue_timeout_secs(pool: &PgPool, agent_id: Uuid) -> i32 {
    effective_settings(pool, agent_id)
        .await
        .map(|s| s.queue_timeout_secs)
        .unwrap_or(86400)
}

pub(crate) const INV_RETURNING: &str =
    "RETURNING id, agent_id, conversation_id, trigger_message_id, schedule_id, status, error,
                   attachment_ids, created_at, started_at, finished_at, parent_invocation_id,
                   root_conversation_id,
                   (SELECT name FROM agents WHERE id = agent_invocations.agent_id) AS agent_name";

/// 子调用额外落库的关联。主人直接发起 / 定时调用用默认值（depth 0，无父无根）。
struct InvLink {
    parent: Option<Uuid>,
    depth: i32,
    root: Option<Uuid>,
    /// `call:<上下文号>`。空则收尾时按会话或定时任务推导。
    context_key: Option<String>,
    /// 离线补发用的完整指令。触发消息只是截断后的记录，不能当指令。
    prompt: Option<String>,
}

impl Default for InvLink {
    fn default() -> Self {
        Self {
            parent: None,
            depth: 0,
            root: None,
            context_key: None,
            prompt: None,
        }
    }
}

async fn insert_running(
    ex: impl sqlx::PgExecutor<'_>,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Option<Uuid>,
    schedule: Option<Uuid>,
    atts: &[Uuid],
    link: &InvLink,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, schedule_id, status, started_at,
             attachment_ids, parent_invocation_id, depth, root_conversation_id, context_key,
             prompt)
         VALUES ($1, $2, $3, $4, 'running', now(), $5, $6, $7, $8, $9, $10)
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(schedule)
    .bind(atts)
    .bind(link.parent)
    .bind(link.depth)
    .bind(link.root)
    .bind(&link.context_key)
    .bind(&link.prompt)
    .fetch_one(ex)
    .await
}

async fn insert_queued(
    ex: impl sqlx::PgExecutor<'_>,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Option<Uuid>,
    schedule: Option<Uuid>,
    timeout_secs: i32,
    atts: &[Uuid],
    link: &InvLink,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, schedule_id, status, queue_deadline,
             attachment_ids, parent_invocation_id, depth, root_conversation_id, context_key,
             prompt)
         VALUES ($1, $2, $3, $4, 'queued',
                 CASE WHEN $5 > 0 THEN now() + make_interval(secs => $5::double precision) END,
                 $6, $7, $8, $9, $10, $11)
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(schedule)
    .bind(timeout_secs)
    .bind(atts)
    .bind(link.parent)
    .bind(link.depth)
    .bind(link.root)
    .bind(&link.context_key)
    .bind(&link.prompt)
    .fetch_one(ex)
    .await
}

/// invocation_start 帧：Agent 配置快照（A06）+ 该会话记住的运行时会话（A02）
/// + 本次调用的工具凭据（A13，只在调用进行中有效）+ 待交付附件元数据（A05）。
/// 即时派发与宿主上线后清排队共用。
async fn invocation_start_payload(
    st: &SharedState,
    inv: &InvocationView,
    agent: &AgentRow,
    context_key: &str,
    prompt: &str,
    session_id: Option<String>,
) -> String {
    // 深度以调用行为准（主人直接发起是 0）。签发失败时不带凭据，调用照常跑。
    let depth: i32 = sqlx::query_scalar("SELECT depth FROM agent_invocations WHERE id = $1")
        .bind(inv.id)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    let tools_token = crate::auth::mint_invocation(
        inv.id,
        agent.owner,
        agent.id,
        depth as i64,
        &st.cfg.jwt_secret,
    )
    .map_err(|e| tracing::error!(error = ?e, "tools_token mint failed"))
    .ok();
    let attachments = attachment_meta(&st.pool, &inv.attachment_ids).await;
    json!({
        "type": "invocation_start",
        "invocation_id": inv.id,
        "agent": agent_snapshot(agent),
        "prompt": prompt,
        "context": {"key": context_key, "session_id": session_id},
        "tools_token": tools_token,
        "attachments": attachments,
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
    prompt: String,
    trigger_message_id: Option<Uuid>,
    schedule_id: Option<Uuid>,
    context_key: Option<String>,
    prompt_stored: bool,
}

/// 排队时 prompt 没落库。私聊 / 群上线补发时，按这条触发消息上的全部目标 Agent 再剥一次 @。
/// 专属会话保持原文，与在线派发一致。
async fn prompt_for_flushed(pool: &PgPool, conv: Uuid, trigger: Uuid, content: &str) -> String {
    let kind: Option<String> = sqlx::query_scalar("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    if kind.as_deref() == Some("agent") {
        return content.to_string();
    }
    let names = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT a.name
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.trigger_message_id = $1",
    )
    .bind(trigger)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    prompt_without_agent_tokens(content, &names)
}

/// host_ready 之后：该宿主上所有 queued 调用按 created_at 逐个改成 running 并下发。
/// 主人已没有使用权时整批跳过（进行中的应已取消，这里是兜底）。
pub async fn flush_queued(st: &SharedState, host_id: Uuid) {
    let owner = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
    if let Some(owner) = owner {
        if !crate::agent_access::user_allows(&st.pool, owner)
            .await
            .unwrap_or(false)
        {
            return;
        }
    }
    let rows = sqlx::query_as::<_, QueuedStart>(
        "SELECT i.id, i.conversation_id, a.id AS agent_id,
                COALESCE(i.prompt, m.content, s.prompt, '') AS prompt,
                i.trigger_message_id, i.schedule_id, i.context_key,
                (i.prompt IS NOT NULL) AS prompt_stored
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           LEFT JOIN messages m ON m.id = i.trigger_message_id
           LEFT JOIN agent_schedules s ON s.id = i.schedule_id
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
        let agent = match load_cfg(&st.pool, row.agent_id).await {
            Ok(Some(a)) => a,
            _ => {
                let members = with_root_members(st, &members, &inv).await;
                push_invocation(st, &members, &inv);
                continue;
            }
        };
        // 定时任务被删掉之后，排队中的调用没有指令可发。
        // 子调用把完整指令存在 prompt 列，不走这条。
        if !row.prompt_stored
            && ((row.schedule_id.is_some() && row.prompt.is_empty())
                || (row.schedule_id.is_none() && row.trigger_message_id.is_none()))
        {
            let _ =
                finish_invocation(st, row.id, "failed", Some("schedule_deleted"), &members).await;
            continue;
        }
        let prompt = if row.prompt_stored || row.schedule_id.is_some() {
            row.prompt.clone()
        } else if let Some(trigger) = row.trigger_message_id {
            prompt_for_flushed(&st.pool, row.conversation_id, trigger, &row.prompt).await
        } else {
            String::new()
        };
        let context_key = row
            .context_key
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| match row.schedule_id {
                Some(id) => format!("schedule:{id}"),
                None => row.conversation_id.to_string(),
            });
        deliver_invocation(st, &agent, inv, &prompt, &context_key, &members).await;
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
        let members = with_root_members(st, &members, &inv).await;
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
    let inv = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = $2, error = $3, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .bind(status)
    .bind(error)
    .fetch_optional(&st.pool)
    .await?;
    if let Some(inv) = &inv {
        let members = with_root_members(st, member_ids, inv).await;
        push_invocation(st, &members, inv);
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

/// 成员变更提交后重算进行中调用的接收者。
/// 命中调用会话或根会话是 `conv` 的运行；新名单是两侧当前成员的并集，
/// 所以人还留在另一侧会话时不会被拿掉。查询失败就保持原快照。
pub(crate) async fn refresh_run_members(st: &SharedState, conv: Uuid) {
    let ids: Vec<Uuid> = st
        .inv_runs
        .iter()
        .filter(|e| {
            let run = e.value();
            run.conversation_id == conv || run.inv.root_conversation_id == Some(conv)
        })
        .map(|e| *e.key())
        .collect();
    for id in ids {
        let Some(inv) = st.inv_runs.get(&id).map(|r| r.inv.clone()) else {
            continue;
        };
        if inv.conversation_id != conv && inv.root_conversation_id != Some(conv) {
            continue;
        }
        let Ok(mut members) = domain::member_ids(&st.pool, inv.conversation_id).await else {
            tracing::warn!(%conv, %id, "重算调用接收者时读成员失败");
            continue;
        };
        if let Some(root) = inv.root_conversation_id {
            if root != inv.conversation_id {
                let Ok(extra) = domain::member_ids(&st.pool, root).await else {
                    tracing::warn!(%conv, %id, "重算调用接收者时读根会话成员失败");
                    continue;
                };
                for uid in extra {
                    if !members.contains(&uid) {
                        members.push(uid);
                    }
                }
            }
        }
        if let Some(mut run) = st.inv_runs.get_mut(&id) {
            if run.conversation_id == inv.conversation_id {
                run.member_ids = members;
            }
        }
    }
}

/// 会话成员，再加上根会话里还没包括的人。根会话就是本会话时不重复查。
pub(crate) async fn with_root_members(
    st: &SharedState,
    member_ids: &[Uuid],
    inv: &InvocationView,
) -> Vec<Uuid> {
    let mut members = member_ids.to_vec();
    let Some(root) = inv.root_conversation_id else {
        return members;
    };
    if root == inv.conversation_id {
        return members;
    }
    if let Ok(extra) = domain::member_ids(&st.pool, root).await {
        for id in extra {
            if !members.contains(&id) {
                members.push(id);
            }
        }
    }
    members
}

/// 把 roots 的全部未结束后代改成 cancelled，推帧，并通知各自宿主。
/// roots 自己应已是终态（这里不再改它们）。
pub(crate) async fn cascade_cancel(st: &SharedState, roots: &[Uuid], error: Option<&str>) {
    if roots.is_empty() {
        return;
    }
    let rows = sqlx::query_as::<_, Cascaded>(
        "WITH RECURSIVE tree AS (
            SELECT id, status FROM agent_invocations
             WHERE parent_invocation_id = ANY($1) AND finished_at IS NULL
            UNION
            SELECT i.id, i.status FROM agent_invocations i
            JOIN tree t ON i.parent_invocation_id = t.id
             WHERE i.finished_at IS NULL
        )
        UPDATE agent_invocations AS i
           SET status = 'cancelled', error = $2, finished_at = now()
          FROM tree
         WHERE i.id = tree.id AND i.finished_at IS NULL
        RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                  i.status, i.error, i.created_at, i.started_at, i.finished_at,
                  i.parent_invocation_id, i.root_conversation_id,
                  (SELECT name FROM agents WHERE id = i.agent_id) AS agent_name,
                  (SELECT host_id FROM agents WHERE id = i.agent_id) AS host_id,
                  tree.status AS prev_status",
    )
    .bind(roots)
    .bind(error)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "级联取消子调用失败");
        Vec::new()
    });
    for row in rows {
        let id = row.id;
        let host_id = row.host_id;
        let conv = row.conversation_id;
        let prev = row.prev_status.clone();
        if prev == "running" || prev == "awaiting_approval" {
            let payload = json!({
                "type": "invocation_cancel",
                "invocation_id": id,
            })
            .to_string();
            st.send_to_host(host_id, &payload);
            st.inv_runs.remove(&id);
        }
        let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
        let view = row.into_view();
        let members = with_root_members(st, &members, &view).await;
        push_invocation(st, &members, &view);
        crate::approvals::expire_invocation(st, id).await;
    }
}

#[derive(sqlx::FromRow)]
struct Cascaded {
    id: Uuid,
    agent_id: Uuid,
    conversation_id: Uuid,
    trigger_message_id: Option<Uuid>,
    schedule_id: Option<Uuid>,
    status: String,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    parent_invocation_id: Option<Uuid>,
    root_conversation_id: Option<Uuid>,
    agent_name: String,
    host_id: Uuid,
    prev_status: String,
}

impl Cascaded {
    fn into_view(self) -> InvocationView {
        InvocationView {
            id: self.id,
            agent_id: self.agent_id,
            conversation_id: self.conversation_id,
            trigger_message_id: self.trigger_message_id,
            schedule_id: self.schedule_id,
            attachment_ids: Vec::new(),
            status: self.status,
            error: self.error,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            current_step: None,
            parent_invocation_id: self.parent_invocation_id,
            root_conversation_id: self.root_conversation_id,
            agent_name: self.agent_name,
            result_preview: None,
        }
    }
}

/// 工具接口发起的子调用。找不到主人名下未删除的同名 Agent → not found。
pub(crate) struct ChildReq {
    pub owner: Uuid,
    pub parent_id: Uuid,
    pub parent_depth: i32,
    pub parent_conversation: Uuid,
    pub parent_root: Option<Uuid>,
    pub caller_name: String,
    pub agent_name: String,
    pub prompt: String,
    pub context_id: Option<Uuid>,
}

pub(crate) struct ChildRes {
    pub invocation_id: Uuid,
    pub context_id: Uuid,
    pub status: String,
}

pub(crate) async fn start_child_call(st: &SharedState, req: ChildReq) -> AppResult<ChildRes> {
    if req.prompt.is_empty() || req.prompt.chars().count() > 4000 {
        return Err(AppError::bad_request("bad_prompt"));
    }
    if !crate::agent_access::user_allows(&st.pool, req.owner).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    let found = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT a.id, c.id
           FROM agents a
           JOIN conversations c ON c.agent_id = a.id AND c.type = 'agent'
          WHERE a.owner_id = $1 AND a.deleted_at IS NULL AND lower(a.name) = lower($2)",
    )
    .bind(req.owner)
    .bind(&req.agent_name)
    .fetch_optional(&st.pool)
    .await?;
    let Some((callee_id, callee_conv)) = found else {
        return Err(AppError::not_found("agent not found"));
    };
    let max_depth: i32 =
        sqlx::query_scalar("SELECT max_call_depth FROM agent_user_settings WHERE user_id = $1")
            .bind(req.owner)
            .fetch_optional(&st.pool)
            .await?
            .unwrap_or(3);
    let depth = req.parent_depth.saturating_add(1);
    if depth > max_depth {
        return Err(AppError::bad_request("depth_exceeded"));
    }
    let agent = load_cfg(&st.pool, callee_id)
        .await?
        .filter(|a| a.deleted_at.is_none())
        .ok_or_else(|| AppError::not_found("agent not found"))?;
    let context_id = req.context_id.unwrap_or_else(Uuid::new_v4);
    let context_key = format!("call:{context_id}");
    let root = req.parent_root.unwrap_or(req.parent_conversation);
    let excerpt: String = req.prompt.chars().take(200).collect();
    let record = format!("📨 被 @{} 调用：{excerpt}", req.caller_name);
    let posted = domain::post_message(
        &st.pool,
        domain::PostMessage {
            conversation_id: callee_conv,
            sender_id: req.owner,
            content: &record,
            client_msg_id: None,
            attachment_id: None,
            reply_to_id: None,
            poll: None,
            forwarded_from: None,
            forward_attachment: None,
            agent_id: Some(agent.id),
            agent_steps: None,
            approval: None,
        },
    )
    .await
    .map_err(|e| match e {
        PostError::Deny { message, .. } => AppError::bad_request(message),
        PostError::Db(err) => AppError::internal(err),
    })?;
    let trigger = posted.message.id;
    crate::ws::broadcast_posted(st, callee_conv, &posted).await;

    let link = InvLink {
        parent: Some(req.parent_id),
        depth,
        root: Some(root),
        context_key: Some(context_key.clone()),
        prompt: Some(req.prompt.clone()),
    };
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    let inv = if online {
        insert_running(
            &st.pool,
            &agent,
            callee_conv,
            Some(trigger),
            None,
            &[],
            &link,
        )
        .await?
    } else {
        insert_queued(
            &st.pool,
            &agent,
            callee_conv,
            Some(trigger),
            None,
            timeout,
            &[],
            &link,
        )
        .await?
    };
    let members = domain::member_ids(&st.pool, callee_conv)
        .await
        .unwrap_or_default();
    let inv_id = inv.id;
    deliver_invocation(st, &agent, inv, &req.prompt, &context_key, &members).await;
    let status: String = sqlx::query_scalar("SELECT status FROM agent_invocations WHERE id = $1")
        .bind(inv_id)
        .fetch_one(&st.pool)
        .await?;
    Ok(ChildRes {
        invocation_id: inv_id,
        context_id,
        status,
    })
}

// ---------- Agent A03：宿主事件流（invocation_event） ----------

/// stream 增量的最小推送间隔：同一次调用每 200ms 至多一帧 delta
const STREAM_FLUSH_MS: u64 = 200;

/// 宿主回传的 invocation_event：只受理派给本宿主连接的进行中调用；
/// 事件类型不明/字段残缺一律忽略（不致命，不影响后续事件）。
pub async fn handle_event(st: &SharedState, host_id: Uuid, v: &serde_json::Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let ev = &v["event"];
    // steps 模式的步骤消息要进 DashMap 守卫外写库
    let mut step_msg: Option<(Uuid, Uuid, Uuid, String)> = None;
    {
        let Some(mut run) = st.inv_runs.get_mut(&inv_id) else {
            return; // 未知/已终结的调用：伪造或迟到的事件
        };
        if run.host_id != host_id {
            return;
        }
        match ev["type"].as_str() {
            Some("text") => {
                if let Some(t) = ev["text"].as_str() {
                    if !t.is_empty() && run.mode == "stream" {
                        run.stream_buf.push_str(t);
                        maybe_flush_stream(st, &mut run, inv_id);
                    }
                }
            }
            Some("step_start") => {
                let id = ev["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    return;
                }
                let title: String = ev["title"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                let kind: String = ev["kind"]
                    .as_str()
                    .unwrap_or("other")
                    .chars()
                    .take(40)
                    .collect();
                // 同 id 重复 start：去掉旧记录，以最新一次为准
                run.open.retain(|(x, _, _)| x != &id);
                run.open.push((id.clone(), title.clone(), kind.clone()));
                match run.mode.as_str() {
                    "stream" => push_stream(
                        st,
                        &mut run,
                        inv_id,
                        Some(StreamStepView {
                            id,
                            title,
                            kind,
                            ok: None,
                        }),
                    ),
                    "status" => push_current_step(st, &run),
                    _ => {}
                }
            }
            Some("step_end") => {
                let id = ev["id"].as_str().unwrap_or_default();
                let ok = ev["ok"].as_bool().unwrap_or(false);
                // 未知或重复的 step_end（没开过这步）忽略
                let Some(pos) = run.open.iter().position(|(x, _, _)| x == id) else {
                    return;
                };
                let (_, title, kind) = run.open.remove(pos);
                run.steps.push(AgentStepView {
                    id: id.to_string(),
                    title: title.clone(),
                    kind: kind.clone(),
                    ok,
                });
                match run.mode.as_str() {
                    "stream" => push_stream(
                        st,
                        &mut run,
                        inv_id,
                        Some(StreamStepView {
                            id: id.to_string(),
                            title,
                            kind,
                            ok: Some(ok),
                        }),
                    ),
                    "status" => push_current_step(st, &run),
                    // steps：每步完成落一条 Agent 消息
                    "steps" => {
                        let title = if title.is_empty() {
                            "未命名步骤".to_string()
                        } else {
                            title
                        };
                        let icon = if ok { "✓" } else { "✗" };
                        step_msg = Some((
                            run.conversation_id,
                            run.owner,
                            run.agent_id,
                            format!("{icon} {}", title.chars().take(300).collect::<String>()),
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if let Some((conv, owner, agent_id, text)) = step_msg {
        post_agent_text(st, conv, owner, agent_id, &text, None).await;
    }
}

/// stream 模式：积压 delta 距上次推送 ≥200ms 立即推，否则挂延迟冲刷任务。
fn maybe_flush_stream(st: &SharedState, run: &mut InvRun, inv_id: Uuid) {
    if run.stream_buf.is_empty() {
        return;
    }
    if run.last_push.elapsed() >= Duration::from_millis(STREAM_FLUSH_MS) {
        push_stream(st, run, inv_id, None);
    } else if !run.flushing {
        run.flushing = true;
        let st2 = st.clone();
        tokio::spawn(async move { stream_flush_task(st2, inv_id).await });
    }
}

/// 延迟冲刷：每 200ms 检查一次缓冲，有货就推；清空后任务退出。
async fn stream_flush_task(st: SharedState, inv_id: Uuid) {
    loop {
        tokio::time::sleep(Duration::from_millis(STREAM_FLUSH_MS)).await;
        let Some(mut run) = st.inv_runs.get_mut(&inv_id) else {
            return; // 调用已终结
        };
        if run.stream_buf.is_empty() {
            run.flushing = false;
            return;
        }
        push_stream(&st, &mut run, inv_id, None);
    }
}

/// 发一帧 agent_stream：带 step 时顺带冲刷积压 delta（保证文本先于步骤到达）。
fn push_stream(st: &SharedState, run: &mut InvRun, inv_id: Uuid, step: Option<StreamStepView>) {
    let delta = if run.stream_buf.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut run.stream_buf))
    };
    if delta.is_none() && step.is_none() {
        return;
    }
    let frame = serde_json::to_string(&WsOut::AgentStream {
        invocation_id: inv_id,
        conversation_id: run.conversation_id,
        delta,
        step,
    })
    .unwrap();
    // 接收者在成员变更提交后重算，这里不查库（流式大约每 200ms 一帧）。
    st.send_to_users(&run.member_ids, &frame);
    run.last_push = Instant::now();
}

/// status 模式：步骤开/收把 current_step 填进调用快照再推状态帧。
fn push_current_step(st: &SharedState, run: &InvRun) {
    let mut inv = run.inv.clone();
    inv.current_step = run.open.last().map(|(_, t, _)| t.clone());
    push_invocation(st, &run.member_ids, &inv);
}

#[derive(sqlx::FromRow)]
struct FinishHead {
    status: String,
    owner_id: Uuid,
    conversation_id: Uuid,
    agent_id: Uuid,
    host_id: Uuid,
    schedule_id: Option<Uuid>,
    schedule_name: Option<String>,
    context_key: Option<String>,
    /// 派发时记下的代次。NULL = 迁移前已经派发的调用。
    context_epoch: Option<i64>,
}

/// 宿主回传的 invocation_finish：校验该调用确实派给这条宿主连接。
/// 行锁内推进终态。取消已经落库时这里读到的不是 running，直接忽略，不发回复。
/// ok:true → 先写 Agent 消息（stream/status 模式的定稿消息带上步骤记录）再置 succeeded；
/// ok:false → failed + error（截 500 字符）。
/// 定时调用的回复在正文前加一行「⏰ 定时任务「名字」」，上下文键是 schedule:<id>。
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
    let row = sqlx::query_as::<_, FinishHead>(
        "SELECT i.status, a.owner_id, i.conversation_id, a.id AS agent_id, a.host_id,
                i.schedule_id, s.name AS schedule_name, i.context_key, i.context_epoch
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           LEFT JOIN agent_schedules s ON s.id = i.schedule_id
          WHERE i.id = $1
          FOR UPDATE OF i",
    )
    .bind(inv_id)
    .fetch_optional(&mut *tx)
    .await;
    let Ok(Some(row)) = row else {
        return;
    };
    let FinishHead {
        status,
        owner_id: owner,
        conversation_id: conv,
        agent_id,
        host_id: real_host,
        schedule_id,
        schedule_name,
        context_key: stored_key,
        context_epoch,
    } = row;
    // 已取消 / 已超时 / 已结束，或不是这条宿主的调用：不发回复。
    // awaiting_approval 仍是进行中（进程崩了也会回 finish）。
    if (status != "running" && status != "awaiting_approval") || real_host != host_id {
        return;
    }
    let context_key = stored_key
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match schedule_id {
            Some(sid) => format!("schedule:{sid}"),
            None => conv.to_string(),
        });

    // A03：摘下运行时状态；终结前的滞留增量在定稿消息前冲刷出去
    let run = st.inv_runs.remove(&inv_id).map(|(_, r)| r);

    if v["ok"].as_bool() == Some(true) {
        // 失败的 finish 不进这个分支，因此不会覆盖已记住的运行时会话。
        let mut text = v["text"].as_str().unwrap_or_default().to_string();
        if !text.is_empty() {
            if let Some(name) = schedule_name.as_deref().filter(|s| !s.is_empty()) {
                text = format!("⏰ 定时任务「{name}」\n{text}");
            }
        }
        // A05：配额不够没发出去的产出文件，在正文末尾说明
        let skipped = v["files_skipped"].as_i64().unwrap_or(0).max(0);
        if skipped > 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("（{skipped} 个文件因存储空间不足未发送）"));
        }
        // 步骤记录：finish 帧带的 steps 优先（宿主权威记录），缺省退回事件累积
        let steps: Vec<AgentStepView> = match v["steps"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|s| serde_json::from_value(s.clone()).ok())
                .collect(),
            None => run.as_ref().map(|r| r.steps.clone()).unwrap_or_default(),
        };
        let mode = run.as_ref().map(|r| r.mode.as_str()).unwrap_or("stream");
        if let Some(r) = &run {
            if !r.stream_buf.is_empty() && r.mode == "stream" {
                let frame = serde_json::to_string(&WsOut::AgentStream {
                    invocation_id: inv_id,
                    conversation_id: r.conversation_id,
                    delta: Some(r.stream_buf.clone()),
                    step: None,
                })
                .unwrap();
                st.send_to_users(&r.member_ids, &frame);
            }
        }
        // steps 模式的正文不带步骤记录（步骤消息已逐条落库）；
        // stream/status 的定稿消息带 agent_steps（空集不落列）。
        let steps_json = match mode {
            "steps" => None,
            _ if steps.is_empty() => None,
            _ => serde_json::to_value(&steps).ok(),
        };
        // 持锁写回复，取消的 UPDATE 会等这把锁，提交后看到的已是终态
        post_agent_text(st, conv, owner, agent_id, &text, steps_json).await;
        // A05：正文之后按序发产出附件；files 里只认本次调用凭据上传的附件
        post_agent_files(st, conv, owner, agent_id, inv_id, &v["files"]).await;
        // 代次取调用行（上面 FOR UPDATE 锁住的那一列），不看内存里的运行时快照。
        if let Some(sid) = v["session_id"].as_str().filter(|s| !s.is_empty()) {
            if let Err(e) =
                persist_runtime_session(&mut tx, agent_id, &context_key, sid, context_epoch).await
            {
                tracing::error!(error = %e, "agent_context upsert failed");
                return;
            }
        }
        // 成功时记下完整回复（截 100k 字符），供互调查询。失败不写。
        let stored: String = text.chars().take(100_000).collect();
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'succeeded', error = NULL, finished_at = now(), result_text = $2
              WHERE id = $1 AND status IN ('running', 'awaiting_approval')
             {INV_RETURNING}"
        ))
        .bind(inv_id)
        .bind(&stored)
        .fetch_optional(&mut *tx)
        .await;
        if tx.commit().await.is_err() {
            return;
        }
        if let Ok(Some(mut inv)) = updated {
            inv.result_preview =
                result_preview_text(inv.parent_invocation_id, &inv.status, &stored);
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
            let member_ids = with_root_members(st, &member_ids, &inv).await;
            push_invocation(st, &member_ids, &inv);
        }
        crate::approvals::expire_invocation(st, inv_id).await;
    } else {
        let err = v["error"].as_str().unwrap_or("agent error");
        let err: String = err.chars().take(500).collect();
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'failed', error = $2, finished_at = now()
              WHERE id = $1 AND status IN ('running', 'awaiting_approval')
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
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
            let member_ids = with_root_members(st, &member_ids, &inv).await;
            push_invocation(st, &member_ids, &inv);
        }
        crate::approvals::expire_invocation(st, inv_id).await;
    }
}

/// Agent A05：finish.files 给出的附件逐个发成 Agent 的附件消息（排在正文之后）。
/// 只认 attachments.agent_invocation_id = 本次调用的行——别调用/普通上传/不存在的
/// id 一律静默忽略；文件顺序与 files 数组一致（unnest ORDINALITY 还原）。
async fn post_agent_files(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    agent_id: Uuid,
    inv_id: Uuid,
    files: &Value,
) {
    let ids: Vec<Uuid> = files
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return;
    }
    let rows = sqlx::query_scalar::<_, Uuid>(
        "SELECT a.id
           FROM unnest($2::uuid[]) WITH ORDINALITY u(id, ord)
           JOIN attachments a ON a.id = u.id
          WHERE a.agent_invocation_id = $1
          ORDER BY u.ord",
    )
    .bind(inv_id)
    .bind(&ids)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for att_id in rows {
        let posted = domain::post_message(
            &st.pool,
            domain::PostMessage {
                conversation_id: conv,
                sender_id: owner,
                content: "",
                client_msg_id: None,
                attachment_id: Some(att_id),
                reply_to_id: None,
                poll: None,
                forwarded_from: None,
                forward_attachment: None,
                agent_id: Some(agent_id),
                agent_steps: None,
                approval: None,
            },
        )
        .await;
        match posted {
            Ok(p) => crate::ws::broadcast_posted(st, conv, &p).await,
            Err(PostError::Deny { code, message }) => {
                tracing::warn!(code, message, "agent file message rejected");
            }
            Err(PostError::Db(e)) => {
                tracing::error!(error = %e, "agent file message insert failed");
            }
        }
    }
}

/// Agent 正文拆条写库 + 逐条广播。空正文不发消息（调用仍算成功）。
/// steps 只附在最后一个分片消息上（A03：定稿消息的步骤记录）。
pub async fn post_agent_text(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    agent_id: Uuid,
    text: &str,
    agent_steps: Option<serde_json::Value>,
) {
    // 预计算分片：非空白片才发；最后一片挂 agent_steps
    let mut chunks: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take = rest
            .char_indices()
            .nth(4000)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(take);
        rest = tail;
        if !head.trim().is_empty() {
            chunks.push(head);
        }
    }
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.iter().enumerate() {
        let posted = domain::post_message(
            &st.pool,
            domain::PostMessage {
                conversation_id: conv,
                sender_id: owner,
                content: chunk,
                client_msg_id: None,
                attachment_id: None,
                reply_to_id: None,
                poll: None,
                forwarded_from: None,
                forward_attachment: None,
                agent_id: Some(agent_id),
                agent_steps: if i == last { agent_steps.clone() } else { None },
                approval: None,
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

// ---------- Agent A07：登记表转发 ----------

async fn require_own_host(st: &SharedState, uid: Uuid, host_id: Uuid) -> AppResult<()> {
    let owner = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if owner != uid {
        return Err(AppError::not_found("host not found"));
    }
    Ok(())
}

/// GET /v1/agent-hosts/{id}/registry：向宿主要列表，最多等 20 秒。
pub async fn registry_list(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let uid = uid(&headers, &st).await?;
    require_own_host(&st, uid, id).await?;
    if !st.host_online(id) {
        return Err(AppError::conflict("host_offline"));
    }
    let request_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.registry_wait.insert(request_id, tx);
    let payload = json!({"type": "registry_list", "request_id": request_id}).to_string();
    if !st.send_to_host(id, &payload) {
        st.registry_wait.remove(&request_id);
        return Err(AppError::conflict("host_offline"));
    }
    let v = match tokio::time::timeout(Duration::from_secs(20), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            st.registry_wait.remove(&request_id);
            return Err(AppError::gateway_timeout("registry_timeout"));
        }
    };
    let entries = v.get("entries").cloned().unwrap_or_else(|| json!([]));
    let error = v.get("error").filter(|e| !e.is_null()).cloned();
    Ok(Json(json!({"entries": entries, "error": error})))
}

/// POST /v1/agent-hosts/{id}/registry/{entry_id}/install → 202，安装进度走 WS。
pub async fn registry_install(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((id, entry_id)): Path<(Uuid, String)>,
    Json(body): Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let uid = uid(&headers, &st).await?;
    require_own_host(&st, uid, id).await?;
    check_pin(&entry_id, 128)?;
    let version = body.get("version").and_then(Value::as_str).unwrap_or("");
    let version = check_pin(version, 64)?;
    if !st.host_online(id) {
        return Err(AppError::conflict("host_offline"));
    }
    let request_id = Uuid::new_v4();
    let payload = json!({
        "type": "registry_install",
        "request_id": request_id,
        "entry_id": entry_id,
        "version": version,
    })
    .to_string();
    if !st.send_to_host(id, &payload) {
        return Err(AppError::conflict("host_offline"));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true}))))
}

/// 宿主回 registry_result：配对到正在等的 GET。迟到的回包没有接收方就丢掉。
pub fn handle_registry_result(st: &SharedState, v: &Value) {
    let Some(id) = v["request_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    if let Some((_, tx)) = st.registry_wait.remove(&id) {
        let _ = tx.send(v.clone());
    }
}

/// 安装进度原样转给主人的普通连接，并带上 host_id。
pub fn handle_registry_progress(st: &SharedState, host_id: Uuid, v: &Value) {
    let Some(owner) = st.hosts.get(&host_id).map(|c| c.user_id) else {
        return;
    };
    let mut frame = json!({
        "type": "registry_install_progress",
        "host_id": host_id,
        "entry_id": v["entry_id"],
        "stage": v["stage"],
    });
    if let Some(err) = v
        .get("error")
        .and_then(|e| e.as_str())
        .filter(|s| !s.is_empty())
    {
        frame["error"] = json!(err);
    }
    st.send_to_user(owner, &frame.to_string());
}
