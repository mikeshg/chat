// Agent A17：定时调用的 REST、工具接口和到点扫描。
//
// 扫描按 `AppState::schedule_sweep_interval_ms`（默认 30 秒）找
// enabled 且 next_run_at <= now() 的条目。条件更新把 next_run_at 推进到下一次
// （once 则停用），同一行只有一个事务能成功，所以并发两次扫描只建一个调用。
// 停机错过的周期不回填：下一次用 now() 重算，而不是从旧槽位逐个补。
//
// 触发前的准入在 `schedule::schedule_block_reason`。A08 的使用权检查挂在那个函数上。

use crate::agent_tools::{require_invocation, ToolCtx};
use crate::auth;
use crate::domain;
use crate::error::{AppError, AppResult};
use crate::schedule::{self, ScheduleSpec};
use crate::state::SharedState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use uuid::Uuid;

const SCHEDULE_COLS: &str = "id, agent_id, owner_id, name, prompt, target_conversation_id, spec, \
     timezone, enabled, next_run_at, last_run_at, created_by, created_at, disabled_reason";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ScheduleView {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub prompt: String,
    pub target_conversation_id: Uuid,
    pub spec: Value,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateScheduleBody {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub target_conversation_id: Option<Uuid>,
    pub spec: Value,
    pub timezone: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PatchScheduleBody {
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub target_conversation_id: Option<Uuid>,
    pub spec: Option<Value>,
    pub timezone: Option<String>,
    pub enabled: Option<bool>,
}

fn check_name(name: &str) -> AppResult<()> {
    let n = name.chars().count();
    if n == 0 || n > 40 || name.trim().is_empty() {
        return Err(AppError::bad_request("bad_name"));
    }
    Ok(())
}

fn check_prompt(prompt: &str) -> AppResult<()> {
    let n = prompt.chars().count();
    if n == 0 || n > 4000 || prompt.trim().is_empty() {
        return Err(AppError::bad_request("bad_prompt"));
    }
    Ok(())
}

/// 主人必须是目标会话成员；只读私聊（好友已解除）不能作为投递目标。
async fn ensure_target(pool: &PgPool, owner: Uuid, conv: Uuid) -> AppResult<()> {
    let kind = domain::conv_kind(pool, conv)
        .await?
        .ok_or_else(|| AppError::bad_request("not_member"))?;
    if domain::membership(pool, conv, owner).await?.is_none() {
        return Err(AppError::bad_request("not_member"));
    }
    if kind == "dm" {
        let peer: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM memberships
              WHERE conversation_id = $1 AND user_id <> $2
              LIMIT 1",
        )
        .bind(conv)
        .bind(owner)
        .fetch_optional(pool)
        .await?;
        if let Some(peer) = peer {
            if !domain::is_friend(pool, owner, peer).await? {
                return Err(AppError::bad_request("readonly_dm"));
            }
        }
    }
    Ok(())
}

async fn owned_live_agent(pool: &PgPool, owner: Uuid, agent_id: Uuid) -> AppResult<()> {
    let row = sqlx::query_as::<_, (Uuid, Option<DateTime<Utc>>)>(
        "SELECT owner_id, deleted_at FROM agents WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some((o, None)) if o == owner => Ok(()),
        _ => Err(AppError::not_found("agent not found")),
    }
}

async fn agent_conversation(pool: &PgPool, agent_id: Uuid) -> AppResult<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM conversations WHERE agent_id = $1 AND type = 'agent' AND workspace_id IS NULL",
    )
        .bind(agent_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::not_found("agent not found"))
}

struct ParsedSpec {
    spec: ScheduleSpec,
    tz_name: String,
    next: DateTime<Utc>,
}

fn parse_timing(spec_v: &Value, timezone: &str, now: DateTime<Utc>) -> AppResult<ParsedSpec> {
    let spec = schedule::parse_spec(spec_v).map_err(|_| AppError::bad_request("bad_spec"))?;
    let tz =
        schedule::parse_timezone(timezone).map_err(|_| AppError::bad_request("bad_timezone"))?;
    let next =
        schedule::next_run(&spec, tz, now).ok_or_else(|| AppError::bad_request("bad_spec"))?;
    Ok(ParsedSpec {
        spec,
        tz_name: tz.name().to_string(),
        next,
    })
}

async fn insert_schedule(
    st: &SharedState,
    owner: Uuid,
    agent_id: Uuid,
    created_by: &str,
    body: CreateScheduleBody,
    default_target: Uuid,
) -> AppResult<ScheduleView> {
    check_name(&body.name)?;
    check_prompt(&body.prompt)?;
    owned_live_agent(&st.pool, owner, agent_id).await?;
    let target = body.target_conversation_id.unwrap_or(default_target);
    ensure_target(&st.pool, owner, target).await?;
    let timing = parse_timing(&body.spec, &body.timezone, Utc::now())?;
    let spec_json = schedule::spec_to_json(&timing.spec);

    let mut tx = st.pool.begin().await?;
    // 锁住主人，避免并发创建冲破 50 条
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_schedules WHERE owner_id = $1")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
    if n >= 50 {
        return Err(AppError::bad_request("too_many"));
    }
    let view = sqlx::query_as::<_, ScheduleView>(&format!(
        "INSERT INTO agent_schedules
            (agent_id, owner_id, name, prompt, target_conversation_id, spec, timezone,
             enabled, next_run_at, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8, $9)
         RETURNING {SCHEDULE_COLS}"
    ))
    .bind(agent_id)
    .bind(owner)
    .bind(&body.name)
    .bind(&body.prompt)
    .bind(target)
    .bind(&spec_json)
    .bind(&timing.tz_name)
    .bind(timing.next)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(view)
}

/// GET /v1/agents/{id}/schedules
pub async fn list_schedules(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
) -> AppResult<Json<Vec<ScheduleView>>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    owned_live_agent(&st.pool, uid, agent_id).await?;
    let rows = sqlx::query_as::<_, ScheduleView>(&format!(
        "SELECT {SCHEDULE_COLS} FROM agent_schedules
          WHERE agent_id = $1 AND owner_id = $2
          ORDER BY created_at, id"
    ))
    .bind(agent_id)
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

/// POST /v1/agents/{id}/schedules。目标缺省为该 Agent 的专属会话。
pub async fn create_schedule(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
    Json(body): Json<CreateScheduleBody>,
) -> AppResult<Json<ScheduleView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let agent_conv = agent_conversation(&st.pool, agent_id).await?;
    let view = insert_schedule(&st, uid, agent_id, "user", body, agent_conv).await?;
    Ok(Json(view))
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
struct PatchRow {
    id: Uuid,
    agent_id: Uuid,
    owner_id: Uuid,
    name: String,
    prompt: String,
    target_conversation_id: Uuid,
    spec: Value,
    timezone: String,
    enabled: bool,
    next_run_at: Option<DateTime<Utc>>,
    disabled_reason: Option<String>,
    agent_deleted_at: Option<DateTime<Utc>>,
}

/// PATCH /v1/agent-schedules/{id}
pub async fn patch_schedule(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchScheduleBody>,
) -> AppResult<Json<ScheduleView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, PatchRow>(
        "SELECT s.id, s.agent_id, s.owner_id, s.name, s.prompt, s.target_conversation_id,
                s.spec, s.timezone, s.enabled, s.next_run_at, s.disabled_reason,
                a.deleted_at AS agent_deleted_at
           FROM agent_schedules s
           JOIN agents a ON a.id = s.agent_id
          WHERE s.id = $1
          FOR UPDATE OF s",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(AppError::not_found("schedule not found"));
    };
    if row.owner_id != uid || row.agent_deleted_at.is_some() {
        return Err(AppError::not_found("schedule not found"));
    }

    let name = match body.name {
        Some(n) => {
            check_name(&n)?;
            n
        }
        None => row.name,
    };
    let prompt = match body.prompt {
        Some(p) => {
            check_prompt(&p)?;
            p
        }
        None => row.prompt,
    };
    let target = body
        .target_conversation_id
        .unwrap_or(row.target_conversation_id);
    let turning_on = body.enabled == Some(true) && !row.enabled;
    if body.target_conversation_id.is_some() || turning_on {
        ensure_target(&st.pool, uid, target).await?;
    }

    let mut spec_v = row.spec;
    let mut tz_name = row.timezone;
    let timing_changed = body.spec.is_some() || body.timezone.is_some() || turning_on;
    if let Some(spec) = body.spec {
        spec_v = spec;
    }
    if let Some(tz) = body.timezone {
        tz_name = tz;
    }
    let (spec_json, tz_stored, next, reason, enabled) = if timing_changed {
        let timing = parse_timing(&spec_v, &tz_name, Utc::now())?;
        let enabled = body.enabled.unwrap_or(row.enabled);
        (
            schedule::spec_to_json(&timing.spec),
            timing.tz_name,
            Some(timing.next),
            None,
            enabled,
        )
    } else {
        let enabled = body.enabled.unwrap_or(row.enabled);
        (
            spec_v,
            tz_name,
            row.next_run_at,
            row.disabled_reason,
            enabled,
        )
    };

    let view = sqlx::query_as::<_, ScheduleView>(&format!(
        "UPDATE agent_schedules
            SET name = $2, prompt = $3, target_conversation_id = $4, spec = $5, timezone = $6,
                enabled = $7, next_run_at = $8, disabled_reason = $9
          WHERE id = $1
         RETURNING {SCHEDULE_COLS}"
    ))
    .bind(id)
    .bind(&name)
    .bind(&prompt)
    .bind(target)
    .bind(&spec_json)
    .bind(&tz_stored)
    .bind(enabled)
    .bind(next)
    .bind(&reason)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}

/// DELETE /v1/agent-schedules/{id}
pub async fn delete_schedule(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let n = sqlx::query("DELETE FROM agent_schedules WHERE id = $1 AND owner_id = $2")
        .bind(id)
        .bind(uid)
        .execute(&st.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("schedule not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 工具（Invocation 凭据，范围 = 这次调用的 Agent / 主人） ----------

async fn tool_ctx(st: &SharedState, headers: &HeaderMap) -> AppResult<ToolCtx> {
    require_invocation(st, headers).await
}

/// GET /v1/agent-tools/schedules
pub async fn tool_list_schedules(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<ScheduleView>>> {
    let ctx = tool_ctx(&st, &headers).await?;
    let rows = sqlx::query_as::<_, ScheduleView>(&format!(
        "SELECT {SCHEDULE_COLS} FROM agent_schedules
          WHERE agent_id = $1 AND owner_id = $2
          ORDER BY created_at, id"
    ))
    .bind(ctx.agent)
    .bind(ctx.owner)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

/// POST /v1/agent-tools/schedules。目标缺省为发起这次调用的会话。created_by = agent。
pub async fn tool_create_schedule(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<CreateScheduleBody>,
) -> AppResult<Json<ScheduleView>> {
    let ctx = tool_ctx(&st, &headers).await?;
    let view = insert_schedule(&st, ctx.owner, ctx.agent, "agent", body, ctx.conversation).await?;
    Ok(Json(view))
}

/// DELETE /v1/agent-tools/schedules/{id}。只能删这次调用的 Agent 自己的条目。
pub async fn tool_delete_schedule(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let ctx = tool_ctx(&st, &headers).await?;
    let n = sqlx::query(
        "DELETE FROM agent_schedules WHERE id = $1 AND owner_id = $2 AND agent_id = $3",
    )
    .bind(id)
    .bind(ctx.owner)
    .bind(ctx.agent)
    .execute(&st.pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("schedule not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 到点扫描 ----------

#[derive(sqlx::FromRow)]
struct DueRow {
    id: Uuid,
    agent_id: Uuid,
    owner_id: Uuid,
    prompt: String,
    target_conversation_id: Uuid,
    spec: Value,
    timezone: String,
    agent_deleted_at: Option<DateTime<Utc>>,
    disabled_at: Option<DateTime<Utc>>,
}

/// 扫描一轮。返回实际建出调用的条数。测试可直接调用；后台任务也走这里。
pub async fn sweep_due_schedules(st: &SharedState) -> Result<usize, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM agent_schedules
          WHERE enabled AND next_run_at IS NOT NULL AND next_run_at <= now()
          ORDER BY next_run_at, id
          LIMIT 100",
    )
    .fetch_all(&st.pool)
    .await?;
    let mut fired = 0usize;
    for id in ids {
        // 推进计划和建调用在同一事务：中途失败整体回滚，下一轮重试，不会「显示已触发却没调用」
        if let Some((mut tx, job)) = claim_one(st, id).await? {
            let ready = crate::agents::insert_schedule_invocation(st, &mut tx, &job).await?;
            tx.commit().await?;
            if let Some((host_id, inv)) = ready {
                crate::agents::dispatch_schedule_invocation(st, host_id, &inv).await;
                fired += 1;
            }
        }
    }
    Ok(fired)
}

async fn disable_in<'e, E>(ex: E, id: Uuid, reason: &str) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "UPDATE agent_schedules
            SET enabled = false, next_run_at = NULL, disabled_reason = $2
          WHERE id = $1",
    )
    .bind(id)
    .bind(reason)
    .execute(ex)
    .await?;
    Ok(())
}

/// 锁住这一行。不满足准入就停用；否则推进 next_run_at（once 直接停用），
/// 连同还没提交的事务一起返回——调用方在同一事务里建调用再提交。
async fn claim_one(
    st: &SharedState,
    id: Uuid,
) -> Result<Option<(sqlx::Transaction<'static, sqlx::Postgres>, crate::agents::ScheduleFire)>, sqlx::Error>
{
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, DueRow>(
        "SELECT s.id, s.agent_id, s.owner_id, s.prompt, s.target_conversation_id, s.spec, s.timezone,
                a.deleted_at AS agent_deleted_at, u.disabled_at
           FROM agent_schedules s
           JOIN agents a ON a.id = s.agent_id
           JOIN users u ON u.id = s.owner_id
          WHERE s.id = $1 AND s.enabled AND s.next_run_at IS NOT NULL AND s.next_run_at <= now()
          FOR UPDATE OF s",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let still_member = domain::membership(&mut *tx, row.target_conversation_id, row.owner_id)
        .await?
        .is_some();

    // 使用权（A08）不通过时与下面一样停用，reason = "agent_access"
    let allowed = crate::agent_access::user_allows(&mut *tx, row.owner_id).await?;
    let reason = schedule::schedule_block_reason(
        row.disabled_at.is_some(),
        row.agent_deleted_at.is_some(),
        still_member,
    )
    .or((!allowed).then_some("agent_access"));
    if let Some(reason) = reason {
        disable_in(&mut *tx, row.id, reason).await?;
        tx.commit().await?;
        tracing::info!(schedule_id = %row.id, reason, "定时任务已停用");
        return Ok(None);
    }

    let spec = match schedule::parse_spec(&row.spec) {
        Ok(s) => s,
        Err(_) => {
            disable_in(&mut *tx, row.id, "bad_spec").await?;
            tx.commit().await?;
            return Ok(None);
        }
    };
    let tz = match schedule::parse_timezone(&row.timezone) {
        Ok(tz) => tz,
        Err(_) => {
            disable_in(&mut *tx, row.id, "bad_spec").await?;
            tx.commit().await?;
            return Ok(None);
        }
    };

    let job = crate::agents::ScheduleFire {
        schedule_id: row.id,
        agent_id: row.agent_id,
        conversation_id: row.target_conversation_id,
        prompt: row.prompt.clone(),
    };

    if spec.is_once() {
        // once 跑完即停。测试把 next_run_at 拨到过去来模拟到点，spec.at 可能仍在未来，
        // 所以不能靠 next_run() 返回 None 来判断，而是凡触发过就停用。
        sqlx::query(
            "UPDATE agent_schedules
                SET enabled = false, next_run_at = NULL, last_run_at = now(), disabled_reason = NULL
              WHERE id = $1",
        )
        .bind(row.id)
        .execute(&mut *tx)
        .await?;
        return Ok(Some((tx, job)));
    }

    let Some(next) = schedule::next_run(&spec, tz, Utc::now()) else {
        disable_in(&mut *tx, row.id, "bad_spec").await?;
        tx.commit().await?;
        return Ok(None);
    };
    let n = sqlx::query(
        "UPDATE agent_schedules
            SET next_run_at = $2, last_run_at = now(), enabled = true, disabled_reason = NULL
          WHERE id = $1 AND enabled AND next_run_at IS NOT NULL AND next_run_at <= now()",
    )
    .bind(row.id)
    .bind(next)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        tx.rollback().await.ok();
        return Ok(None);
    }
    Ok(Some((tx, job)))
}

pub fn spawn_sweeper(st: SharedState) {
    tokio::spawn(async move {
        loop {
            wait_interval(&st).await;
            if let Err(e) = sweep_due_schedules(&st).await {
                tracing::warn!(error = %e, "定时调用扫描失败");
            }
        }
    });
}

async fn wait_interval(st: &SharedState) {
    let started = Instant::now();
    loop {
        let ms = st
            .schedule_sweep_interval_ms
            .load(Ordering::Relaxed)
            .max(50);
        let target = Duration::from_millis(ms);
        if started.elapsed() >= target {
            return;
        }
        let left = target.saturating_sub(started.elapsed());
        tokio::time::sleep(left.min(Duration::from_millis(250))).await;
    }
}
