// Agent A01：宿主专用 WS 会话（/v1/ws?ticket=…&role=host）。
//
// 与普通连接的差异：
// - 不计入 conns —— 不产生 presence、不被 send_to_user 业务帧打扰；
// - 首帧 10s 内必须是 host_hello（install_id/label/platform），否则断开；
// - 同一 (user, install_id) 再连：新连接顶替，旧连接收到 host_superseded 后关闭；
// - 断开 = 宿主离线：广播 agent_host_status，记下 offline_since。
//   running / awaiting_approval 保持原状态（A23），等重连核对或宽限到期。
//   queued 保持排队（A04）。

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
    let sid = grant.sid;
    // 握手（WS 升级）之后立刻看使用权。无权则关闭、不登记、不写宿主行。
    if !crate::agent_access::user_allows(&st.pool, uid)
        .await
        .unwrap_or(false)
    {
        let _ = socket.send(Message::Close(None)).await;
        return Ok(());
    }
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
    // 旧宿主不带这两个字段，按空处理：重连时进行中的调用一律视为没报告。
    let reported = hello_uuids(&hello, "running");
    let pending_approvals = hello_strings(&hello, "pending_approvals");

    // install_id 是桌面端永久标识：(user_id, install_id) 唯一 → 重连复用同一 host 记录。
    // label 只在第一次登记时写入；改名走 PATCH，重连不再覆盖。platform / last_seen_at 仍更新。
    // 重连清掉离线宽限起点。
    let host_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agent_hosts (user_id, install_id, label, platform)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, install_id)
         DO UPDATE SET platform = EXCLUDED.platform, last_seen_at = now(), offline_since = NULL
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
    st.register_host(host_id, uid, conn_id, sid, tx.clone());

    // A24：登记后立刻复核一次（与 ws.rs connection_valid 同条件）。升级后、hello 前
    // 被撤销/禁用/改密的连接，kick_session 只断开已登记宿主会漏掉它——这里兜底：
    // 不通过就摘掉登记、关连接，不发 host_ready、不补发排队调用、不广播上线。
    if !crate::auth::grant_valid(&st.pool, uid, grant.session_version, sid).await {
        st.unregister_host(host_id, conn_id);
        let _ = socket.send(Message::Close(None)).await;
        return Ok(());
    }

    let _ = tx.try_send(ConnMsg::Frame(
        serde_json::json!({"type": "host_ready", "host_id": host_id}).to_string(),
    ));
    // host_ready 已入队。先按宿主报告的进行中调用核对，再补发排队。
    // 取消帧和审批决定排在 host_ready 之后、排队的 invocation_start 之前。
    agents::on_host_ready(&st, host_id, &reported, &pending_approvals).await;
    broadcast_status(&st, uid, host_id, true);
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
                // A24：每个 ping 周期复核一次登录会话（禁用/改密/移出设备按时断开）；
                // 不逐帧查库——流式事件可达 ~100ms/帧。失败走正常断开收尾。
                if !crate::auth::grant_valid(&st.pool, uid, grant.session_version, sid).await {
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
                            match v["type"].as_str().unwrap_or_default() {
                                "invocation_finish" => {
                                    agents::handle_finish(&st, host_id, &v).await;
                                }
                                // A03：步骤/增量事件（在 handler 内核验调用归属本宿主）
                                "invocation_event" => {
                                    agents::handle_event(&st, host_id, &v).await;
                                }
                                "agent_options" => {
                                    agents::handle_options(&st, host_id, &v).await;
                                }
                                // A07：登记表查询结果 / 安装进度；试读可选项的结果同样按 request_id 配对
                                "registry_result" | "probe_result" => {
                                    agents::handle_registry_result(&st, &v)
                                }
                                "registry_install_progress" => {
                                    agents::handle_registry_progress(&st, host_id, &v);
                                }
                                // A12：超档审批。归属校验在 handler 里。
                                "approval_request" => {
                                    crate::approvals::handle_request(&st, host_id, &v).await;
                                }
                                _ => {}
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
        // 宽限从这一刻起算。已经有起点（重复断开）就保留原来的。
        let _ = sqlx::query(
            "UPDATE agent_hosts
                SET last_seen_at = now(), offline_since = COALESCE(offline_since, now())
              WHERE id = $1",
        )
        .bind(host_id)
        .execute(&st.pool)
        .await;
        broadcast_status(&st, uid, host_id, false);
    }
    Ok(())
}

fn hello_uuids(v: &Value, key: &str) -> Vec<Uuid> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn hello_strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.chars().take(80).collect::<String>())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
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
