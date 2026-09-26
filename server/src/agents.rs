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
use crate::model::{AgentHostView, AgentView, CreateAgentReq, InvocationView, WsOut};
use crate::state::SharedState;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
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

pub async fn list_agents(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Uuid,
            serde_json::Value,
            String,
            DateTime<Utc>,
            Option<Uuid>,
        ),
    >(
        "SELECT a.id, a.name, a.host_id, a.runtime, a.cwd, a.created_at,
                (SELECT c.id FROM conversations c
                  WHERE c.agent_id = a.id AND c.type = 'agent') AS conversation_id
           FROM agents a WHERE a.owner_id = $1 ORDER BY a.created_at, a.id",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, name, host_id, runtime, cwd, created_at, conv)| AgentView {
                    id,
                    name,
                    host_id,
                    host_online: st.host_online(host_id),
                    runtime,
                    cwd,
                    conversation_id: conv.unwrap_or_default(),
                    created_at,
                },
            )
            .collect(),
    ))
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
    }))
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

/// 宿主回传的 invocation_finish：校验该调用确实派给这条宿主连接。
/// ok:true → 把正文拆成 ≤4000 字符的 Agent 消息广播，然后置 succeeded；
/// ok:false → failed + error（截 500 字符）。
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

    if v["ok"].as_bool() == Some(true) {
        let text = v["text"].as_str().unwrap_or_default().to_string();
        post_agent_text(st, conv, owner, agent_id, &text).await;
        let _ = finish_invocation(st, inv_id, "succeeded", None, &member_ids).await;
    } else {
        let err = v["error"].as_str().unwrap_or("agent error");
        let err: String = err.chars().take(500).collect();
        let _ = finish_invocation(st, inv_id, "failed", Some(&err), &member_ids).await;
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
