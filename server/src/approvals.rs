// Agent A12：审批。宿主上报超档操作 → 调用改成 awaiting_approval，
// 在 Agent 专属会话里以 Agent 身份发一张卡片。主人决定后把选项交回宿主。
// 调用取消 / 失败 / 宿主断开时，未决审批改为 expired。

use crate::auth;
use crate::domain::{self, PostError};
use crate::error::{AppError, AppResult};
use crate::model::{InvocationView, MessageView, WsOut};
use crate::state::SharedState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

// 调用行的返回列与 agents.rs 共用一份，免得两边列不一致导致行解码失败
use crate::agents::INV_RETURNING;

#[derive(Deserialize)]
pub struct DecideReq {
    option_id: String,
}

#[derive(sqlx::FromRow)]
struct DecideRow {
    status: String,
    options: Value,
    request: Value,
    message_id: Uuid,
    invocation_id: Uuid,
    owner_id: Uuid,
    host_id: Uuid,
    inv_status: String,
    conversation_id: Uuid,
}

/// 宿主 → approval_request。调用必须属于这条宿主连接且仍是 running，否则忽略。
pub async fn handle_request(st: &SharedState, host_id: Uuid, v: &Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let request_id: String = v["request_id"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect();
    if request_id.is_empty() {
        return;
    }
    let Some(options) = sanitize_options(&v["options"]) else {
        return;
    };
    let request = sanitize_request(&v["tool_call"], &request_id, inv_id);
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String)>(
        "SELECT i.conversation_id, a.id, a.owner_id, i.status
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.id = $1 AND a.host_id = $2",
    )
    .bind(inv_id)
    .bind(host_id)
    .fetch_optional(&st.pool)
    .await;
    let Ok(Some((conv, agent_id, owner, status))) = row else {
        return;
    };
    if status != "running" {
        return;
    }
    let updated = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = 'awaiting_approval'
          WHERE id = $1 AND status = 'running' AND finished_at IS NULL
          {INV_RETURNING}"
    ))
    .bind(inv_id)
    .fetch_optional(&st.pool)
    .await;
    let Ok(Some(inv)) = updated else {
        return;
    };
    let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
    push_invocation(st, &members, &inv);

    let agent_conv = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM conversations WHERE agent_id = $1 AND type = 'agent'",
    )
    .bind(agent_id)
    .fetch_optional(&st.pool)
    .await
    .ok()
    .flatten();
    let Some(agent_conv) = agent_conv else {
        tracing::error!("审批找不到 Agent 专属会话");
        return;
    };
    let text = approval_text(&request);
    let options_value = Value::Array(options);
    let posted = domain::post_message(
        &st.pool,
        domain::PostMessage {
            conversation_id: agent_conv,
            sender_id: owner,
            content: &text,
            client_msg_id: None,
            attachment_id: None,
            reply_to_id: None,
            poll: None,
            forwarded_from: None,
            forward_attachment: None,
            agent_id: Some(agent_id),
            agent_steps: None,
            approval: Some(domain::ApprovalDraft {
                invocation_id: inv_id,
                request: &request,
                options: &options_value,
            }),
        },
    )
    .await;
    match posted {
        Ok(p) => crate::ws::broadcast_posted(st, agent_conv, &p).await,
        Err(PostError::Deny { code, message }) => {
            tracing::warn!(code, message, "审批卡片被拒绝");
        }
        Err(PostError::Db(e)) => tracing::error!(error = %e, "审批卡片写入失败"),
    }
}

/// POST /v1/agent-approvals/{id}/decide。不是主人 → 404；不是 pending → 409；
/// option_id 不在选项里 → 400。
pub async fn decide(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<DecideReq>,
) -> AppResult<Json<MessageView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    if !crate::agent_access::user_allows(&st.pool, uid).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    let option_id = req.option_id.trim().to_string();
    if option_id.is_empty() || option_id.chars().count() > 80 {
        return Err(AppError::bad_request("bad_option"));
    }

    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, DecideRow>(
        "SELECT ap.status, ap.options, ap.request, ap.message_id, ap.invocation_id,
                a.owner_id, a.host_id, i.status AS inv_status, i.conversation_id
           FROM agent_approvals ap
           JOIN agent_invocations i ON i.id = ap.invocation_id
           JOIN agents a ON a.id = i.agent_id
          WHERE ap.id = $1
          FOR UPDATE OF ap, i",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(AppError::not_found("approval not found"));
    };
    if row.owner_id != uid {
        return Err(AppError::not_found("approval not found"));
    }
    if row.status != "pending" || row.inv_status != "awaiting_approval" {
        return Err(AppError::conflict("not_pending"));
    }
    if !option_in(&row.options, &option_id) {
        return Err(AppError::bad_request("bad_option"));
    }

    sqlx::query(
        "UPDATE agent_approvals
            SET status = 'decided', decision = $2, decided_at = now()
          WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(&option_id)
    .execute(&mut *tx)
    .await?;
    let inv = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = 'running'
          WHERE id = $1 AND status = 'awaiting_approval' AND finished_at IS NULL
          {INV_RETURNING}"
    ))
    .bind(row.invocation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(inv) = inv else {
        return Err(AppError::conflict("not_pending"));
    };
    sqlx::query("UPDATE messages SET edit_version = edit_version + 1 WHERE id = $1")
        .bind(row.message_id)
        .execute(&mut *tx)
        .await?;
    let view_row =
        sqlx::query_as::<_, domain::MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
            .bind(row.message_id)
            .fetch_one(&mut *tx)
            .await?;
    let card_conv = view_row.conversation_id;
    let edit_version = view_row.edit_version;
    let card_members = domain::member_ids(&mut *tx, card_conv).await?;
    let origin_members = domain::member_ids(&mut *tx, row.conversation_id).await?;
    let request_id = row
        .request
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let host_id = row.host_id;
    tx.commit().await?;

    push_invocation(&st, &origin_members, &inv);
    crate::ws::broadcast_message_updated(
        &st,
        &domain::Edited {
            conversation_id: card_conv,
            row: view_row.clone(),
            member_ids: card_members,
            edit_version,
        },
    )
    .await;
    if !request_id.is_empty() {
        let frame = json!({
            "type": "approval_decision",
            "request_id": request_id,
            "option_id": option_id,
        })
        .to_string();
        let _ = st.send_to_host(host_id, &frame);
    }
    Ok(Json(view_row.into_view(uid)))
}

/// 调用离开「还能被决定」的状态时，把它剩下的 pending 审批标成 expired 并刷新卡片。
pub async fn expire_invocation(st: &SharedState, inv_id: Uuid) {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "UPDATE agent_approvals
            SET status = 'expired', decided_at = now()
          WHERE invocation_id = $1 AND status = 'pending'
         RETURNING message_id",
    )
    .bind(inv_id)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for message_id in ids {
        publish_message(st, message_id).await;
    }
}

async fn publish_message(st: &SharedState, message_id: Uuid) {
    let bumped = sqlx::query_scalar::<_, Uuid>(
        "UPDATE messages SET edit_version = edit_version + 1 WHERE id = $1 RETURNING conversation_id",
    )
    .bind(message_id)
    .fetch_optional(&st.pool)
    .await;
    let Ok(Some(conv)) = bumped else {
        return;
    };
    let row = sqlx::query_as::<_, domain::MsgViewRow>("SELECT * FROM message_views WHERE id = $1")
        .bind(message_id)
        .fetch_optional(&st.pool)
        .await;
    let Ok(Some(row)) = row else {
        return;
    };
    let edit_version = row.edit_version;
    let member_ids = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
    crate::ws::broadcast_message_updated(
        st,
        &domain::Edited {
            conversation_id: conv,
            row,
            member_ids,
            edit_version,
        },
    )
    .await;
}

fn push_invocation(st: &SharedState, members: &[Uuid], inv: &InvocationView) {
    let p = serde_json::to_string(&WsOut::AgentInvocation {
        invocation: inv.clone(),
    })
    .unwrap();
    st.send_to_users(members, &p);
}

fn option_in(options: &Value, option_id: &str) -> bool {
    options
        .as_array()
        .is_some_and(|a| a.iter().any(|o| o["optionId"].as_str() == Some(option_id)))
}

fn sanitize_options(v: &Value) -> Option<Vec<Value>> {
    let arr = v.as_array()?;
    let out: Vec<Value> = arr
        .iter()
        .take(20)
        .filter_map(|item| {
            let id: String = item["optionId"].as_str()?.chars().take(80).collect();
            if id.is_empty() {
                return None;
            }
            let name: String = item["name"]
                .as_str()
                .unwrap_or(&id)
                .chars()
                .take(80)
                .collect();
            let kind: String = item["kind"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            Some(json!({"optionId": id, "name": name, "kind": kind}))
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn sanitize_locations(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .take(20)
                .filter_map(|item| {
                    let s = item
                        .as_str()
                        .or_else(|| item.get("path").and_then(Value::as_str))?;
                    let s: String = s.trim().chars().take(500).collect();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn sanitize_request(tool: &Value, request_id: &str, inv_id: Uuid) -> Value {
    let title: String = tool["title"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    let kind: String = tool["kind"]
        .as_str()
        .unwrap_or("other")
        .chars()
        .take(40)
        .collect();
    let locations = sanitize_locations(&tool["locations"]);
    json!({
        "title": title,
        "kind": kind,
        "locations": locations,
        "request_id": request_id,
        "invocation_id": inv_id,
        "tool_call": {
            "title": title,
            "kind": kind,
            "locations": locations,
        },
    })
}

fn approval_text(request: &Value) -> String {
    let kind = request["kind"].as_str().unwrap_or("other");
    let verb = match kind {
        "read" => "读取",
        "edit" => "写入",
        "delete" => "删除",
        "move" => "移动",
        "search" => "搜索",
        "execute" => "执行",
        "think" => "思考",
        "fetch" => "抓取",
        "switch_mode" => "切换模式",
        _ => "操作",
    };
    let title = request["title"].as_str().unwrap_or("").trim();
    let locs: Vec<&str> = request["locations"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let detail = if !title.is_empty() {
        title.to_string()
    } else {
        locs.join(" ")
    };
    let text = if detail.is_empty() {
        format!("请求{verb}")
    } else {
        format!("请求{verb}：{detail}")
    };
    text.chars().take(4000).collect()
}
