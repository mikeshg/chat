use crate::domain::{self, ReactionChange};
use crate::error::AppResult;
use crate::model::{UserPublic, WsIn, WsOut, WsTicketQuery};
use crate::state::{ConnMsg, SharedState};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn ws_handler(
    State(st): State<SharedState>,
    Query(q): Query<WsTicketQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let grant = st
        .tickets
        .remove(&q.ticket)
        .map(|(_, v)| v)
        .ok_or_else(|| crate::error::AppError::unauthorized("invalid ticket"))?;
    if grant.issued.elapsed() > Duration::from_secs(30) {
        return Err(crate::error::AppError::unauthorized("ticket expired"));
    }
    // ticket 签发后被禁用/改密/移出设备的缝隙：升级前再确认一次（禁用、凭据版本、登录会话都要仍有效）
    if !crate::auth::grant_valid(&st.pool, grant.uid, grant.session_version, grant.sid).await {
        return Err(crate::error::AppError::unauthorized(
            "session expired, please login again",
        ));
    }
    let access = crate::auth::AccessGrant {
        uid: grant.uid,
        session_version: grant.session_version,
        sid: grant.sid,
    };
    // Agent A01：role=host 走宿主会话（不计 presence、收 invocation_start）
    if q.role.as_deref() == Some("host") {
        return Ok(ws.on_upgrade(move |socket| async move {
            let _ = crate::host_ws::host_session(socket, st, access).await;
        }));
    }
    Ok(ws.on_upgrade(move |socket| handle_session(socket, st, access)))
}

fn frame(out: &WsOut) -> String {
    serde_json::to_string(out).unwrap()
}

/// Process an upgraded socket using the grant validated at the HTTP boundary.
pub async fn handle_session(
    mut socket: WebSocket,
    st: SharedState,
    grant: crate::auth::AccessGrant,
) {
    let uid = grant.uid;
    let conn_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<ConnMsg>(crate::state::CONN_QUEUE);
    let stalled = Arc::new(AtomicBool::new(false));

    let online_epoch = st.register_conn(uid, conn_id, tx.clone(), grant.sid, stalled.clone());

    let watchers = st.watchers(uid).await.unwrap_or_default();
    if !connection_valid(&st, uid, conn_id, grant.session_version, grant.sid).await {
        st.unregister_conn(uid, conn_id);
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    if let Some(epoch) = online_epoch {
        if st.epoch_is(uid, epoch) && st.is_online(uid) {
            let p = frame(&WsOut::Presence {
                user_id: uid,
                online: true,
                last_seen_at: None,
            });
            st.send_to_users(&watchers, &p);
        }
    }
    // 告知本连接：我的观察者中谁在线
    let online: Vec<Uuid> = watchers
        .iter()
        .copied()
        .filter(|w| st.is_online(*w))
        .collect();
    let _ = tx.try_send(ConnMsg::Frame(frame(&WsOut::PresenceInit { online })));

    let (mut sink, mut stream) = socket.split();
    // V2 P3-3：心跳超时——任何入站帧（含 Pong）都刷新 last_inbound；
    // 静默超时说明对端已死（手机断网常见），主动断开走正常断开清理。
    // 周期/超时在 AppState 上可调，测试用 spawn_named_state 后改小。
    let ping_every = Duration::from_millis(st.ws_ping_interval_ms.load(Ordering::Relaxed).max(10));
    let idle_timeout = Duration::from_millis(st.ws_idle_timeout_ms.load(Ordering::Relaxed).max(1));
    let mut ping = tokio::time::interval(ping_every);
    ping.tick().await; // 跳过立即触发
    let mut last_inbound = Instant::now();

    // 每次写都限时 idle_timeout：对端收不下（死连接把发送缓冲塞满）就断开，
    // 否则一次写卡住会饿死同一 select 里的心跳超时与踢下线
    let t = idle_timeout;
    loop {
        // 出站队列满：广播方已丢帧，断开让客户端重连补齐
        if stalled.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(ConnMsg::Frame(payload)) => {
                        if !send_timed(&mut sink, Message::Text(payload.into()), t).await {
                            break;
                        }
                    }
                    // 被踢下线：先把 force_logout 帧送到客户端，再主动关闭 socket
                    Some(ConnMsg::Shutdown(payload)) => {
                        if send_timed(&mut sink, Message::Text(payload.into()), t).await {
                            send_timed(&mut sink, Message::Close(None), t).await;
                        }
                        break;
                    }
                    None => break,
                }
            }
            _ = ping.tick() => {
                if last_inbound.elapsed() > idle_timeout {
                    send_timed(&mut sink, Message::Close(None), t).await;
                    break;
                }
                if !send_timed(&mut sink, Message::Ping(Vec::new().into()), t).await {
                    break;
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        last_inbound = Instant::now();
                        match msg {
                            Message::Text(text) => {
                                if !on_text(&st, uid, conn_id, grant.session_version, grant.sid, &tx, &text).await {
                                    send_timed(&mut sink, Message::Close(None), t).await;
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            // Ping/Pong/Binary 等其余帧只算活跃，不进业务分发
                            _ => {}
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    if let Some(epoch) = st.unregister_conn(uid, conn_id) {
        publish_offline(&st, uid, epoch).await;
    }
}

async fn send_timed<S>(sink: &mut S, m: Message, limit: Duration) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    matches!(tokio::time::timeout(limit, sink.send(m)).await, Ok(Ok(())))
}

/// 最后一个连接断开或被踢下线后广播离线。epoch 已经变了就放弃，避免盖住刚连上的设备。
pub async fn publish_offline(st: &SharedState, uid: Uuid, epoch: u64) {
    if !st.epoch_is(uid, epoch) || st.is_online(uid) {
        return;
    }
    let now = Utc::now();
    let _ = sqlx::query("UPDATE users SET last_seen_at = now() WHERE id = $1")
        .bind(uid)
        .execute(&st.pool)
        .await;
    if !st.epoch_is(uid, epoch) || st.is_online(uid) {
        return;
    }
    let watchers = st.watchers(uid).await.unwrap_or_default();
    if !st.epoch_is(uid, epoch) || st.is_online(uid) {
        return;
    }
    let p = frame(&WsOut::Presence {
        user_id: uid,
        online: false,
        last_seen_at: Some(now),
    });
    st.send_to_users(&watchers, &p);
}

fn ws_err(code: &'static str, message: impl Into<String>) -> (&'static str, String) {
    (code, message.into())
}

// ---------- 生命周期广播（api.rs REST 路径复用） ----------

/// 编辑广播：reactions 含 reacted_by_me，必须逐成员分别序列化。
pub async fn broadcast_message_updated(st: &SharedState, ed: &domain::Edited) {
    for m in &ed.member_ids {
        let view = ed.row.clone().into_view(*m);
        st.send_to_user(
            *m,
            &frame(&WsOut::MessageUpdated {
                conversation_id: ed.conversation_id,
                message: view,
                edit_version: ed.edit_version,
            }),
        );
    }
}

/// 软删除广播：完整墓碑视图逐成员成帧（删除后 reactions 已清空，各成员等价，
/// 但统一走 into_view 保持协议形状一致）。
pub fn broadcast_message_deleted(st: &SharedState, d: &domain::Deleted) {
    for m in &d.member_ids {
        let view = d.row.clone().into_view(*m);
        st.send_to_user(
            *m,
            &frame(&WsOut::MessageDeleted {
                conversation_id: d.conversation_id,
                edit_version: view.edit_version,
                message: view,
            }),
        );
    }
}

/// reaction 变更广播：完整视图逐成员成帧（reactions.reacted_by_me 按接收者计算）。
/// 携带整视图是关键——部分字段帧的全局版本会吞掉反序到达的完整编辑。
pub fn broadcast_reaction_update(st: &SharedState, ch: &ReactionChange) {
    for m in &ch.member_ids {
        let view = ch.row.clone().into_view(*m);
        st.send_to_user(
            *m,
            &frame(&WsOut::ReactionUpdate {
                conversation_id: ch.conversation_id,
                edit_version: view.edit_version,
                message: view,
            }),
        );
    }
}

/// 新消息广播：成员各连一帧 + 被 @ 解归档成员的个人偏好帧。
/// WS send_message / REST create_poll / REST forward 三处共用
/// （第 3 阶段离线推送也挂在它上面，所有新消息必须经过这里）。
pub async fn broadcast_posted(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    if posted.inserted {
        let p = frame(&WsOut::Message {
            message: posted.message.clone(),
        });
        st.send_to_users(&posted.member_ids, &p);
        // 发送者已读位置在事务内推进到这条：同步给他的其他设备清角标。
        // Agent 消息没有推进主人 last_read_seq（对主人是「别人发来的」），不发 read_sync。
        if posted.message.agent.is_none() {
            st.send_to_user(
                posted.message.sender_id,
                &frame(&WsOut::ReadSync {
                    conversation_id: conv,
                    seq: posted.message.seq,
                }),
            );
        }
        // V2 P3-3：离线推送（ntfy）——判定与发送全在 spawned 任务里，不拖慢广播
        crate::push::schedule(st, conv, posted);
        // Agent A01/A11：文字消息落库后的唯一调用挂点（专属会话，或私聊/群里的 @）
        crate::agents::maybe_invoke(st, conv, posted).await;
    }
    // 新 @本人消息已在事务内解除其归档：偏好帧只发给本人全部连接（其他成员不可见）
    for target in &posted.unarchived {
        if let Ok(preference) = domain::pref_of(&st.pool, conv, *target).await {
            st.send_to_user(
                *target,
                &frame(&WsOut::PreferenceUpdated {
                    conversation_id: conv,
                    preference,
                }),
            );
        }
    }
}

/// mutation 回执：只回发起连接，与 client_msg_id 严格区分。
fn op_ack(tx: &mpsc::Sender<ConnMsg>, client_op_id: Option<Uuid>) {
    if let Some(id) = client_op_id {
        let _ = tx.try_send(ConnMsg::Frame(frame(&WsOut::OpAck { client_op_id: id })));
    }
}

/// 每帧复核：账号未禁用、凭据版本未变、所属登录会话未撤销（老连接无 sid 不查 session）
async fn connection_valid(
    st: &SharedState,
    uid: Uuid,
    conn_id: Uuid,
    session_version: i64,
    sid: Option<Uuid>,
) -> bool {
    crate::auth::grant_valid(&st.pool, uid, session_version, sid).await
        && st.conn_alive(uid, conn_id)
}

async fn on_text(
    st: &SharedState,
    uid: Uuid,
    conn_id: Uuid,
    session_version: i64,
    sid: Option<Uuid>,
    tx: &mpsc::Sender<ConnMsg>,
    text: &str,
) -> bool {
    if !connection_valid(st, uid, conn_id, session_version, sid).await {
        return false;
    }
    let parsed: Result<WsIn, _> = serde_json::from_str(text);
    let (res, client_msg_id, client_op_id) = match parsed {
        Ok(WsIn::SendMessage {
            conversation_id,
            client_msg_id,
            content,
            attachment_id,
            reply_to_id,
            agent_targets,
            ..
        }) => {
            let res = send_message(
                st,
                uid,
                tx,
                conversation_id,
                client_msg_id.clone(),
                &content,
                attachment_id,
                reply_to_id,
                agent_targets,
            )
            .await;
            (res, client_msg_id, None)
        }
        Ok(WsIn::EditMessage {
            message_id,
            content,
            expected_version,
            client_op_id,
        }) => {
            let res = edit_message(
                st,
                uid,
                tx,
                message_id,
                &content,
                expected_version,
                client_op_id.clone(),
            )
            .await;
            (res, None, client_op_id)
        }
        Ok(WsIn::DeleteMessage {
            message_id,
            client_op_id,
        }) => {
            let res = delete_message(st, uid, tx, message_id, client_op_id.clone()).await;
            (res, None, client_op_id)
        }
        Ok(WsIn::ReactionUpsert {
            message_id,
            emoji,
            client_op_id,
        }) => {
            let res = reaction(st, uid, tx, message_id, &emoji, true, client_op_id.clone()).await;
            (res, None, client_op_id)
        }
        Ok(WsIn::ReactionRemove {
            message_id,
            emoji,
            client_op_id,
        }) => {
            let res = reaction(st, uid, tx, message_id, &emoji, false, client_op_id.clone()).await;
            (res, None, client_op_id)
        }
        Ok(WsIn::ClientState { active }) => {
            st.set_conn_active(uid, conn_id, active);
            (Ok(()), None, None)
        }
        Ok(WsIn::Typing { conversation_id }) => {
            (typing(st, uid, conversation_id).await, None, None)
        }
        Ok(WsIn::Read {
            conversation_id,
            seq,
        }) => (mark_read(st, uid, conversation_id, seq).await, None, None),
        Ok(WsIn::Sync {
            conversation_id,
            last_seq,
        }) => (
            sync(st, uid, tx, conversation_id, last_seq).await,
            None,
            None,
        ),
        Err(_) => (Err(ws_err("bad_frame", "unrecognized frame")), None, None),
    };
    if let Err((code, message)) = res {
        let _ = tx.try_send(ConnMsg::Frame(frame(&WsOut::Error {
            code,
            message,
            client_msg_id,
            client_op_id,
        })));
    }
    true
}

type WsResult = Result<(), (&'static str, String)>;

async fn send_message(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::Sender<ConnMsg>,
    conv: Uuid,
    client_msg_id: Option<String>,
    content: &str,
    attachment_id: Option<Uuid>,
    reply_to_id: Option<Uuid>,
    agent_targets: Vec<Uuid>,
) -> WsResult {
    let mut posted = domain::post_message(
        &st.pool,
        domain::PostMessage {
            conversation_id: conv,
            sender_id: uid,
            content,
            client_msg_id: client_msg_id.as_deref(),
            attachment_id,
            reply_to_id,
            // WS 不接受投票负载：kind='poll' 只能经 REST create_poll 产生（防伪造）
            poll: None,
            // 转发字段仅服务端转发端点可达：WS 发送不能伪造转发来源/克隆附件
            forwarded_from: None,
            forward_attachment: None,
            // agent_id 仅服务端宿主回写路径可达：用户 WS 不能伪造 Agent 身份
            agent_id: None,
            agent_steps: None,
            approval: None,
        },
    )
    .await
    .map_err(domain::PostError::into_ws)?;
    posted.agent_targets = agent_targets;

    broadcast_posted(st, conv, &posted).await;
    // ack 只回发起连接：携带 client_msg_id 完成乐观替换
    let _ = tx.try_send(ConnMsg::Frame(frame(&WsOut::MessageAck {
        client_msg_id,
        message: posted.message,
    })));
    Ok(())
}

async fn edit_message(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::Sender<ConnMsg>,
    message_id: Uuid,
    content: &str,
    expected_version: i32,
    client_op_id: Option<Uuid>,
) -> WsResult {
    let ed = domain::edit_message(
        &st.pool,
        message_id,
        uid,
        content,
        expected_version,
        client_op_id,
    )
    .await
    .map_err(domain::PostError::into_ws)?;
    broadcast_message_updated(st, &ed).await;
    op_ack(tx, client_op_id);
    Ok(())
}

async fn delete_message(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::Sender<ConnMsg>,
    message_id: Uuid,
    client_op_id: Option<Uuid>,
) -> WsResult {
    let d = domain::delete_message(&st.pool, message_id, uid)
        .await
        .map_err(domain::PostError::into_ws)?;
    broadcast_message_deleted(st, &d);
    op_ack(tx, client_op_id);
    crate::api::spawn_file_cleanup(st, d.orphan_files);
    Ok(())
}

async fn reaction(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::Sender<ConnMsg>,
    message_id: Uuid,
    emoji: &str,
    add: bool,
    client_op_id: Option<Uuid>,
) -> WsResult {
    let ch = domain::set_reaction(&st.pool, message_id, uid, emoji, add)
        .await
        .map_err(domain::PostError::into_ws)?;
    broadcast_reaction_update(st, &ch);
    op_ack(tx, client_op_id);
    Ok(())
}

async fn typing(st: &SharedState, uid: Uuid, conv: Uuid) -> WsResult {
    // self/agent 会话不广播 typing（唯一成员即本人；显式短路，省得查成员再空转）
    if matches!(
        domain::conv_kind(&st.pool, conv)
            .await
            .map_err(|_| ws_err("internal", "db error"))?
            .as_deref(),
        Some("self") | Some("agent")
    ) {
        return Ok(());
    }
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ws_err("internal", "db error"))?
        .ok_or_else(|| ws_err("forbidden", "not a member"))?;
    let user = user_public(st, uid).await?;
    let members = domain::member_ids(&st.pool, conv)
        .await
        .map_err(|_| ws_err("internal", "db error"))?;
    let p = frame(&WsOut::Typing {
        conversation_id: conv,
        user,
    });
    for m in members.into_iter().filter(|m| *m != uid) {
        st.send_to_user(m, &p);
    }
    Ok(())
}

async fn mark_read(st: &SharedState, uid: Uuid, conv: Uuid, seq: i64) -> WsResult {
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ws_err("internal", "db error"))?
        .ok_or_else(|| ws_err("forbidden", "not a member"))?;
    // 只推进不回退；回显实际生效的 seq（可能因 last_seq 上限被收敛），无变化则不广播
    let applied = sqlx::query_scalar::<_, i64>(
        "UPDATE memberships SET last_read_seq = LEAST($3, (SELECT last_seq FROM conversations WHERE id = $1))
         WHERE conversation_id = $1 AND user_id = $2 AND last_read_seq < $3
         RETURNING last_read_seq",
    )
    .bind(conv)
    .bind(uid)
    .bind(seq)
    .fetch_optional(&st.pool)
    .await
    .map_err(|_| ws_err("internal", "db error"))?;
    if let Some(applied) = applied {
        // 同步到我的其他设备（清除未读角标）
        st.send_to_user(
            uid,
            &frame(&WsOut::ReadSync {
                conversation_id: conv,
                seq: applied,
            }),
        );
    }
    Ok(())
}

/// sync 单页上限；缺口更大时 has_more=true，客户端按新水位续拉
const SYNC_PAGE: i64 = 500;

async fn sync(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::Sender<ConnMsg>,
    conv: Uuid,
    last_seq: i64,
) -> WsResult {
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ws_err("internal", "db error"))?
        .ok_or_else(|| ws_err("forbidden", "not a member"))?;
    // 多取一条探测是否还有后续页
    let mut messages = domain::messages_after(&st.pool, conv, uid, last_seq, SYNC_PAGE + 1)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "sync failed");
            ws_err("internal", "db error")
        })?;
    let has_more = messages.len() as i64 > SYNC_PAGE;
    messages.truncate(SYNC_PAGE as usize);
    let _ = tx.try_send(ConnMsg::Frame(frame(&WsOut::SyncMessages {
        conversation_id: conv,
        messages,
        has_more,
    })));
    Ok(())
}

async fn user_public(st: &SharedState, uid: Uuid) -> Result<UserPublic, (&'static str, String)> {
    sqlx::query_as::<_, (String, String, String, Option<chrono::DateTime<Utc>>, i32, bool)>(
        "SELECT username::text, display_name, role, last_seen_at, avatar_version, avatar_path IS NOT NULL
           FROM users WHERE id = $1",
    )
    .bind(uid)
    .fetch_one(&st.pool)
    .await
    .map(
        |(username, display_name, role, last_seen_at, avatar_version, has_avatar)| UserPublic {
            id: uid,
            username,
            display_name,
            role,
            last_seen_at,
            avatar_version,
            has_avatar,
            agent_access: None,
        },
    )
    .map_err(|_| ws_err("internal", "db error"))
}
