use crate::auth;
use crate::domain;
use crate::error::{is_unique_violation, AppError, AppResult};
use crate::model::*;
use crate::state::SharedState;
use crate::ws;
use axum::{
    extract::{ConnectInfo, Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Instant;
use uuid::Uuid;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/refresh", post(refresh))
        .route("/auth/logout", post(logout))
        .route("/users/me", get(get_me).patch(update_me))
        .route("/users/search", get(search_user))
        .route("/friends", get(list_friends))
        .route("/friends/{user_id}", delete(delete_friend))
        .route("/friends/requests", get(list_requests).post(create_request))
        .route("/friends/requests/{id}/accept", post(accept_request))
        .route("/friends/requests/{id}/decline", post(decline_request))
        .route("/conversations", get(list_conversations))
        .route("/conversations/group", post(create_group))
        .route(
            "/conversations/{id}",
            get(get_conversation)
                .patch(rename_conversation)
                .delete(dissolve_conversation),
        )
        .route("/conversations/{id}/messages", get(list_messages))
        .route("/conversations/{id}/messages/search", get(search_messages))
        .route("/conversations/{id}/members", post(add_members))
        .route("/conversations/{id}/members/me", delete(leave_conversation))
        .route("/conversations/{id}/members/{uid}", delete(kick_member))
        .route("/conversations/{id}/transfer", post(transfer_ownership))
        .route("/files", post(upload_file))
        .route("/files/{id}", get(download_file))
        .route("/ws/ticket", get(ws_ticket))
        .route("/ws", get(ws::ws_handler))
}

fn uid_of(headers: &HeaderMap, st: &SharedState) -> AppResult<Uuid> {
    auth::bearer_uid(headers, &st.cfg)
}

fn notify(st: &SharedState, uids: &[Uuid], what: &'static str) {
    let payload = serde_json::to_string(&WsOut::Invalidate { what }).unwrap();
    st.send_to_users(uids, &payload);
}

/// IP 滑动窗口限流（注册/登录）：20 次 / 5 分钟
fn rate_limit(st: &SharedState, key: String) -> AppResult<()> {
    const LIMIT: usize = 20;
    const WINDOW_SECS: u64 = 300;
    let mut entry = st.rate.entry(key).or_default();
    entry.retain(|t| t.elapsed().as_secs() < WINDOW_SECS);
    if entry.len() >= LIMIT {
        return Err(AppError::bad_request("too many attempts, try later"));
    }
    entry.push(Instant::now());
    Ok(())
}

// ---------- auth ----------

fn valid_username(s: &str) -> bool {
    (3..=20).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn register(
    State(st): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<RegisterReq>,
) -> AppResult<Json<AuthResponse>> {
    rate_limit(&st, format!("reg:{}", addr.ip()))?;
    let username = req.username.trim();
    if !valid_username(username) {
        return Err(AppError::bad_request(
            "username must be 3-20 chars of [a-zA-Z0-9_]",
        ));
    }
    if !(6..=72).contains(&req.password.len()) {
        return Err(AppError::bad_request("password must be 6-72 chars"));
    }
    let display_name = req
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 50)
        .unwrap_or(username)
        .to_string();

    let hash = auth::hash_password(&req.password)?;
    let row = sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, display_name, password_hash)
         VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(username)
    .bind(&display_name)
    .bind(&hash)
    .fetch_one(&st.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            AppError::conflict("username already taken")
        } else {
            e.into()
        }
    })?;

    let user = UserPublic::from(&row);
    let res = auth::issue_tokens(&st.pool, &st.cfg, &user, None).await?;
    Ok(Json(res))
}

async fn login(
    State(st): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> AppResult<Json<AuthResponse>> {
    rate_limit(&st, format!("login:{}", addr.ip()))?;
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE username = $1")
        .bind(req.username.trim())
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::unauthorized("invalid credentials"))?;

    if !auth::verify_password(&row.password_hash, &req.password) {
        return Err(AppError::unauthorized("invalid credentials"));
    }
    let user = UserPublic::from(&row);
    Ok(Json(
        auth::issue_tokens(&st.pool, &st.cfg, &user, None).await?,
    ))
}

async fn refresh(
    State(st): State<SharedState>,
    Json(req): Json<RefreshReq>,
) -> AppResult<Json<AuthResponse>> {
    let hash = auth::refresh_hash(&req.refresh_token);
    let tok = sqlx::query_as::<_, (Uuid, Uuid, DateTime<Utc>, Option<DateTime<Utc>>)>(
        "SELECT id, user_id, expires_at, revoked_at FROM refresh_tokens WHERE token_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::unauthorized("invalid refresh token"))?;

    let (tok_id, user_id, expires_at, revoked_at) = tok;
    if revoked_at.is_some() || expires_at < Utc::now() {
        return Err(AppError::unauthorized("refresh token expired"));
    }
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE id = $1")
        .bind(tok_id)
        .execute(&st.pool)
        .await?;

    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&st.pool)
        .await?;
    let user = UserPublic::from(&row);
    Ok(Json(
        auth::issue_tokens(&st.pool, &st.cfg, &user, None).await?,
    ))
}

async fn logout(
    State(st): State<SharedState>,
    Json(req): Json<LogoutReq>,
) -> AppResult<StatusCode> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(auth::refresh_hash(&req.refresh_token))
    .execute(&st.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- users ----------

async fn get_me(State(st): State<SharedState>, headers: HeaderMap) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st)?;
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(UserPublic::from(&row)))
}

async fn update_me(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<UpdateMeReq>,
) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st)?;
    let name = req.display_name.trim();
    if name.is_empty() || name.len() > 50 {
        return Err(AppError::bad_request("display_name must be 1-50 chars"));
    }
    let row = sqlx::query_as::<_, UserRow>(
        "UPDATE users SET display_name = $2 WHERE id = $1 RETURNING *",
    )
    .bind(uid)
    .bind(name)
    .fetch_one(&st.pool)
    .await?;
    Ok(Json(UserPublic::from(&row)))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

/// 用户搜索：username/display_name 模糊匹配（不含自己），精确命中排前
async fn search_user(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<Vec<UserPublic>>> {
    let uid = uid_of(&headers, &st)?;
    let needle = q.q.trim();
    if needle.is_empty() {
        return Ok(Json(vec![]));
    }
        let esc = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users
          WHERE id <> $1 AND (username ILIKE '%' || $2 || '%' ESCAPE '\\'
                              OR display_name ILIKE '%' || $2 || '%' ESCAPE '\\')
          ORDER BY (username = $3 OR display_name = $3) DESC, username
          LIMIT 20",
    )
    .bind(uid)
    .bind(esc)
    .bind(needle)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(UserPublic::from).collect()))
}

// ---------- friends ----------

#[derive(sqlx::FromRow)]
struct FriendRow {
    id: Uuid,
    username: String,
    display_name: String,
    last_seen_at: Option<DateTime<Utc>>,
    since: DateTime<Utc>,
}

async fn list_friends(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<FriendView>>> {
    let uid = uid_of(&headers, &st)?;
    let rows = sqlx::query_as::<_, FriendRow>(
        "SELECT u.id, u.username::text AS username, u.display_name, u.last_seen_at,
                f.created_at AS since
           FROM friendships f
           JOIN users u ON u.id = CASE WHEN f.user_a = $1 THEN f.user_b ELSE f.user_a END
          WHERE f.user_a = $1 OR f.user_b = $1
          ORDER BY u.username",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| FriendView {
                user: UserPublic {
                    id: r.id,
                    username: r.username,
                    display_name: r.display_name,
                    last_seen_at: r.last_seen_at,
                },
                since: r.since,
            })
            .collect(),
    ))
}

async fn delete_friend(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(friend_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    let (lo, hi) = domain::canonical_pair(uid, friend_id);
    let res = sqlx::query("DELETE FROM friendships WHERE user_a = $1 AND user_b = $2")
        .bind(lo)
        .bind(hi)
        .execute(&st.pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("not friends"));
    }
    notify(&st, &[uid, friend_id], "friends");
    notify(&st, &[uid, friend_id], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct ReqRow {
    id: Uuid,
    message: String,
    created_at: DateTime<Utc>,
    cp_id: Uuid,
    cp_username: String,
    cp_display: String,
    cp_last_seen: Option<DateTime<Utc>>,
}

async fn list_requests(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st)?;
    let incoming = sqlx::query_as::<_, ReqRow>(
        "SELECT r.id, r.message, r.created_at,
                u.id AS cp_id, u.username::text AS cp_username, u.display_name AS cp_display,
                u.last_seen_at AS cp_last_seen
           FROM friend_requests r JOIN users u ON u.id = r.from_user
          WHERE r.to_user = $1 AND r.status = 'pending'
          ORDER BY r.created_at DESC",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    let outgoing = sqlx::query_as::<_, ReqRow>(
        "SELECT r.id, r.message, r.created_at,
                u.id AS cp_id, u.username::text AS cp_username, u.display_name AS cp_display,
                u.last_seen_at AS cp_last_seen
           FROM friend_requests r JOIN users u ON u.id = r.to_user
          WHERE r.from_user = $1 AND r.status = 'pending'
          ORDER BY r.created_at DESC",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;

    let map = |rows: Vec<ReqRow>, dir: &'static str| -> Vec<FriendRequestView> {
        rows.into_iter()
            .map(|r| FriendRequestView {
                id: r.id,
                user: UserPublic {
                    id: r.cp_id,
                    username: r.cp_username,
                    display_name: r.cp_display,
                    last_seen_at: r.cp_last_seen,
                },
                message: r.message,
                direction: dir,
                created_at: r.created_at,
            })
            .collect()
    };
    Ok(Json(json!({
        "incoming": map(incoming, "in"),
        "outgoing": map(outgoing, "out"),
    })))
}

async fn create_request(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<FriendReqCreate>,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st)?;
    let to = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE username = $1")
        .bind(req.username.trim())
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("user not found"))?;

    if to.id == uid {
        return Err(AppError::bad_request("cannot add yourself"));
    }
    if domain::is_friend(&st.pool, uid, to.id).await? {
        return Err(AppError::conflict("already friends"));
    }

    // 对方已有向我发的 pending 申请 → 直接互相接受
    let reverse = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM friend_requests
         WHERE from_user = $1 AND to_user = $2 AND status = 'pending'",
    )
    .bind(to.id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    if let Some(rid) = reverse {
        accept_internal(&st, rid).await?;
        return Ok(Json(json!({ "status": "accepted" })));
    }

    let msg = req.message.trim();
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO friend_requests (from_user, to_user, message)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(uid)
    .bind(to.id)
    .bind(if msg.len() > 200 { &msg[..200] } else { msg })
    .fetch_one(&st.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            AppError::conflict("request already pending")
        } else {
            e.into()
        }
    })?;

    notify(&st, &[to.id], "requests");
    Ok(Json(json!({ "status": "pending", "id": id })))
}

/// 接受申请：置状态 → 建好友关系 → 确保 DM 会话。返回 (from_user, to_user)。
async fn accept_internal(st: &SharedState, req_id: Uuid) -> AppResult<(Uuid, Uuid)> {
    let mut tx = st.pool.begin().await?;
    let pair = sqlx::query_as::<_, (Uuid, Uuid)>(
        "UPDATE friend_requests SET status = 'accepted', resolved_at = now()
         WHERE id = $1 AND status = 'pending'
         RETURNING from_user, to_user",
    )
    .bind(req_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::not_found("request not found or resolved"))?;

    let (lo, hi) = domain::canonical_pair(pair.0, pair.1);
    sqlx::query("INSERT INTO friendships (user_a, user_b) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(lo)
        .bind(hi)
        .execute(&mut *tx)
        .await?;
    domain::ensure_dm(&mut tx, pair.0, pair.1).await?;
    tx.commit().await?;

    notify(st, &[pair.0, pair.1], "friends");
    notify(st, &[pair.0, pair.1], "conversations");
    Ok(pair)
}

async fn accept_request(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(req_id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st)?;
    let to_user = sqlx::query_scalar::<_, Uuid>(
        "SELECT to_user FROM friend_requests WHERE id = $1 AND status = 'pending'",
    )
    .bind(req_id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("request not found or resolved"))?;
    if to_user != uid {
        return Err(AppError::forbidden("only the recipient can accept"));
    }
    let (a, b) = accept_internal(&st, req_id).await?;
    Ok(Json(
        json!({ "status": "accepted", "user_a": a, "user_b": b }),
    ))
}

async fn decline_request(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(req_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    let res = sqlx::query(
        "UPDATE friend_requests SET status = 'declined', resolved_at = now()
         WHERE id = $1 AND to_user = $2 AND status = 'pending'",
    )
    .bind(req_id)
    .bind(uid)
    .execute(&st.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("request not found or resolved"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- conversations ----------

#[derive(sqlx::FromRow)]
struct ConvRow {
    id: Uuid,
    kind: String,
    name: Option<String>,
    owner_id: Option<Uuid>,
    last_seq: i64,
    created_at: DateTime<Utc>,
    unread_count: i64,
    has_mention: bool,
    member_count: i64,
    peer_id: Option<Uuid>,
    peer_username: Option<String>,
    peer_display: Option<String>,
    peer_last_seen: Option<DateTime<Utc>>,
    m_id: Option<Uuid>,
    m_seq: Option<i64>,
    m_sender_id: Option<Uuid>,
    m_sender_username: Option<String>,
    m_sender_name: Option<String>,
    m_content: Option<String>,
    m_kind: Option<String>,
    m_created_at: Option<DateTime<Utc>>,
}

async fn list_conversations(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<ConversationView>>> {
    let uid = uid_of(&headers, &st)?;
    let rows = sqlx::query_as::<_, ConvRow>(
        "SELECT c.id, c.type AS kind, c.name, c.owner_id, c.last_seq, c.created_at,
                (c.last_seq - m.last_read_seq) AS unread_count,
                EXISTS(SELECT 1 FROM message_mentions mm JOIN messages mmsg ON mmsg.id = mm.message_id
                        WHERE mm.user_id = $1 AND mmsg.conversation_id = c.id
                          AND mmsg.seq > m.last_read_seq) AS has_mention,
                (SELECT count(*) FROM memberships mc WHERE mc.conversation_id = c.id) AS member_count,
                p.id AS peer_id, p.username::text AS peer_username, p.display_name AS peer_display,
                p.last_seen_at AS peer_last_seen,
                lm.id AS m_id, lm.seq AS m_seq, lm.sender_id AS m_sender_id,
                su.username::text AS m_sender_username, su.display_name AS m_sender_name,
                lm.content AS m_content, lm.kind AS m_kind, lm.created_at AS m_created_at
           FROM memberships m
           JOIN conversations c ON c.id = m.conversation_id
           LEFT JOIN LATERAL (
               SELECT u.id, u.username, u.display_name, u.last_seen_at
                 FROM memberships mm JOIN users u ON u.id = mm.user_id
                WHERE mm.conversation_id = c.id AND mm.user_id <> $1
                LIMIT 1
           ) p ON c.type = 'dm'
           LEFT JOIN LATERAL (
               SELECT id, seq, sender_id, content, kind, created_at FROM messages
                WHERE conversation_id = c.id ORDER BY seq DESC LIMIT 1
           ) lm ON true
           LEFT JOIN users su ON su.id = lm.sender_id
          WHERE m.user_id = $1
          ORDER BY COALESCE(lm.created_at, c.created_at) DESC",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| {
            let peer = r.peer_id.map(|id| UserPublic {
                id,
                username: r.peer_username.unwrap_or_default(),
                display_name: r.peer_display.unwrap_or_default(),
                last_seen_at: r.peer_last_seen,
            });
            let last_message = r.m_id.map(|id| MessageView {
                id,
                conversation_id: r.id,
                seq: r.m_seq.unwrap_or(0),
                sender_id: r.m_sender_id.unwrap_or_default(),
                sender_username: r.m_sender_username.unwrap_or_default(),
                sender_name: r.m_sender_name.unwrap_or_default(),
                content: r.m_content.unwrap_or_default(),
                kind: r.m_kind.unwrap_or_else(|| "text".into()),
                attachment: None,
                mentions: Vec::new(),
                created_at: r.m_created_at.unwrap_or_else(Utc::now),
            });
            ConversationView {
                id: r.id,
                kind: r.kind,
                name: r.name,
                owner_id: r.owner_id,
                peer,
                member_count: r.member_count,
                last_seq: r.last_seq,
                unread_count: r.unread_count,
                has_mention: r.has_mention,
                last_message,
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(Json(out))
}

async fn create_group(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<GroupCreateReq>,
) -> AppResult<Json<ConversationDetail>> {
    let uid = uid_of(&headers, &st)?;
    let ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = req.member_ids.into_iter().filter(|i| *i != uid).collect();
        v.sort();
        v.dedup();
        v
    };
    if ids.is_empty() {
        return Err(AppError::bad_request("invite at least one friend"));
    }
    let friend_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM friendships
         WHERE (user_a = $1 AND user_b = ANY($2)) OR (user_b = $1 AND user_a = ANY($2))",
    )
    .bind(uid)
    .bind(&ids)
    .fetch_one(&st.pool)
    .await?;
    if friend_count != ids.len() as i64 {
        return Err(AppError::forbidden("can only invite your friends"));
    }

    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("群聊")
        .to_string();
    if name.len() > 100 {
        return Err(AppError::bad_request("name too long"));
    }

    let mut tx = st.pool.begin().await?;
    let conv_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO conversations (type, name, owner_id) VALUES ('group', $1, $2) RETURNING id",
    )
    .bind(&name)
    .bind(uid)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(conv_id)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role)
         SELECT $1, unnest($2::uuid[]), 'member'",
    )
    .bind(conv_id)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    notify(&st, &ids, "conversations");
    conversation_detail(&st, conv_id).await.map(Json)
}

async fn require_member(st: &SharedState, conv: Uuid, uid: Uuid) -> AppResult<domain::MemberInfo> {
    domain::membership(&st.pool, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))
}

async fn conversation_detail(st: &SharedState, conv: Uuid) -> AppResult<ConversationDetail> {
    let (kind, name, owner_id, last_seq, created_at) =
        sqlx::query_as::<_, (String, Option<String>, Option<Uuid>, i64, DateTime<Utc>)>(
            "SELECT type, name, owner_id, last_seq, created_at FROM conversations WHERE id = $1",
        )
        .bind(conv)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("conversation not found"))?;

    let members = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<DateTime<Utc>>,
            String,
            DateTime<Utc>,
        ),
    >(
        "SELECT u.id, u.username::text, u.display_name, u.last_seen_at, m.role, m.joined_at
           FROM memberships m JOIN users u ON u.id = m.user_id
          WHERE m.conversation_id = $1 ORDER BY m.joined_at",
    )
    .bind(conv)
    .fetch_all(&st.pool)
    .await?;

    Ok(ConversationDetail {
        id: conv,
        kind,
        name,
        owner_id,
        last_seq,
        created_at,
        members: members
            .into_iter()
            .map(
                |(id, username, display_name, last_seen_at, role, joined_at)| MemberView {
                    user: UserPublic {
                        id,
                        username,
                        display_name,
                        last_seen_at,
                    },
                    role,
                    joined_at,
                },
            )
            .collect(),
    })
}

async fn get_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<Json<ConversationDetail>> {
    let uid = uid_of(&headers, &st)?;
    require_member(&st, conv, uid).await?;
    conversation_detail(&st, conv).await.map(Json)
}

#[derive(Deserialize)]
struct MsgQuery {
    after_seq: Option<i64>,
    before_seq: Option<i64>,
    limit: Option<i64>,
}

async fn list_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<MsgQuery>,
) -> AppResult<Json<MessagesPage>> {
    let uid = uid_of(&headers, &st)?;
    require_member(&st, conv, uid).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let messages: Vec<MessageView> = if let Some(after) = q.after_seq {
        sqlx::query_as::<_, domain::MsgViewRow>(
            &format!(
                "SELECT {} {} WHERE m.conversation_id = $1 AND m.seq > $2
                   ORDER BY m.seq ASC LIMIT $3",
                domain::MsgViewRow::SELECT,
                domain::MsgViewRow::JOIN
            ),
        )
        .bind(conv)
        .bind(after)
        .bind(limit)
        .fetch_all(&st.pool)
        .await?
        .into_iter()
        .map(domain::MsgViewRow::into_view)
        .collect()
    } else {
        let mut rows = if let Some(before) = q.before_seq {
            sqlx::query_as::<_, domain::MsgViewRow>(
                &format!(
                    "SELECT {} {} WHERE m.conversation_id = $1 AND m.seq < $2
                       ORDER BY m.seq DESC LIMIT $3",
                    domain::MsgViewRow::SELECT,
                    domain::MsgViewRow::JOIN
                ),
            )
            .bind(conv)
            .bind(before)
            .bind(limit)
            .fetch_all(&st.pool)
            .await?
        } else {
            sqlx::query_as::<_, domain::MsgViewRow>(
                &format!(
                    "SELECT {} {} WHERE m.conversation_id = $1
                       ORDER BY m.seq DESC LIMIT $2",
                    domain::MsgViewRow::SELECT,
                    domain::MsgViewRow::JOIN
                ),
            )
            .bind(conv)
            .bind(limit)
            .fetch_all(&st.pool)
            .await?
        };
        rows.reverse();
        rows.into_iter()
            .map(domain::MsgViewRow::into_view)
            .collect()
    };

    let has_more = if q.after_seq.is_some() {
        messages.len() as i64 == limit
    } else {
        messages.len() as i64 == limit && messages.first().map(|m| m.seq > 1).unwrap_or(false)
    };
    Ok(Json(MessagesPage { messages, has_more }))
}


#[derive(Deserialize)]
struct MsgSearchQuery {
    q: String,
    limit: Option<i64>,
}

/// 会话内消息搜索：content ILIKE 子串匹配（转义通配符），按 seq 倒序
async fn search_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<MsgSearchQuery>,
) -> AppResult<Json<Vec<MessageView>>> {
    let uid = uid_of(&headers, &st)?;
    require_member(&st, conv, uid).await?;
    let needle = q.q.trim();
    if needle.is_empty() {
        return Ok(Json(vec![]));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let esc = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let rows = sqlx::query_as::<_, domain::MsgViewRow>(&format!(
        "SELECT {} {} WHERE m.conversation_id = $1
           AND m.content ILIKE '%' || $2 || '%' ESCAPE '\\'
         ORDER BY m.seq DESC LIMIT $3",
        domain::MsgViewRow::SELECT,
        domain::MsgViewRow::JOIN
    ))
    .bind(conv)
    .bind(esc)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter().map(domain::MsgViewRow::into_view).collect(),
    ))
}

async fn add_members(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<AddMembersReq>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    require_member(&st, conv, uid).await?;
    if domain::conv_kind(&st.pool, conv).await?.as_deref() != Some("group") {
        return Err(AppError::bad_request(
            "only group conversations accept members",
        ));
    }
    let ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = req.user_ids.into_iter().filter(|i| *i != uid).collect();
        v.sort();
        v.dedup();
        v
    };
    if ids.is_empty() {
        return Err(AppError::bad_request("no users to add"));
    }
    let friend_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM friendships
         WHERE (user_a = $1 AND user_b = ANY($2)) OR (user_b = $1 AND user_a = ANY($2))",
    )
    .bind(uid)
    .bind(&ids)
    .fetch_one(&st.pool)
    .await?;
    if friend_count != ids.len() as i64 {
        return Err(AppError::forbidden("can only invite your friends"));
    }
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role)
         SELECT $1, unnest($2::uuid[]), 'member' ON CONFLICT DO NOTHING",
    )
    .bind(conv)
    .bind(&ids)
    .execute(&st.pool)
    .await?;
    let all = domain::member_ids(&st.pool, conv).await?;
    notify(&st, &all, "conversations");
    Ok(StatusCode::NO_CONTENT)
}

async fn leave_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    let info = require_member(&st, conv, uid).await?;
    if domain::conv_kind(&st.pool, conv).await?.as_deref() != Some("group") {
        return Err(AppError::bad_request("cannot leave a direct conversation"));
    }
    let others = domain::member_ids(&st.pool, conv)
        .await?
        .into_iter()
        .filter(|u| *u != uid)
        .collect::<Vec<_>>();

    let mut tx = st.pool.begin().await?;
    if info.role == "owner" {
        if others.is_empty() {
            sqlx::query("DELETE FROM conversations WHERE id = $1")
                .bind(conv)
                .execute(&mut *tx)
                .await?;
        } else {
            // 转让给最早加入的成员
            let new_owner = sqlx::query_scalar::<_, Uuid>(
                "UPDATE memberships SET role = 'owner'
                 WHERE conversation_id = $1 AND user_id = (
                     SELECT user_id FROM memberships
                     WHERE conversation_id = $1 AND user_id <> $2
                     ORDER BY joined_at LIMIT 1)
                 RETURNING user_id",
            )
            .bind(conv)
            .bind(uid)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE conversations SET owner_id = $2 WHERE id = $1")
                .bind(conv)
                .bind(new_owner)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM memberships WHERE conversation_id = $1 AND user_id = $2")
                .bind(conv)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        sqlx::query("DELETE FROM memberships WHERE conversation_id = $1 AND user_id = $2")
            .bind(conv)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    notify(&st, &others, "conversations");
    notify(&st, &[uid], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

async fn kick_member(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((conv, target)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    if uid == target {
        return Err(AppError::bad_request("use leave endpoint for yourself"));
    }
    let me = require_member(&st, conv, uid).await?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can remove members"));
    }
    let res = sqlx::query(
        "DELETE FROM memberships WHERE conversation_id = $1 AND user_id = $2 AND role <> 'owner'",
    )
    .bind(conv)
    .bind(target)
    .execute(&st.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("member not found"));
    }
    let rest = domain::member_ids(&st.pool, conv).await?;
    notify(&st, &rest, "conversations");
    notify(&st, &[target], "conversations");
    Ok(StatusCode::NO_CONTENT)
}

async fn rename_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<RenameReq>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    let me = require_member(&st, conv, uid).await?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can rename"));
    }
    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::bad_request("name must be 1-100 chars"));
    }
    sqlx::query("UPDATE conversations SET name = $2 WHERE id = $1")
        .bind(conv)
        .bind(name)
        .execute(&st.pool)
        .await?;
    let members = domain::member_ids(&st.pool, conv).await?;
    notify(&st, &members, "conversations");
    Ok(StatusCode::NO_CONTENT)
}

async fn dissolve_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    let me = require_member(&st, conv, uid).await?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can dissolve"));
    }
    let members = domain::member_ids(&st.pool, conv).await?;
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(conv)
        .execute(&st.pool)
        .await?;
    notify(&st, &members, "conversations");
    Ok(StatusCode::NO_CONTENT)
}


// ---------- 文件上传/下载 ----------

fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// multipart: conversation_id 字段 + file 字段。上传者必须是会话成员。
async fn upload_file(
    State(st): State<SharedState>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> AppResult<Json<AttachmentView>> {
    let uid = uid_of(&headers, &st)?;
    let mut conv: Option<Uuid> = None;
    let mut file_name = "file".to_string();
    let mut mime = "application/octet-stream".to_string();
    let mut buf: Vec<u8> = Vec::new();
    let mut got_file = false;

    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("bad multipart: {e}")))?
    {
        match field.name() {
            Some("conversation_id") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::bad_request("bad conversation_id"))?;
                conv = Some(
                    Uuid::parse_str(v.trim())
                        .map_err(|_| AppError::bad_request("bad conversation_id"))?,
                );
            }
            Some("file") => {
                if let Some(n) = field.file_name() {
                    file_name = n.to_string();
                }
                if let Some(ct) = field.content_type() {
                    mime = ct.to_string();
                }
                got_file = true;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| AppError::bad_request(format!("read error: {e}")))?
                {
                    buf.extend_from_slice(&chunk);
                    if buf.len() > st.cfg.upload_max_bytes {
                        return Err(AppError::bad_request("file too large"));
                    }
                }
            }
            _ => {}
        }
    }
    let conv = conv.ok_or_else(|| AppError::bad_request("missing conversation_id"))?;
    if !got_file || buf.is_empty() {
        return Err(AppError::bad_request("missing file"));
    }
    require_member(&st, conv, uid).await?;

    let id = Uuid::new_v4();
    let path = format!("{}/{}", st.cfg.files_dir, id);
    tokio::fs::write(&path, &buf)
        .await
        .map_err(|e| AppError::internal(format!("store file: {e}")))?;
    let size = buf.len() as i64;
    sqlx::query(
        "INSERT INTO attachments (id, uploader_id, conv_id, file_name, mime, size, storage_path)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(uid)
    .bind(conv)
    .bind(&file_name)
    .bind(&mime)
    .bind(size)
    .bind(&path)
    .execute(&st.pool)
    .await?;
    Ok(Json(AttachmentView {
        id,
        file_name,
        mime,
        size,
    }))
}

/// 按 UUID 直链下载（能力 URL：id 不可猜测，仅会话内可见）。
async fn download_file(
    State(st): State<SharedState>,
    Path(id): Path<Uuid>,
) -> AppResult<impl IntoResponse> {
    let (file_name, mime, path) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT file_name, mime, storage_path FROM attachments WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("file not found"))?;
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| AppError::not_found("file not found"))?;
    let dispo = format!("inline; filename*=UTF-8''{}", pct_encode(&file_name));
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, dispo),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable".into()),
        ],
        data,
    ))
}

// ---------- ws ticket ----------

async fn ws_ticket(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<TicketRes>> {
    let uid = uid_of(&headers, &st)?;
    let ticket = st.issue_ticket(uid);
    Ok(Json(TicketRes {
        ticket,
        expires_in: 30,
    }))
}
#[derive(Deserialize)]
struct TransferReq {
    user_id: Uuid,
}

/// 群主转让：仅群主；目标必须是普通成员
async fn transfer_ownership(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<TransferReq>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st)?;
    if uid == req.user_id {
        return Err(AppError::bad_request("already the owner"));
    }
    let me = require_member(&st, conv, uid).await?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can transfer ownership"));
    }
    let target = require_member(&st, conv, req.user_id).await?;
    if target.role == "owner" {
        return Err(AppError::bad_request("target is already the owner"));
    }

    let mut tx = st.pool.begin().await?;
    sqlx::query(
        "UPDATE memberships SET role = 'member' WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE memberships SET role = 'owner' WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conv)
    .bind(req.user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET owner_id = $2 WHERE id = $1")
        .bind(conv)
        .bind(req.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let members = domain::member_ids(&st.pool, conv).await?;
    notify(&st, &members, "conversations");
    Ok(StatusCode::NO_CONTENT)
}
