// Agent A01：Agent / AgentHost / AgentInvocation 的 REST 与派发。
//
// - REST：GET /v1/agent-hosts、GET|POST /v1/agents、
//   GET /v1/conversations/{id}/agent-invocations —— 一律只作用于本人资源，
//   别人的宿主/Agent 按 id 访问返回 404，不泄露存在性。
// - 派发挂点在 ws::broadcast_posted：专属会话里的主人文字消息触发一次调用；
//   宿主在线 → invocation_start 下发宿主连接，离线 → 立即 failed/host_offline。

use crate::auth;
use crate::domain::{self, PostError};
use crate::error::{AppError, AppResult};
use crate::model::{
    AgentHostView, AgentStepView, AgentUserSettings, AgentView, CreateAgentReq, InvocationView,
    PatchAgentReq, StreamStepView, UpdateAgentUserSettingsReq, WsOut,
};
use crate::state::{InvRun, SharedState};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
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
    let mut rows = sqlx::query_as::<_, InvocationView>(
        "SELECT id, agent_id, conversation_id, trigger_message_id, status, error,
                created_at, started_at, finished_at
           FROM agent_invocations WHERE conversation_id = $1
          ORDER BY created_at DESC, id DESC LIMIT $2",
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
    // 宿主在线 → running 并把 invocation_start 下发宿主连接；离线 → 直接 failed
    let online = st.host_online(agent.host_id);
    let inv = if online {
        insert_invocation(&st.pool, &agent, conv, trigger, "running", None, true).await
    } else {
        insert_invocation(
            &st.pool,
            &agent,
            conv,
            trigger,
            "failed",
            Some("host_offline"),
            false,
        )
        .await
    };
    let Ok(inv) = inv else {
        tracing::error!("agent_invocation insert failed");
        return;
    };
    if inv.status == "running" {
        // A03：登记运行时状态（输出方式、进行中步骤、流式缓冲）。
        // 必须在 invocation_start 下发前就绪——宿主事件随时可能先回来。
        let mode = effective_settings(&st.pool, agent.id)
            .await
            .map(|s| s.output_mode)
            .unwrap_or_else(|_| "stream".into());
        st.inv_runs.insert(
            inv.id,
            InvRun {
                host_id: agent.host_id,
                conversation_id: conv,
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
    push_invocation(st, member_ids, &inv);
    if inv.status != "running" {
        return;
    }
    let payload = serde_json::json!({
        "type": "invocation_start",
        "invocation_id": inv.id,
        "agent": {"id": agent.id, "name": agent.name, "runtime": agent.runtime, "cwd": agent.cwd},
        "prompt": prompt,
    })
    .to_string();
    // 发不出去（恰在检查后断开等）→ 与离线同语义：failed/host_offline
    if !st.send_to_host(agent.host_id, &payload) {
        st.inv_runs.remove(&inv.id);
        let _ = finish_invocation(st, inv.id, "failed", Some("host_offline"), member_ids).await;
    }
}

async fn insert_invocation(
    pool: &PgPool,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Uuid,
    status: &str,
    error: Option<&str>,
    started: bool,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, status, error,
             started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5,
                 CASE WHEN $6 THEN now() END,
                 CASE WHEN $6 THEN NULL ELSE now() END)
         RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at",
    )
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(status)
    .bind(error)
    .bind(started)
    .fetch_one(pool)
    .await
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
    st.send_to_users(&run.member_ids, &frame);
    run.last_push = Instant::now();
}

/// status 模式：步骤开/收把 current_step 填进调用快照再推状态帧。
fn push_current_step(st: &SharedState, run: &InvRun) {
    let mut inv = run.inv.clone();
    inv.current_step = run.open.last().map(|(_, t, _)| t.clone());
    push_invocation(st, &run.member_ids, &inv);
}

/// 宿主回传的 invocation_finish：校验该调用确实派给这条宿主连接。
/// ok:true → 把正文拆成 ≤4000 字符的 Agent 消息广播（stream/status 模式
/// 的定稿消息带上步骤记录），然后置 succeeded；ok:false → failed + error。
pub async fn handle_finish(st: &SharedState, host_id: Uuid, v: &serde_json::Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let row = sqlx::query_as::<_, (Uuid, String, Option<Uuid>, Uuid, Uuid, String)>(
        "SELECT i.id, i.status, c.owner_id, i.conversation_id, a.id, a.name
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN conversations c ON c.id = i.conversation_id
          WHERE i.id = $1",
    )
    .bind(inv_id)
    .fetch_optional(&st.pool)
    .await;
    let Ok(Some((_, status, owner, conv, agent_id, _name))) = row else {
        return;
    };
    if status != "running" {
        return; // 已完成/未开始：重复或乱序的 finish 一律忽略
    }
    let owner = owner.unwrap_or_default();
    let member_ids = vec![owner];
    // 防伪造：只有派给本宿主的调用才受理其 finish
    let real_host = sqlx::query_scalar::<_, Uuid>("SELECT host_id FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_one(&st.pool)
        .await;
    if !matches!(real_host, Ok(h) if h == host_id) {
        return;
    }

    // A03：摘下运行时状态；终结前的滞留增量在定稿消息前冲刷出去
    let run = st.inv_runs.remove(&inv_id).map(|(_, r)| r);

    if v["ok"].as_bool() == Some(true) {
        let text = v["text"].as_str().unwrap_or_default().to_string();
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
        post_agent_text(st, conv, owner, agent_id, &text, steps_json).await;
        let _ = finish_invocation(st, inv_id, "succeeded", None, &member_ids).await;
    } else {
        let err = v["error"].as_str().unwrap_or("agent error");
        let err: String = err.chars().take(500).collect();
        let _ = finish_invocation(st, inv_id, "failed", Some(&err), &member_ids).await;
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
