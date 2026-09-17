use crate::domain;
use crate::error::AppResult;
use crate::model::{AttachmentView, UserPublic, WsIn, WsOut, WsTicketQuery};
use crate::state::SharedState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn ws_handler(
    State(st): State<SharedState>,
    Query(q): Query<WsTicketQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let (uid, issued) = st
        .tickets
        .remove(&q.ticket)
        .map(|(_, v)| v)
        .ok_or_else(|| crate::error::AppError::unauthorized("invalid ticket"))?;
    if issued.elapsed() > Duration::from_secs(30) {
        return Err(crate::error::AppError::unauthorized("ticket expired"));
    }
    Ok(ws.on_upgrade(move |socket| handle(socket, st, uid)))
}

fn frame(out: &WsOut) -> String {
    serde_json::to_string(out).unwrap()
}

async fn handle(socket: WebSocket, st: SharedState, uid: Uuid) {
    let conn_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let first_conn = {
        st.conns.entry(uid).or_default().insert(conn_id, tx.clone());
        st.conn_count(uid) == 1
    };

    let watchers = st.watchers(uid).await.unwrap_or_default();
    if first_conn {
        let p = frame(&WsOut::Presence {
            user_id: uid,
            online: true,
            last_seen_at: None,
        });
        st.send_to_users(&watchers, &p);
    }
    // 告知本连接：我的观察者中谁在线
    let online: Vec<Uuid> = watchers
        .iter()
        .copied()
        .filter(|w| st.is_online(*w))
        .collect();
    let _ = tx.send(frame(&WsOut::PresenceInit { online }));

    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await; // 跳过立即触发

    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(payload) => {
                        if sink.send(Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        on_text(&st, uid, &tx, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    // 清理连接；最后一个断开 → 落 last_seen + 广播离线
    let last = {
        if let Some(m) = st.conns.get(&uid) {
            m.remove(&conn_id);
        }
        let empty = st.conn_count(uid) == 0;
        if empty {
            st.conns.remove(&uid);
        }
        empty
    };
    if last {
        let now = Utc::now();
        let _ = sqlx::query("UPDATE users SET last_seen_at = now() WHERE id = $1")
            .bind(uid)
            .execute(&st.pool)
            .await;
        let p = frame(&WsOut::Presence {
            user_id: uid,
            online: false,
            last_seen_at: Some(now),
        });
        st.send_to_users(&watchers, &p);
    }
}

async fn on_text(st: &SharedState, uid: Uuid, tx: &mpsc::UnboundedSender<String>, text: &str) {
    let parsed: Result<WsIn, _> = serde_json::from_str(text);
    let res = match parsed {
        Ok(WsIn::SendMessage {
            conversation_id,
            client_msg_id,
            content,
            kind,
            attachment_id,
        }) => {
            send_message(
                st,
                uid,
                tx,
                conversation_id,
                client_msg_id,
                &content,
                kind,
                attachment_id,
            )
            .await
        }
        Ok(WsIn::Typing { conversation_id }) => typing(st, uid, conversation_id).await,
        Ok(WsIn::Read {
            conversation_id,
            seq,
        }) => mark_read(st, uid, conversation_id, seq).await,
        Ok(WsIn::Sync {
            conversation_id,
            last_seq,
        }) => sync(st, uid, tx, conversation_id, last_seq).await,
        Err(_) => Err(("bad_frame", "unrecognized frame")),
    };
    if let Err((code, message)) = res {
        let _ = tx.send(frame(&WsOut::Error {
            code,
            message: message.to_string(),
        }));
    }
}

type WsResult = Result<(), (&'static str, &'static str)>;

async fn send_message(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::UnboundedSender<String>,
    conv: Uuid,
    client_msg_id: Option<String>,
    content: &str,
    _kind: Option<String>,
    attachment_id: Option<Uuid>,
) -> WsResult {
    let content = content.trim();
    if content.chars().count() > 4000 {
        return Err(("bad_message", "message too long (max 4000 chars)"));
    }
    // 附件归属校验：必须是本人上传、绑定到本会话、且未被其他消息使用
    let attachment = if let Some(att_id) = attachment_id {
        let row = sqlx::query_as::<_, (Uuid, String, String, i64, Uuid)>(
            "SELECT id, file_name, mime, size, conv_id FROM attachments
             WHERE id = $1 AND uploader_id = $2",
        )
        .bind(att_id)
        .bind(uid)
        .fetch_optional(&st.pool)
        .await
        .map_err(|_| ("internal", "db error"))?
        .ok_or(("not_found", "attachment not found"))?;
        if row.4 != conv {
            return Err(("bad_attachment", "attachment belongs to another conversation"));
        }
        let used = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE attachment_id = $1)",
        )
        .bind(att_id)
        .fetch_one(&st.pool)
        .await
        .map_err(|_| ("internal", "db error"))?;
        if used {
            return Err(("bad_attachment", "attachment already sent"));
        }
        Some(AttachmentView {
            id: row.0,
            file_name: row.1,
            mime: row.2,
            size: row.3,
        })
    } else {
        None
    };
    if attachment.is_none() && content.is_empty() {
        return Err(("bad_message", "message must be 1-4000 chars"));
    }
    let kind: &str = match &attachment {
        Some(a) if a.mime.starts_with("image/") => "image",
        Some(_) => "file",
        None => "text",
    };
    let content: String = if content.is_empty() {
        match kind {
            "image" => "[图片]".into(),
            _ => format!("[文件] {}", attachment.as_ref().unwrap().file_name),
        }
    } else {
        content.to_string()
    };
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ("internal", "db error"))?
        .ok_or(("forbidden", "not a member of this conversation"))?;

    // DM：好友关系解除后只读
    if domain::conv_kind(&st.pool, conv)
        .await
        .map_err(|_| ("internal", "db error"))?
        .as_deref()
        == Some("dm")
    {
        let others = domain::member_ids(&st.pool, conv)
            .await
            .map_err(|_| ("internal", "db error"))?;
        if let Some(peer) = others.iter().copied().find(|u| *u != uid) {
            let still = domain::is_friend(&st.pool, uid, peer)
                .await
                .map_err(|_| ("internal", "db error"))?;
            if !still {
                return Err(("not_friends", "好友关系已解除，会话只读"));
            }
        }
    }

    let mut db_tx = st
        .pool
        .begin()
        .await
        .map_err(|_| ("internal", "db error"))?;
    let msg = domain::append_message(&mut db_tx, conv, uid, &content, kind, attachment)
        .await
        .map_err(|_| ("internal", "db error"))?;
    db_tx.commit().await.map_err(|_| ("internal", "db error"))?;

    let members = domain::member_ids(&st.pool, conv)
        .await
        .map_err(|_| ("internal", "db error"))?;
    let p = frame(&WsOut::Message {
        message: msg.clone(),
    });
    st.send_to_users(&members, &p);
    // ack 只回发起连接：携带 client_msg_id 完成乐观替换
    let _ = tx.send(frame(&WsOut::MessageAck {
        client_msg_id,
        message: msg,
    }));
    Ok(())
}

async fn typing(st: &SharedState, uid: Uuid, conv: Uuid) -> WsResult {
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ("internal", "db error"))?
        .ok_or(("forbidden", "not a member"))?;
    let user = user_public(st, uid).await?;
    let members = domain::member_ids(&st.pool, conv)
        .await
        .map_err(|_| ("internal", "db error"))?;
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
        .map_err(|_| ("internal", "db error"))?
        .ok_or(("forbidden", "not a member"))?;
    sqlx::query(
        "UPDATE memberships SET last_read_seq = LEAST($3, (SELECT last_seq FROM conversations WHERE id = $1))
         WHERE conversation_id = $1 AND user_id = $2 AND last_read_seq < $3",
    )
    .bind(conv)
    .bind(uid)
    .bind(seq)
    .execute(&st.pool)
    .await
    .map_err(|_| ("internal", "db error"))?;
    // 同步到我的其他设备（清除未读角标）
    st.send_to_user(
        uid,
        &frame(&WsOut::ReadSync {
            conversation_id: conv,
            seq,
        }),
    );
    Ok(())
}

async fn sync(
    st: &SharedState,
    uid: Uuid,
    tx: &mpsc::UnboundedSender<String>,
    conv: Uuid,
    last_seq: i64,
) -> WsResult {
    domain::membership(&st.pool, conv, uid)
        .await
        .map_err(|_| ("internal", "db error"))?
        .ok_or(("forbidden", "not a member"))?;
    let rows = sqlx::query_as::<_, domain::MsgViewRow>(&format!(
        "SELECT {} {} WHERE m.conversation_id = $1 AND m.seq > $2
          ORDER BY m.seq ASC LIMIT 500",
        domain::MsgViewRow::SELECT,
        domain::MsgViewRow::JOIN
    ))
    .bind(conv)
    .bind(last_seq)
    .fetch_all(&st.pool)
    .await
    .map_err(|_| ("internal", "db error"))?;
    let _ = tx.send(frame(&WsOut::SyncMessages {
        conversation_id: conv,
        messages: rows.into_iter().map(|r| r.into_view()).collect(),
    }));
    Ok(())
}

async fn user_public(
    st: &SharedState,
    uid: Uuid,
) -> Result<UserPublic, (&'static str, &'static str)> {
    sqlx::query_as::<_, (String, String, Option<chrono::DateTime<Utc>>)>(
        "SELECT username::text, display_name, last_seen_at FROM users WHERE id = $1",
    )
    .bind(uid)
    .fetch_one(&st.pool)
    .await
    .map(|(username, display_name, last_seen_at)| UserPublic {
        id: uid,
        username,
        display_name,
        last_seen_at,
    })
    .map_err(|_| ("internal", "db error"))
}
