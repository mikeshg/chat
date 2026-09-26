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
    AgentHostView, AgentRuntimeReq, AgentUserSettings, AgentView, CreateAgentReq, InvocationView,
    PatchAgentReq, UpdateAgentUserSettingsReq, WsOut,
};
use crate::state::SharedState;
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
        (a.deleted_at IS NOT NULL) AS deleted
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
        extra_dirs: Vec::new(),
        model: String::new(),
        mode: String::new(),
        instructions: String::new(),
        env: json!({}),
        reported_options: json!({}),
        avatar_version: 0,
        deleted: false,
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
    let invs = sqlx::query_as::<_, InvocationView>(
        "UPDATE agent_invocations SET status = 'failed', error = 'agent_deleted',
                finished_at = now()
          WHERE agent_id = $1 AND finished_at IS NULL
         RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at",
    )
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
        push_invocation(&st, &members, inv);
    }
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

/// POST /conversations/{id}/agent-context/reset
/// 删掉这个会话对应的 AgentContext。仅成员；非 agent 会话 400；没有记录也 204。
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
    if ty != "agent" {
        return Err(AppError::bad_request("not an agent conversation"));
    }
    let Some(agent_id) = agent_id else {
        return Err(AppError::bad_request("not an agent conversation"));
    };
    sqlx::query("DELETE FROM agent_contexts WHERE agent_id = $1 AND context_key = $2")
        .bind(agent_id)
        .bind(conv.to_string())
        .execute(&st.pool)
        .await?;
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
        deleted_at: r.deleted_at,
    }
}

const AGENT_CFG_SQL: &str = "SELECT a.id, a.owner_id, a.host_id, a.name, a.runtime, a.cwd,
        a.extra_dirs, a.model, a.mode, a.instructions, a.env, a.deleted_at";

async fn load_cfg(pool: &PgPool, id: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, AgentCfgRow>(&format!("{AGENT_CFG_SQL} FROM agents a WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|r| r.map(row_to_agent))
}

/// 该 Agent 在这个会话上记住的运行时会话 id。没有或查询失败都当新会话。
async fn context_session(pool: &PgPool, agent_id: Uuid, conv: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT runtime_session_id FROM agent_contexts
          WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(agent_id)
    .bind(conv.to_string())
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
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
    let payload = invocation_start_payload(st, inv.id, &agent, conv, &prompt).await;
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

/// invocation_start 帧：Agent 配置快照（A06）+ 该会话记住的运行时会话（A02）
/// + 本次调用的工具凭据（A13，只在调用进行中有效）。即时派发与宿主上线后清排队共用。
async fn invocation_start_payload(
    st: &SharedState,
    inv_id: Uuid,
    agent: &AgentRow,
    conv: Uuid,
    prompt: &str,
) -> String {
    let session_id = context_session(&st.pool, agent.id, conv).await;
    // 签发失败（几乎不可能）时不带凭据：调用照常跑，只是用不了工具
    let tools_token =
        crate::auth::mint_invocation(inv_id, agent.owner, agent.id, 0, &st.cfg.jwt_secret)
            .map_err(|e| tracing::error!(error = ?e, "tools_token mint failed"))
            .ok();
    json!({
        "type": "invocation_start",
        "invocation_id": inv_id,
        "agent": agent_snapshot(agent),
        "prompt": prompt,
        "context": {"key": conv.to_string(), "session_id": session_id},
        "tools_token": tools_token,
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
    trigger_message_id: Uuid,
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
pub async fn flush_queued(st: &SharedState, host_id: Uuid) {
    let rows = sqlx::query_as::<_, QueuedStart>(
        "SELECT i.id, i.conversation_id, a.id AS agent_id, m.content AS prompt,
                i.trigger_message_id
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
        let agent = match load_cfg(&st.pool, row.agent_id).await {
            Ok(Some(a)) => a,
            _ => continue,
        };
        let prompt = prompt_for_flushed(
            &st.pool,
            row.conversation_id,
            row.trigger_message_id,
            &row.prompt,
        )
        .await;
        let payload =
            invocation_start_payload(st, inv.id, &agent, row.conversation_id, &prompt).await;
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
    let row = sqlx::query_as::<_, (String, Uuid, Uuid, Uuid, Uuid)>(
        "SELECT i.status, a.owner_id, i.conversation_id, a.id, a.host_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
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

    if v["ok"].as_bool() == Some(true) {
        // 失败的 finish 不进这个分支，因此不会覆盖已记住的运行时会话
        if let Some(sid) = v["session_id"].as_str().filter(|s| !s.is_empty()) {
            if let Err(e) = sqlx::query(
                "INSERT INTO agent_contexts (agent_id, context_key, runtime_session_id, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (agent_id, context_key)
                 DO UPDATE SET runtime_session_id = EXCLUDED.runtime_session_id,
                               updated_at = now()",
            )
            .bind(agent_id)
            .bind(conv.to_string())
            .bind(sid)
            .execute(&st.pool)
            .await
            {
                tracing::error!(error = %e, "agent_context upsert failed");
            }
        }
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
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
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
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
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
