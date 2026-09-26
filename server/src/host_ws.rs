// Agent A01：宿主专用 WS 会话（/v1/ws?ticket=…&role=host）。
//
// 与普通连接的差异：
// - 不计入 conns —— 不产生 presence、不被 send_to_user 业务帧打扰；
// - 首帧 10s 内必须是 host_hello（install_id/label/platform），否则断开；
// - 同一 (user, install_id) 再连：新连接顶替，旧连接收到 host_superseded 后关闭；
// - 断开 = 宿主离线：广播 agent_host_status；running / awaiting_approval
//   置 failed/host_disconnected。queued 保持排队（A04）。

use crate::agents;
use crate::error::AppResult;
use crate::model::WsOut;
use crate::state::{ConnMsg, SharedState};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn host_session(
    mut socket: WebSocket,
    st: SharedState,
    grant: crate::auth::AccessGrant,
) -> AppResult<()> {
    let uid = grant.uid;
    // 首帧必须是 host_hello：10s 内拿不到合法的握手帧就断开
    let hello = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).ok(),
        _ => None,
    };
    let hello = match hello {
        Some(v) if v["type"] == "host_hello" => v,
        _ => {
            let _ = socket.send(Message::Close(None)).await;
            return Ok(());
        }
    };
    let Some(install_id) = hello["install_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        let _ = socket.send(Message::Close(None)).await;
        return Ok(());
    };
    let label: String = hello["label"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(60)
        .collect();
    let platform: String = hello["platform"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(60)
        .collect();

    // install_id 是桌面端永久标识：(user_id, install_id) 唯一 → 重连复用同一 host 记录。
    // label 只在第一次登记时写入；改名走 PATCH，重连不再覆盖。platform / last_seen_at 仍更新。
    let host_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agent_hosts (user_id, install_id, label, platform)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, install_id)
         DO UPDATE SET platform = EXCLUDED.platform, last_seen_at = now()
         RETURNING id",
    )
    .bind(uid)
    .bind(install_id)
    .bind(&label)
    .bind(&platform)
    .fetch_one(&st.pool)
    .await;
    let Ok(host_id) = host_id else {
        let _ = socket.send(Message::Close(None)).await;
        return Ok(());
    };

    let conn_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<ConnMsg>(crate::state::CONN_QUEUE);
    // 同 host 重复连接：旧连接收到 host_superseded + 关闭，其清理逻辑按 conn_id 校验不再生效
    st.register_host(host_id, uid, conn_id, tx.clone());

    let _ = tx.try_send(ConnMsg::Frame(
        serde_json::json!({"type": "host_ready", "host_id": host_id}).to_string(),
    ));
    broadcast_status(&st, uid, host_id, true);
    // host_ready 已入队，再按 created_at 补发排队调用（invocation_start 排在其后）
    agents::flush_queued(&st, host_id).await;

    let (mut sink, mut stream) = socket.split();
    let ping_every = Duration::from_millis(
        st.ws_ping_interval_ms
            .load(std::sync::atomic::Ordering::Relaxed)
            .max(10),
    );
    let idle_timeout = Duration::from_millis(
        st.ws_idle_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed)
            .max(1),
    );
    let mut ping = tokio::time::interval(ping_every);
    ping.tick().await;
    let mut last_inbound = Instant::now();

    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(ConnMsg::Frame(payload)) => {
                        if !send_timed(&mut sink, Message::Text(payload.into()), idle_timeout).await { break; }
                    }
                    Some(ConnMsg::Shutdown(payload)) => {
                        if send_timed(&mut sink, Message::Text(payload.into()), idle_timeout).await {
                            send_timed(&mut sink, Message::Close(None), idle_timeout).await;
                        }
                        break;
                    }
                    None => break,
                }
            }
            _ = ping.tick() => {
                if last_inbound.elapsed() > idle_timeout {
                    send_timed(&mut sink, Message::Close(None), idle_timeout).await;
                    break;
                }
                if !send_timed(&mut sink, Message::Ping(Vec::new().into()), idle_timeout).await { break; }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(t))) => {
                        last_inbound = Instant::now();
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if v["type"] == "invocation_finish" {
                                agents::handle_finish(&st, host_id, &v).await;
                            } else if v["type"] == "agent_options" {
                                agents::handle_options(&st, host_id, &v).await;
                            }
                        }
                    }
                    // Pong 等其余帧只算活跃
                    Some(Ok(_)) => last_inbound = Instant::now(),
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    // 只有我这条连接还登记在册才算离线；被顶替时新连接已接管
    if st.unregister_host(host_id, conn_id) {
        let _ = sqlx::query("UPDATE agent_hosts SET last_seen_at = now() WHERE id = $1")
            .bind(host_id)
            .execute(&st.pool)
            .await;
        broadcast_status(&st, uid, host_id, false);
        fail_running(&st, host_id).await;
    }
    Ok(())
}

async fn send_timed<S>(sink: &mut S, m: Message, limit: Duration) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    matches!(tokio::time::timeout(limit, sink.send(m)).await, Ok(Ok(())))
}

/// 宿主上下线只通知主人的普通连接（别的用户无感知）
fn broadcast_status(st: &SharedState, owner: Uuid, host_id: Uuid, online: bool) {
    let p = serde_json::to_string(&WsOut::AgentHostStatus {
        host_id,
        online,
        label: None,
    })
    .unwrap();
    st.send_to_user(owner, &p);
}

/// 宿主断开：只把 running / awaiting_approval 置 failed/host_disconnected。
/// queued 保持排队，等这个宿主下次 host_ready 再按创建顺序执行。
async fn fail_running(st: &SharedState, host_id: Uuid) {
    let invs = sqlx::query_as::<_, crate::model::InvocationView>(
        "UPDATE agent_invocations SET status = 'failed', error = 'host_disconnected',
                finished_at = now()
          WHERE finished_at IS NULL
            AND status IN ('running', 'awaiting_approval')
            AND agent_id IN (SELECT id FROM agents WHERE host_id = $1)
         RETURNING id, agent_id, conversation_id, trigger_message_id, status, error,
                   created_at, started_at, finished_at",
    )
    .bind(host_id)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for inv in invs {
        let members = crate::domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let p = serde_json::to_string(&WsOut::AgentInvocation { invocation: inv }).unwrap();
        st.send_to_users(&members, &p);
    }
}
