use crate::auth;
use crate::domain;
use crate::error::{is_unique_violation, AppError, AppResult};
use crate::model::*;
use crate::settings;
use crate::state::SharedState;
use crate::ws;
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Instant;
use uuid::Uuid;

/// multipart 边界/表单字段/文件头的额外余量；文件本体仍由流式检查卡 upload_max_bytes
const UPLOAD_BODY_MARGIN: usize = 1024 * 1024;

/// 头像上限。路由单独设 body 上限，不复用附件的 25MB。
const AVATAR_MAX_BYTES: usize = 1024 * 1024;

/// 账号管理操作（角色/禁用）的事务级顾问锁：串行化，防止并发降权越过末位保护
const ADMIN_LOCK_KEY: i64 = 0x63_68_61_74_61_64_6D_69; // "chatadmi"

pub fn router(cfg: &crate::config::Config) -> Router<SharedState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/refresh", post(refresh))
        .route("/auth/logout", post(logout))
        .route("/users/me", get(get_me).patch(update_me))
        .route("/users/me/storage", get(my_storage))
        .route(
            "/users/me/avatar",
            put(put_avatar)
                .delete(delete_avatar)
                .layer(DefaultBodyLimit::max(AVATAR_MAX_BYTES + UPLOAD_BODY_MARGIN)),
        )
        .route("/users/{id}/avatar", get(get_avatar))
        .route("/users/me/password", post(crate::sessions::change_password))
        .route("/users/me/sessions", get(crate::sessions::list_sessions))
        .route(
            "/users/me/sessions/revoke-others",
            post(crate::sessions::revoke_other_sessions),
        )
        .route(
            "/users/me/sessions/{id}",
            delete(crate::sessions::revoke_session),
        )
        .route("/users/search", get(search_user))
        .route("/friends", get(list_friends))
        .route("/friends/{user_id}", delete(delete_friend))
        .route("/friends/requests", get(list_requests).post(create_request))
        .route("/friends/requests/{id}/accept", post(accept_request))
        .route("/friends/requests/{id}/decline", post(decline_request))
        .route("/conversations", get(list_conversations))
        .route("/conversations/preferences", get(list_preferences))
        .route("/conversations/group", post(create_group))
        .route(
            "/conversations/{id}",
            get(get_conversation)
                .patch(rename_conversation)
                .delete(dissolve_conversation),
        )
        .route("/conversations/{id}/messages", get(list_messages))
        .route("/conversations/{id}/messages/search", get(search_messages))
        .route(
            "/conversations/{id}/messages/refresh",
            post(refresh_messages),
        )
        .route("/conversations/{id}/preferences", put(put_preferences))
        .route("/conversations/{id}/announcement", put(put_announcement))
        .route("/conversations/{id}/pins", get(list_pins))
        .route(
            "/conversations/{id}/pins/{message_id}",
            put(pin_message).delete(unpin_message),
        )
        .route(
            "/messages/{id}",
            axum::routing::patch(edit_message).delete(delete_message),
        )
        .route(
            "/messages/{id}/reactions/{emoji}",
            put(reaction_add).delete(reaction_remove),
        )
        .route("/conversations/{id}/polls", post(create_poll))
        .route("/messages/{id}/poll/votes", put(vote_poll))
        .route("/messages/{id}/poll/close", post(close_poll))
        .route("/messages/{id}/forward", post(forward_message))
        .route("/conversations/{id}/members", post(add_members))
        .route("/conversations/{id}/members/me", delete(leave_conversation))
        .route("/conversations/{id}/members/{uid}", delete(kick_member))
        .route("/conversations/{id}/transfer", post(transfer_ownership))
        .route(
            "/files",
            post(upload_file).layer(DefaultBodyLimit::max(
                cfg.upload_max_bytes + UPLOAD_BODY_MARGIN,
            )),
        )
        .route("/files/{id}", get(download_file))
        .route("/ws/ticket", get(ws_ticket))
        .route("/ws", get(ws::ws_handler))
        .route("/server-info", get(settings::server_info))
        // ---------- 后台管理（admin only） ----------
        .route("/admin/overview", get(admin_overview))
        .route(
            "/admin/settings",
            get(settings::get_settings).put(settings::put_settings),
        )
        .route(
            "/admin/invites",
            get(settings::list_invites).post(settings::create_invite),
        )
        .route("/admin/invites/{id}", delete(settings::revoke_invite))
        .route("/admin/users", get(admin_users))
        .route("/admin/users/{id}/disable", post(admin_disable))
        .route("/admin/users/{id}/enable", post(admin_enable))
        .route("/admin/users/{id}/role", post(admin_set_role))
        .route("/admin/users/{id}/password", post(admin_reset_password))
}

async fn uid_of(headers: &HeaderMap, st: &SharedState) -> AppResult<Uuid> {
    Ok(auth::require_user(st, headers).await?.id)
}

fn notify(st: &SharedState, uids: &[Uuid], what: &'static str) {
    let payload = serde_json::to_string(&WsOut::Invalidate { what }).unwrap();
    st.send_to_users(uids, &payload);
}

fn notify_pins(st: &SharedState, change: &domain::PinChange) {
    if !change.changed {
        return;
    }
    let payload = serde_json::to_string(&WsOut::PinsUpdated {
        conversation_id: change.conversation_id,
    })
    .unwrap();
    st.send_to_users(&change.member_ids, &payload);
}

/// 消息 mutation 的领域拒绝 → HTTP 状态（WS 侧走 PostError::into_ws 错误帧）。
fn post_err(e: domain::PostError) -> AppError {
    match e {
        domain::PostError::Deny { code, message } => match code {
            "forbidden" | "not_friends" => AppError::forbidden(message),
            "not_found" => AppError::not_found(message),
            "conflict" => AppError::conflict(message),
            _ => AppError::bad_request(message),
        },
        domain::PostError::Db(e) => e.into(),
    }
}

/// 软删除后异步清理已无引用的附件物理文件（失败只记日志：行已删，下载已失效）。
pub fn spawn_file_cleanup(st: &SharedState, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }
    let files_dir = st.cfg.files_dir.clone();
    tokio::spawn(async move {
        for p in paths {
            let fp = stored_file_path(&files_dir, &p);
            if let Err(e) = tokio::fs::remove_file(&fp).await {
                tracing::warn!(path = %fp.display(), error = %e, "orphan attachment cleanup failed");
            }
        }
    });
}

/// 关系建立后补发 presence：to 立刻看到 from 的真实在线状态
async fn push_presence(st: &SharedState, from: Uuid, to: Uuid) {
    let seen = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT last_seen_at FROM users WHERE id = $1",
    )
    .bind(from)
    .fetch_one(&st.pool)
    .await
    .ok()
    .flatten();
    if let Ok(p) = serde_json::to_string(&WsOut::Presence {
        user_id: from,
        online: st.is_online(from),
        last_seen_at: seen,
    }) {
        st.send_to_user(to, &p);
    }
}
/// IP/UID 滑动窗口限流（注册/登录按 IP、改密按 uid）：20 次 / 5 分钟
pub(crate) fn rate_limit(st: &SharedState, key: String) -> AppResult<()> {
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
    headers: HeaderMap,
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
    let mut tx = st.pool.begin().await?;

    // 注册闸门：判断与插入在同一事务内（不留竞态窗口）。
    // 空库放行（首个用户仍是管理员引导路径）；非空库按 registration_mode 校验。
    let mut invite_id = None;
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users)")
        .fetch_one(&mut *tx)
        .await?
    {
        match settings::load(&mut *tx).await?.registration_mode.as_str() {
            settings::MODE_CLOSED => {
                return Err(AppError::forbidden("管理员已关闭注册"));
            }
            settings::MODE_INVITE => {
                let code = req
                    .invite_code
                    .as_deref()
                    .map(settings::normalize_code)
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| AppError::forbidden("邀请码无效或已失效"))?;
                invite_id = Some(settings::consume_invite(&mut *tx, &code).await?);
            }
            // open（含未知值兜底）：忽略 invite_code，不校验不计数
            _ => {}
        }
    }

    let row = sqlx::query_as::<_, UserRow>(
        "INSERT INTO users (username, display_name, password_hash, role, invite_id)
         SELECT $1, $2, $3,
                CASE WHEN NOT EXISTS(SELECT 1 FROM users) THEN 'admin' ELSE 'user' END,
                $4
         RETURNING *",
    )
    .bind(username)
    .bind(&display_name)
    .bind(&hash)
    .bind(invite_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            AppError::conflict("username already taken")
        } else {
            e.into()
        }
    })?;

    // 文件传输助手：每用户一个 self 会话，与用户同事务创建（失败整体回滚）
    domain::ensure_self_conversation(&mut *tx, row.id).await?;

    // 注册即一条登录会话（一台设备）
    let sid = auth::create_session(
        &mut *tx,
        row.id,
        &crate::sessions::device_label_of(req.device_label.as_deref()),
        &crate::sessions::user_agent_of(&headers),
    )
    .await?;
    let res = auth::issue_tokens(&mut *tx, &st.cfg, &row, sid).await?;
    tx.commit().await?;
    Ok(Json(res))
}

async fn login(
    State(st): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> AppResult<Json<AuthResponse>> {
    rate_limit(&st, format!("login:{}", addr.ip()))?;
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE username = $1 FOR UPDATE")
        .bind(req.username.trim())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::unauthorized("invalid credentials"))?;

    if !auth::verify_password(&row.password_hash, &req.password) {
        return Err(AppError::unauthorized("invalid credentials"));
    }
    if row.disabled_at.is_some() {
        return Err(AppError::forbidden("账号已被禁用"));
    }
    // 登录即一条登录会话（一台设备）
    let sid = auth::create_session(
        &mut *tx,
        row.id,
        &crate::sessions::device_label_of(req.device_label.as_deref()),
        &crate::sessions::user_agent_of(&headers),
    )
    .await?;
    let res = auth::issue_tokens(&mut *tx, &st.cfg, &row, sid).await?;
    tx.commit().await?;
    Ok(Json(res))
}

async fn refresh(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<RefreshReq>,
) -> AppResult<Json<AuthResponse>> {
    let hash = auth::refresh_hash(&req.refresh_token);
    let mut tx = st.pool.begin().await?;
    // 先定位用户，再按 user → refresh 顺序加锁，与改密/禁用串行化。
    let user_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM refresh_tokens
          WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(user_id) = user_id else {
        // 统一 401：不区分「从未签发」与「已撤销/过期」，避免令牌状态探测。
        // 失败路径不再向 pool 借第二个连接（本函数只持有这一个事务连接），
        // 并发非法 refresh 不会把连接池拖死。
        return Err(AppError::unauthorized("invalid refresh token"));
    };

    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    // 等待用户锁期间可能已被撤销；锁内重检并轮换，失败回滚旧 token。
    let old_sid = sqlx::query_scalar::<_, Option<Uuid>>(
        "UPDATE refresh_tokens SET revoked_at = now()
          WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()
          RETURNING session_id",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(old_sid) = old_sid else {
        return Err(AppError::unauthorized("invalid refresh token"));
    };
    if row.disabled_at.is_some() {
        tx.commit().await?;
        return Err(AppError::forbidden("账号已被禁用"));
    }
    let sid = match old_sid {
        Some(sid) => {
            // 沿用同一登录会话；session 已撤销则整次登录作废（顺带烧掉已轮换的旧 token）
            let alive = sqlx::query_scalar::<_, bool>(
                "UPDATE sessions SET last_used_at = now()
                  WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
                  RETURNING true",
            )
            .bind(sid)
            .bind(row.id)
            .fetch_optional(&mut *tx)
            .await?;
            if alive.is_none() {
                tx.commit().await?;
                return Err(AppError::unauthorized(
                    "session expired, please login again",
                ));
            }
            sid
        }
        // 升级前签发的老 token（session_id 为 NULL）：补建一条 session
        None => {
            auth::create_session(
                &mut *tx,
                row.id,
                "旧设备",
                &crate::sessions::user_agent_of(&headers),
            )
            .await?
        }
    };
    let res = auth::issue_tokens(&mut *tx, &st.cfg, &row, sid).await?;
    tx.commit().await?;
    Ok(Json(res))
}

async fn logout(
    State(st): State<SharedState>,
    Json(req): Json<LogoutReq>,
) -> AppResult<StatusCode> {
    let hash = auth::refresh_hash(&req.refresh_token);
    let mut tx = st.pool.begin().await?;
    let found = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT user_id, session_id FROM refresh_tokens WHERE token_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((uid, sid)) = found {
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        match sid {
            // 登出 = 撤销该 refresh token 所属的整个登录会话（连带其全部 refresh token）
            Some(sid) => {
                sqlx::query(
                    "UPDATE sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
                )
                .bind(sid)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE refresh_tokens SET revoked_at = now()
                     WHERE session_id = $1 AND revoked_at IS NULL",
                )
                .bind(sid)
                .execute(&mut *tx)
                .await?;
            }
            // 无 session 的老 token：只撤销自己
            None => {
                sqlx::query(
                    "UPDATE refresh_tokens SET revoked_at = now()
                     WHERE token_hash = $1 AND revoked_at IS NULL",
                )
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- users ----------

async fn get_me(State(st): State<SharedState>, headers: HeaderMap) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st).await?;
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(UserPublic::from(&row)))
}

/// GET /users/me/storage（V2 P2-1）：本人附件用量与配额；quota_bytes 为 null 表示不限
async fn my_storage(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<StorageView>> {
    let uid = uid_of(&headers, &st).await?;
    let quota_mb = settings::load(&st.pool).await?.storage_quota_mb;
    let used = domain::storage_used(&st.pool, uid).await?;
    Ok(Json(StorageView {
        used_bytes: used,
        quota_bytes: (quota_mb > 0).then_some(quota_mb * 1024 * 1024),
    }))
}

async fn update_me(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<UpdateMeReq>,
) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st).await?;
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
    let uid = uid_of(&headers, &st).await?;
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
          WHERE id <> $1 AND disabled_at IS NULL
            AND (username ILIKE '%' || $2 || '%' ESCAPE '\\'
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
    role: String,
    last_seen_at: Option<DateTime<Utc>>,
    since: DateTime<Utc>,
    avatar_version: i32,
    has_avatar: bool,
}

async fn list_friends(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<FriendView>>> {
    let uid = uid_of(&headers, &st).await?;
    let rows = sqlx::query_as::<_, FriendRow>(
        "SELECT u.id, u.username::text AS username, u.display_name, u.role, u.last_seen_at,
                f.created_at AS since, u.avatar_version, u.avatar_path IS NOT NULL AS has_avatar
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
                    role: r.role,
                    last_seen_at: r.last_seen_at,
                    avatar_version: r.avatar_version,
                    has_avatar: r.has_avatar,
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
    let uid = uid_of(&headers, &st).await?;
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
struct MemberRow {
    id: Uuid,
    username: String,
    display_name: String,
    urole: String,
    last_seen_at: Option<DateTime<Utc>>,
    role: String,
    joined_at: DateTime<Utc>,
    avatar_version: i32,
    has_avatar: bool,
}

#[derive(sqlx::FromRow)]
struct ReqRow {
    id: Uuid,
    message: String,
    created_at: DateTime<Utc>,
    cp_id: Uuid,
    cp_username: String,
    cp_display: String,
    cp_role: String,
    cp_last_seen: Option<DateTime<Utc>>,
    cp_avatar_version: i32,
    cp_has_avatar: bool,
}

async fn list_requests(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st).await?;
    let incoming = sqlx::query_as::<_, ReqRow>(
        "SELECT r.id, r.message, r.created_at,
                u.id AS cp_id, u.username::text AS cp_username, u.display_name AS cp_display,
                u.role AS cp_role, u.last_seen_at AS cp_last_seen,
                u.avatar_version AS cp_avatar_version, u.avatar_path IS NOT NULL AS cp_has_avatar
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
                u.role AS cp_role, u.last_seen_at AS cp_last_seen,
                u.avatar_version AS cp_avatar_version, u.avatar_path IS NOT NULL AS cp_has_avatar
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
                    role: r.cp_role,
                    last_seen_at: r.cp_last_seen,
                    avatar_version: r.cp_avatar_version,
                    has_avatar: r.cp_has_avatar,
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
    let uid = uid_of(&headers, &st).await?;
    let to = sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users WHERE username = $1 AND disabled_at IS NULL",
    )
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

    // 同方向已被拒绝过 → 不再接受重复申请（除非对方重新发起，由上面的 reverse 分支接住）
    let declined = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM friend_requests
         WHERE from_user = $1 AND to_user = $2 AND status = 'declined')",
    )
    .bind(uid)
    .bind(to.id)
    .fetch_one(&st.pool)
    .await?;
    if declined {
        return Err(AppError::conflict("对方已拒绝过你的好友申请"));
    }

    // 字符级截断：按字节切中文会落在 UTF-8 边界中间直接 panic
    let msg: String = req.message.trim().chars().take(200).collect();
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO friend_requests (from_user, to_user, message)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(uid)
    .bind(to.id)
    .bind(&msg)
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
    let uid = uid_of(&headers, &st).await?;
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
    // 新好友互推当前在线状态（否则要等下一次重连）
    push_presence(&st, a, b).await;
    push_presence(&st, b, a).await;
    Ok(Json(
        json!({ "status": "accepted", "user_a": a, "user_b": b }),
    ))
}

async fn decline_request(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(req_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    let from_user = sqlx::query_scalar::<_, Uuid>(
        "UPDATE friend_requests SET status = 'declined', resolved_at = now()
         WHERE id = $1 AND to_user = $2 AND status = 'pending'
         RETURNING from_user",
    )
    .bind(req_id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("request not found or resolved"))?;
    notify(&st, &[uid, from_user], "requests");
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
    peer_role: Option<String>,
    peer_last_seen: Option<DateTime<Utc>>,
    peer_avatar_version: Option<i32>,
    peer_has_avatar: Option<bool>,
    m_id: Option<Uuid>,
    m_seq: Option<i64>,
    m_sender_id: Option<Uuid>,
    m_sender_username: Option<String>,
    m_sender_name: Option<String>,
    m_content: Option<String>,
    m_kind: Option<String>,
    m_created_at: Option<DateTime<Utc>>,
    m_att_id: Option<Uuid>,
    m_att_name: Option<String>,
    m_att_mime: Option<String>,
    m_att_size: Option<i64>,
    m_mentions: Option<Vec<Uuid>>,
    m_reply_to_id: Option<Uuid>,
    m_reply_to: Option<sqlx::types::Json<ReplyPreview>>,
    m_edited_at: Option<DateTime<Utc>>,
    m_edit_version: Option<i32>,
    m_deleted_at: Option<DateTime<Utc>>,
    m_reactions: Option<sqlx::types::Json<Vec<domain::ReactionAgg>>>,
    m_poll: Option<sqlx::types::Json<domain::PollAgg>>,
    m_att_width: Option<i32>,
    m_att_height: Option<i32>,
    m_att_has_thumb: Option<bool>,
    m_att_duration_ms: Option<i32>,
    m_sender_avatar_version: Option<i32>,
    m_sender_has_avatar: Option<bool>,
    m_forwarded_from: Option<String>,
    // 本人偏好（conversation_preferences 左连接，无行则全 NULL/false）
    p_pinned_at: Option<DateTime<Utc>>,
    p_archived_at: Option<DateTime<Utc>>,
    p_muted_until: Option<DateTime<Utc>>,
    p_mute_forever: Option<bool>,
    p_updated_at: Option<DateTime<Utc>>,
    announcement: Option<String>,
    announcement_by_name: Option<String>,
    announcement_at: Option<DateTime<Utc>>,
}

async fn list_conversations(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<ConversationView>>> {
    let uid = uid_of(&headers, &st).await?;
    let rows = sqlx::query_as::<_, ConvRow>(
        "SELECT c.id, c.type AS kind, c.name, c.owner_id, c.last_seq, c.created_at,
                (c.last_seq - m.last_read_seq) AS unread_count,
                EXISTS(SELECT 1 FROM message_mentions mm JOIN messages mmsg ON mmsg.id = mm.message_id
                        WHERE mm.user_id = $1 AND mmsg.conversation_id = c.id
                          AND mmsg.seq > m.last_read_seq) AS has_mention,
                (SELECT count(*) FROM memberships mc WHERE mc.conversation_id = c.id) AS member_count,
                p.id AS peer_id, p.username::text AS peer_username, p.display_name AS peer_display,
                p.role AS peer_role, p.last_seen_at AS peer_last_seen,
                p.avatar_version AS peer_avatar_version, p.has_avatar AS peer_has_avatar,
                lm.id AS m_id, lm.seq AS m_seq, lm.sender_id AS m_sender_id,
                lm.sender_username AS m_sender_username, lm.sender_name AS m_sender_name,
                lm.content AS m_content, lm.kind AS m_kind, lm.created_at AS m_created_at,
                lm.att_id AS m_att_id, lm.att_name AS m_att_name, lm.att_mime AS m_att_mime,
                lm.att_size AS m_att_size, lm.mentions AS m_mentions,
                lm.reply_to_id AS m_reply_to_id, lm.reply_to AS m_reply_to,
                lm.edited_at AS m_edited_at, lm.edit_version AS m_edit_version,
                lm.deleted_at AS m_deleted_at, lm.reactions AS m_reactions,
                lm.poll AS m_poll,
                lm.att_width AS m_att_width, lm.att_height AS m_att_height,
                lm.att_has_thumb AS m_att_has_thumb, lm.att_duration_ms AS m_att_duration_ms,
                lm.sender_avatar_version AS m_sender_avatar_version,
                lm.sender_has_avatar AS m_sender_has_avatar, lm.forwarded_from AS m_forwarded_from,
                cp.pinned_at AS p_pinned_at, cp.archived_at AS p_archived_at,
                cp.muted_until AS p_muted_until, cp.mute_forever AS p_mute_forever,
                cp.updated_at AS p_updated_at,
                c.announcement, au.display_name AS announcement_by_name, c.announcement_at
           FROM memberships m
           JOIN conversations c ON c.id = m.conversation_id
           LEFT JOIN users au ON au.id = c.announcement_by
           LEFT JOIN conversation_preferences cp
                  ON cp.conversation_id = c.id AND cp.user_id = m.user_id
           LEFT JOIN LATERAL (
               SELECT u.id, u.username, u.display_name, u.role,
                      u.avatar_version, u.avatar_path IS NOT NULL AS has_avatar,
                      -- last_seen_at 属于 presence：删好友后保留的只读 DM 不得继续暴露
                      CASE WHEN EXISTS(SELECT 1 FROM friendships f
                                        WHERE (f.user_a = $1 AND f.user_b = u.id)
                                           OR (f.user_b = $1 AND f.user_a = u.id))
                            OR EXISTS(SELECT 1 FROM memberships mx1
                                       JOIN memberships mx2
                                         ON mx1.conversation_id = mx2.conversation_id
                                       JOIN conversations cx
                                         ON cx.id = mx1.conversation_id AND cx.type = 'group'
                                       WHERE mx1.user_id = $1 AND mx2.user_id = u.id)
                           THEN u.last_seen_at END AS last_seen_at
                 FROM memberships mm JOIN users u ON u.id = mm.user_id
                WHERE mm.conversation_id = c.id AND mm.user_id <> $1
                LIMIT 1
           ) p ON c.type = 'dm'
           LEFT JOIN LATERAL (
               SELECT id, seq, sender_id, sender_username, sender_name, content, kind, created_at,
                      att_id, att_name, att_mime, att_size, mentions,
                      reply_to_id, reply_to, edited_at, edit_version, deleted_at, reactions, poll,
                      att_width, att_height, att_has_thumb, att_duration_ms,
                      sender_avatar_version, sender_has_avatar, forwarded_from
                 FROM message_views
                WHERE conversation_id = c.id
                ORDER BY seq DESC
                LIMIT 1
           ) lm ON true
          WHERE m.user_id = $1
          -- 置顶块在前按 pinned_at DESC，块内与未置顶均按原活动时间 DESC、id 兜底
          ORDER BY (cp.pinned_at IS NOT NULL) DESC, cp.pinned_at DESC,
                   COALESCE(lm.created_at, c.created_at) DESC, c.id",
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
                role: r.peer_role.unwrap_or_else(|| "user".into()),
                last_seen_at: r.peer_last_seen,
                avatar_version: r.peer_avatar_version.unwrap_or(0),
                has_avatar: r.peer_has_avatar.unwrap_or(false),
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
                attachment: r.m_att_id.map(|att_id| AttachmentView {
                    id: att_id,
                    file_name: r.m_att_name.clone().unwrap_or_default(),
                    mime: r.m_att_mime.clone().unwrap_or_default(),
                    size: r.m_att_size.unwrap_or_default(),
                    width: r.m_att_width,
                    height: r.m_att_height,
                    has_thumb: r.m_att_has_thumb.unwrap_or(false),
                    duration_ms: r.m_att_duration_ms,
                }),
                mentions: r.m_mentions.clone().unwrap_or_default(),
                created_at: r.m_created_at.unwrap_or_else(Utc::now),
                reply_to_id: r.m_reply_to_id,
                reply_to_preview: r.m_reply_to.clone().map(|j| j.0),
                edited_at: r.m_edited_at,
                edit_version: r.m_edit_version.unwrap_or(0),
                deleted_at: r.m_deleted_at,
                reactions: r
                    .m_reactions
                    .as_ref()
                    .map(|j| {
                        j.0.iter()
                            .map(|a| ReactionView {
                                emoji: a.emoji.clone(),
                                count: a.users.len() as i64,
                                reacted_by_me: a.users.contains(&uid),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                poll: r.m_poll.map(|j| j.0.into_view(uid)),
                sender_avatar_version: r.m_sender_avatar_version.unwrap_or(0),
                sender_has_avatar: r.m_sender_has_avatar.unwrap_or(false),
                forwarded_from: r.m_forwarded_from.clone(),
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
                preference: ConversationPref {
                    pinned_at: r.p_pinned_at,
                    archived_at: r.p_archived_at,
                    muted_until: r.p_muted_until,
                    mute_forever: r.p_mute_forever.unwrap_or(false),
                    updated_at: r.p_updated_at,
                },
                announcement: r.announcement.map(|text| AnnouncementView {
                    text,
                    updated_by_name: r.announcement_by_name.clone(),
                    updated_at: r.announcement_at,
                }),
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(Json(out))
}

// ---------- 会话偏好（仅本人可见、同步本人全部设备） ----------

/// 全量偏好列表：仅供需要时单独拉取；会话列表本身已带 preference 字段，无需 N+1。
async fn list_preferences(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<PrefEntry>>> {
    let uid = uid_of(&headers, &st).await?;
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            bool,
            DateTime<Utc>,
        ),
    >(
        "SELECT conversation_id, pinned_at, archived_at, muted_until, mute_forever, updated_at
           FROM conversation_preferences WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(cid, pinned_at, archived_at, muted_until, mute_forever, t)| PrefEntry {
                    conversation_id: cid,
                    preference: ConversationPref {
                        pinned_at,
                        archived_at,
                        muted_until,
                        mute_forever,
                        updated_at: Some(t),
                    },
                },
            )
            .collect(),
    ))
}

/// 更新本人偏好：缺省字段不改；mute 形状/时间在领域层校验；解散/非成员 403。
/// client_op_id 仅作契约参数（状态覆写天然幂等，无需持久化 op_id）。
/// 成功后给本人全部连接发 preference_updated（含发起连接，幂等收敛）。
async fn put_preferences(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(_q): Query<ClientOpQuery>,
    Json(req): Json<PrefUpdateReq>,
) -> AppResult<Json<ConversationPref>> {
    let uid = uid_of(&headers, &st).await?;
    let pref = domain::set_preferences(&st.pool, conv, uid, &req)
        .await
        .map_err(post_err)?;
    let frame = serde_json::to_string(&WsOut::PreferenceUpdated {
        conversation_id: conv,
        preference: pref.clone(),
    })
    .unwrap();
    st.send_to_user(uid, &frame);
    Ok(Json(pref))
}

async fn create_group(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<GroupCreateReq>,
) -> AppResult<Json<ConversationDetail>> {
    let uid = uid_of(&headers, &st).await?;
    let ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = req.member_ids.into_iter().filter(|i| *i != uid).collect();
        v.sort();
        v.dedup();
        v
    };
    if ids.is_empty() {
        return Err(AppError::bad_request("invite at least one friend"));
    }
    domain::ensure_friends(&st.pool, uid, &ids).await?;

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

    let members = sqlx::query_as::<_, MemberRow>(
        "SELECT u.id, u.username::text AS username, u.display_name, u.role AS urole, u.last_seen_at,
                m.role, m.joined_at, u.avatar_version, u.avatar_path IS NOT NULL AS has_avatar
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
            .map(|r| MemberView {
                user: UserPublic {
                    id: r.id,
                    username: r.username,
                    display_name: r.display_name,
                    role: r.urole,
                    last_seen_at: r.last_seen_at,
                    avatar_version: r.avatar_version,
                    has_avatar: r.has_avatar,
                },
                role: r.role,
                joined_at: r.joined_at,
            })
            .collect(),
    })
}

async fn get_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<Json<ConversationDetail>> {
    let uid = uid_of(&headers, &st).await?;
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
    let uid = uid_of(&headers, &st).await?;
    require_member(&st, conv, uid).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let messages = if let Some(after) = q.after_seq {
        domain::messages_after(&st.pool, conv, uid, after, limit).await?
    } else if let Some(before) = q.before_seq {
        domain::messages_before(&st.pool, conv, uid, before, limit).await?
    } else {
        domain::messages_latest(&st.pool, conv, uid, limit).await?
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
    let uid = uid_of(&headers, &st).await?;
    require_member(&st, conv, uid).await?;
    let needle = q.q.trim();
    if needle.is_empty() {
        return Ok(Json(vec![]));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    Ok(Json(
        domain::search_messages(&st.pool, conv, uid, needle, limit).await?,
    ))
}

// ---------- 消息生命周期 REST ----------

/// 重连批量刷新：按 id 回读最新状态（正文/墓碑/编辑/reaction/回复预览）。
/// 未授权会话直接 403，客户端据此丢弃本地缓存。每批上限 100。
async fn refresh_messages(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<RefreshMessagesReq>,
) -> AppResult<Json<MessagesPage>> {
    let uid = uid_of(&headers, &st).await?;
    if req.ids.len() > 100 {
        return Err(AppError::bad_request("too many ids (max 100)"));
    }
    require_member(&st, conv, uid).await?;
    let messages = domain::messages_by_ids(&st.pool, conv, uid, &req.ids).await?;
    Ok(Json(MessagesPage {
        messages,
        has_more: false,
    }))
}

/// 编辑消息：仅发送者、文本、15 分钟窗口、expected_version 乐观并发。
/// 成功后按成员个性化广播 message_updated；REST 返回发起者视角视图。
async fn edit_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<EditMessageReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::edit_message(
        &st.pool,
        message_id,
        uid,
        &req.content,
        req.expected_version,
        req.client_op_id,
    )
    .await
    .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

/// 软删除：本人/群主权限在领域层锁内判定；重复删除幂等。
/// client_op_id 仅作幂等契约参数（删除本身已状态幂等）。
/// 返回发起者视角的完整墓碑视图（与 message_deleted 广播同形），发起端立即合并。
async fn delete_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let d = domain::delete_message(&st.pool, message_id, uid)
        .await
        .map_err(post_err)?;
    ws::broadcast_message_deleted(&st, &d);
    if d.pins_dropped {
        notify_pins(
            &st,
            &domain::PinChange {
                conversation_id: d.conversation_id,
                member_ids: d.member_ids.clone(),
                changed: true,
            },
        );
    }
    spawn_file_cleanup(&st, d.orphan_files);
    Ok(Json(d.row.into_view(uid)))
}

async fn reaction_write(
    st: &SharedState,
    headers: &HeaderMap,
    message_id: Uuid,
    emoji: &str,
    add: bool,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(headers, st).await?;
    let ch = domain::set_reaction(&st.pool, message_id, uid, emoji, add)
        .await
        .map_err(post_err)?;
    ws::broadcast_reaction_update(st, &ch);
    // REST 响应为发起者视角的完整视图（客户端按 id+version 整视图合并）
    Ok(Json(ch.row.into_view(uid)))
}

async fn reaction_add(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((message_id, emoji)): Path<(Uuid, String)>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    reaction_write(&st, &headers, message_id, &emoji, true).await
}

async fn reaction_remove(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((message_id, emoji)): Path<(Uuid, String)>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    reaction_write(&st, &headers, message_id, &emoji, false).await
}

// ---------- 轻量投票 REST ----------
// 投票 mutation 只走 REST；同步复用现有 WS 广播（创建→message 帧，
// 投票/关闭→message_updated 完整个性化视图），不新增 WS mutation/事件类型。

/// 创建投票（仅群聊成员）：复用 post_message 单事务（seq + client_msg_id 幂等），
/// 成功即按普通消息广播 message 帧（视图已含 poll 卡片数据），响应为发起者视角视图。
async fn create_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<CreatePollReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let posted = domain::create_poll(&st.pool, conv, uid, &req)
        .await
        .map_err(post_err)?;
    ws::broadcast_posted(&st, conv, &posted).await;
    Ok(Json(posted.message))
}

/// 投票：PUT 替换本人选择集合（空数组撤票）；广播 message_updated 逐成员个性化视图。
async fn vote_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<VotePollReq>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::vote_poll(&st.pool, message_id, uid, &req.option_ids)
        .await
        .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

/// 关闭投票：创建者或群主；重复关票幂等。client_op_id 仅作契约参数。
async fn close_poll(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Query(_q): Query<ClientOpQuery>,
) -> AppResult<Json<MessageView>> {
    let uid = uid_of(&headers, &st).await?;
    let ed = domain::close_poll(&st.pool, message_id, uid)
        .await
        .map_err(post_err)?;
    ws::broadcast_message_updated(&st, &ed).await;
    Ok(Json(ed.row.into_view(uid)))
}

// ---------- 转发 / 收藏（V2 P2-4） ----------

/// POST /messages/{id}/forward：复制一条消息到至多 10 个目标会话。
/// 收藏 = 客户端把目标填本人的文件传输助手。
/// 源侧统一拒绝（非成员 403 / 墓碑或投票 400 / 目标列表形状 400）；目标侧逐条
/// 独立成败：HTTP 恒 200，results 与去重后的请求目标顺序一一对应。
/// 每条成功目标走与 WS 发送相同的 message 帧广播（含 unarchive 偏好推送）。
async fn forward_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(message_id): Path<Uuid>,
    Json(req): Json<ForwardReq>,
) -> AppResult<Json<ForwardResp>> {
    let uid = uid_of(&headers, &st).await?;
    // 整个请求按 uid 限流（与注册/登录同一滑动窗口参数）
    rate_limit(&st, format!("fwd:{uid}"))?;
    if req.conversation_ids.is_empty() || req.conversation_ids.len() > 10 {
        return Err(AppError::bad_request(
            "conversation_ids must be 1-10 targets",
        ));
    }
    // 去重保持首见顺序（results 按此顺序逐条对应）
    let mut seen = std::collections::HashSet::new();
    let targets: Vec<Uuid> = req
        .conversation_ids
        .iter()
        .copied()
        .filter(|c| seen.insert(*c))
        .collect();

    let results = domain::forward_message(&st.pool, uid, message_id, &targets, req.client_op_id)
        .await
        .map_err(post_err)?;
    let mut items = Vec::with_capacity(results.len());
    for r in results {
        match r {
            domain::Forwarded::Ok { conv, posted } => {
                ws::broadcast_posted(&st, conv, &posted).await;
                items.push(ForwardResultItem {
                    conversation_id: conv,
                    ok: true,
                    message: Some(posted.message),
                    error: None,
                    message_text: None,
                });
            }
            domain::Forwarded::Fail {
                conv,
                code,
                message,
            } => items.push(ForwardResultItem {
                conversation_id: conv,
                ok: false,
                message: None,
                error: Some(code.to_string()),
                message_text: Some(message.to_string()),
            }),
        }
    }
    Ok(Json(ForwardResp { results: items }))
}

async fn add_members(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<AddMembersReq>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    let ids: Vec<Uuid> = {
        let mut v: Vec<Uuid> = req.user_ids.into_iter().filter(|i| *i != uid).collect();
        v.sort();
        v.dedup();
        v
    };
    if ids.is_empty() {
        return Err(AppError::bad_request("no users to add"));
    }
    domain::ensure_friends(&st.pool, uid, &ids).await?;

    // 与解散/退群/踢人共用会话行锁：锁内重检类型、解散标记与我的成员身份，
    // 防止「邀请」恰好落在解散与成员清空之间把人插回已解散会话。
    let mut tx = st.pool.begin().await?;
    let Some((kind, _, dissolved)) = domain::lock_conversation(&mut tx, conv).await? else {
        return Err(AppError::forbidden("not a member of this conversation"));
    };
    if kind != "group" {
        return Err(AppError::bad_request(
            "only group conversations accept members",
        ));
    }
    if dissolved {
        return Err(AppError::bad_request("group already dissolved"));
    }
    domain::membership(&mut *tx, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role)
         SELECT $1, unnest($2::uuid[]), 'member' ON CONFLICT DO NOTHING",
    )
    .bind(conv)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    let all = domain::member_ids(&mut *tx, conv).await?;
    tx.commit().await?;

    notify(&st, &all, "conversations");
    // 新成员与既有成员互推在线状态
    for nid in &ids {
        for m in &all {
            if m != nid {
                push_presence(&st, *nid, *m).await;
                push_presence(&st, *m, *nid).await;
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn leave_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;

    // 全程在会话行锁内做检查与改写：转让/退群/踢人/解散互斥，杜绝零群主。
    let mut tx = st.pool.begin().await?;
    let Some((kind, _, _)) = domain::lock_conversation(&mut tx, conv).await? else {
        return Err(AppError::forbidden("not a member of this conversation"));
    };
    if kind != "group" {
        return Err(AppError::bad_request("cannot leave a direct conversation"));
    }
    let me = domain::membership(&mut *tx, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    let others = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM memberships
         WHERE conversation_id = $1 AND user_id <> $2
         ORDER BY joined_at",
    )
    .bind(conv)
    .bind(uid)
    .fetch_all(&mut *tx)
    .await?;

    if me.role == "owner" {
        if others.is_empty() {
            sqlx::query("DELETE FROM conversations WHERE id = $1")
                .bind(conv)
                .execute(&mut *tx)
                .await?;
        } else {
            // 转让给最早加入的成员；锁内执行，rows_affected 异常即整体回滚
            let new_owner = sqlx::query_scalar::<_, Uuid>(
                "UPDATE memberships SET role = 'owner'
                 WHERE conversation_id = $1 AND user_id = $2
                 RETURNING user_id",
            )
            .bind(conv)
            .bind(others[0])
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::internal("ownership transfer failed"))?;
            sqlx::query("UPDATE conversations SET owner_id = $2 WHERE id = $1")
                .bind(conv)
                .bind(new_owner)
                .execute(&mut *tx)
                .await?;
        }
    }
    if me.role == "owner" && others.is_empty() {
        // 最后一人退出：会话已连同成员一起删除
    } else {
        let res =
            sqlx::query("DELETE FROM memberships WHERE conversation_id = $1 AND user_id = $2")
                .bind(conv)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        if res.rows_affected() != 1 {
            return Err(AppError::internal("leave failed"));
        }
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
    let uid = uid_of(&headers, &st).await?;
    if uid == target {
        return Err(AppError::bad_request("use leave endpoint for yourself"));
    }
    // 锁内重检：我仍是群主、目标仍是非群主成员，才能删
    let mut tx = st.pool.begin().await?;
    let Some((kind, _, _)) = domain::lock_conversation(&mut tx, conv).await? else {
        return Err(AppError::forbidden("not a member of this conversation"));
    };
    // self 会话单人不可变：把成员操作与 dm 一样拒绝，但统一 400（群主身份本是唯一成员，
    // 不做类型守卫会落到 member not found 的 404）
    if kind == "self" {
        return Err(AppError::bad_request(
            "self conversation has no members to manage",
        ));
    }
    let me = domain::membership(&mut *tx, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can remove members"));
    }
    let res = sqlx::query(
        "DELETE FROM memberships WHERE conversation_id = $1 AND user_id = $2 AND role <> 'owner'",
    )
    .bind(conv)
    .bind(target)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("member not found"));
    }
    let rest = domain::member_ids(&mut *tx, conv).await?;
    tx.commit().await?;

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
    let uid = uid_of(&headers, &st).await?;
    let me = require_member(&st, conv, uid).await?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can rename"));
    }
    // self 会话 name 恒 NULL（客户端按 kind 显示「文件传输助手」），改名无意义 → 400
    if domain::conv_kind(&st.pool, conv).await?.as_deref() == Some("self") {
        return Err(AppError::bad_request("self conversation cannot be renamed"));
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

/// 群公告：仅群主可改。空字符串 / null 清空三列。变更后 invalidate conversations。
async fn put_announcement(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<AnnouncementReq>,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st).await?;
    let me = require_member(&st, conv, uid).await?;
    let kind = domain::conv_kind(&st.pool, conv).await?.unwrap_or_default();
    if kind != "group" {
        return Err(AppError::bad_request("只有群聊可以设置公告"));
    }
    if me.role != "owner" {
        return Err(AppError::forbidden(
            "only the owner can set the announcement",
        ));
    }
    let text = req
        .text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(t) = &text {
        if t.chars().count() > 2000 {
            return Err(AppError::bad_request("announcement must be 1-2000 chars"));
        }
    }
    sqlx::query(
        "UPDATE conversations
            SET announcement = $2, announcement_by = $3, announcement_at = CASE WHEN $2 IS NULL THEN NULL ELSE now() END
          WHERE id = $1",
    )
    .bind(conv)
    .bind(&text)
    .bind(text.as_ref().map(|_| uid))
    .execute(&st.pool)
    .await?;
    let members = domain::member_ids(&st.pool, conv).await?;
    notify(&st, &members, "conversations");
    let view = announcement_of(&st, conv).await?;
    Ok(Json(match view {
        Some(v) => serde_json::to_value(v).unwrap_or(Value::Null),
        None => json!({"text": null, "updated_by_name": null, "updated_at": null}),
    }))
}

async fn announcement_of(st: &SharedState, conv: Uuid) -> AppResult<Option<AnnouncementView>> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<DateTime<Utc>>)>(
        "SELECT c.announcement, u.display_name, c.announcement_at
           FROM conversations c
           LEFT JOIN users u ON u.id = c.announcement_by
          WHERE c.id = $1",
    )
    .bind(conv)
    .fetch_optional(&st.pool)
    .await?;
    Ok(row.and_then(|(text, name, at)| {
        text.map(|text| AnnouncementView {
            text,
            updated_by_name: name,
            updated_at: at,
        })
    }))
}

#[derive(Serialize)]
struct PinEntry {
    message: MessageView,
    pinned_by_name: Option<String>,
    pinned_at: DateTime<Utc>,
}

/// 置顶列表：只返回未删除消息，按 pinned_at 倒序。非成员 403。
async fn list_pins(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<Json<Vec<PinEntry>>> {
    let uid = uid_of(&headers, &st).await?;
    require_member(&st, conv, uid).await?;
    let rows = sqlx::query_as::<_, (Uuid, Option<String>, DateTime<Utc>)>(
        "SELECT p.message_id, u.display_name, p.pinned_at
           FROM conversation_pins p
           JOIN messages m ON m.id = p.message_id AND m.deleted_at IS NULL
           LEFT JOIN users u ON u.id = p.pinned_by
          WHERE p.conversation_id = $1
          ORDER BY p.pinned_at DESC, p.message_id",
    )
    .bind(conv)
    .fetch_all(&st.pool)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let views = domain::pinned_message_views(&st.pool, conv, uid, &ids).await?;
    let mut by_id: std::collections::HashMap<Uuid, MessageView> =
        views.into_iter().map(|m| (m.id, m)).collect();
    let out = rows
        .into_iter()
        .filter_map(|(id, name, pinned_at)| {
            by_id.remove(&id).map(|message| PinEntry {
                message,
                pinned_by_name: name,
                pinned_at,
            })
        })
        .collect();
    Ok(Json(out))
}

async fn pin_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((conv, message_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    let change = domain::pin_message(&st.pool, conv, message_id, uid)
        .await
        .map_err(post_err)?;
    notify_pins(&st, &change);
    Ok(StatusCode::NO_CONTENT)
}

async fn unpin_message(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((conv, message_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    let change = domain::unpin_message(&st.pool, conv, message_id, uid)
        .await
        .map_err(post_err)?;
    notify_pins(&st, &change);
    Ok(StatusCode::NO_CONTENT)
}

async fn dissolve_conversation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    // 与拉人/退群/踢人共用会话行锁，锁内重检群主身份与解散标记
    let mut tx = st.pool.begin().await?;
    let Some((kind, _, _)) = domain::lock_conversation(&mut tx, conv).await? else {
        return Err(AppError::forbidden("not a member of this conversation"));
    };
    // self 会话不可解散：唯一成员即 owner，不做类型守卫会把助手连同成员一起删掉
    if kind == "self" {
        return Err(AppError::bad_request(
            "self conversation cannot be dissolved",
        ));
    }
    let me = domain::membership(&mut *tx, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can dissolve"));
    }
    // 解散保留消息：标记解散、移出全部成员。最后一人退群仍删除会话。
    let res = sqlx::query(
        "UPDATE conversations SET dissolved_at = now() WHERE id = $1 AND dissolved_at IS NULL",
    )
    .bind(conv)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::conflict("group already dissolved"));
    }
    let members =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM memberships WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM memberships WHERE conversation_id = $1")
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    notify(&st, &members, "conversations");
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 头像 ----------

struct AvatarImage {
    ext: &'static str,
    mime: &'static str,
}

/// 只认文件头魔数，不信任客户端 MIME，也不解码图片。
fn sniff_avatar(buf: &[u8]) -> Option<AvatarImage> {
    if buf.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(AvatarImage {
            ext: "png",
            mime: "image/png",
        })
    } else if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(AvatarImage {
            ext: "jpg",
            mime: "image/jpeg",
        })
    } else if buf.len() >= 12 && buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP" {
        Some(AvatarImage {
            ext: "webp",
            mime: "image/webp",
        })
    } else {
        None
    }
}

fn avatar_file_path(files_dir: &str, stored: &str) -> std::path::PathBuf {
    std::path::Path::new(files_dir).join("avatars").join(stored)
}

/// 提交后异步删旧头像。只接受本目录文件名，避免误删 FILES_DIR 外的路径。
fn spawn_avatar_cleanup(st: &SharedState, stored: Option<String>) {
    let Some(stored) = stored.filter(|s| {
        !s.is_empty()
            && !s.contains('/')
            && !s.contains('\\')
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    }) else {
        return;
    };
    let path = avatar_file_path(&st.cfg.files_dir, &stored);
    tokio::spawn(async move {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(path = %path.display(), error = %e, "old avatar cleanup failed");
        }
    });
}

async fn load_user_row(st: &SharedState, uid: Uuid) -> AppResult<UserRow> {
    sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(uid)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("user not found"))
}

/// 头像变更只影响目录展示：好友与共群成员刷新会话，本人其他连接也要跟上。
async fn notify_avatar_changed(st: &SharedState, uid: Uuid) {
    let mut uids = st.watchers(uid).await.unwrap_or_default();
    uids.push(uid);
    notify(st, &uids, "conversations");
}

/// PUT /users/me/avatar：multipart 字段 `file`。只改自己。
async fn put_avatar(
    State(st): State<SharedState>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st).await?;
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
            if buf.len() > AVATAR_MAX_BYTES {
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
    let img = sniff_avatar(&buf).ok_or_else(|| AppError::bad_request("unsupported image"))?;

    let me = load_user_row(&st, uid).await?;
    let version = me.avatar_version + 1;
    let stored = format!("{uid}-{version}.{}", img.ext);
    let dir = std::path::Path::new(&st.cfg.files_dir).join("avatars");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("create avatar dir: {e}")))?;
    let path = dir.join(&stored);
    if let Err(e) = tokio::fs::write(&path, &buf).await {
        return Err(AppError::internal(format!("store avatar: {e}")));
    }
    let updated = sqlx::query(
        "UPDATE users SET avatar_path = $2, avatar_version = avatar_version + 1
          WHERE id = $1 AND avatar_version = $3",
    )
    .bind(uid)
    .bind(&stored)
    .bind(me.avatar_version)
    .execute(&st.pool)
    .await?;
    if updated.rows_affected() == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::conflict("avatar changed, retry"));
    }
    spawn_avatar_cleanup(&st, me.avatar_path);
    notify_avatar_changed(&st, uid).await;
    let row = load_user_row(&st, uid).await?;
    Ok(Json(UserPublic::from(&row)))
}

async fn delete_avatar(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st).await?;
    let me = load_user_row(&st, uid).await?;
    if me.avatar_path.is_some() {
        sqlx::query(
            "UPDATE users SET avatar_path = NULL, avatar_version = avatar_version + 1
              WHERE id = $1 AND avatar_path IS NOT NULL",
        )
        .bind(uid)
        .execute(&st.pool)
        .await?;
        spawn_avatar_cleanup(&st, me.avatar_path);
        notify_avatar_changed(&st, uid).await;
    }
    let row = load_user_row(&st, uid).await?;
    Ok(Json(UserPublic::from(&row)))
}

#[derive(Deserialize)]
struct AvatarQuery {
    v: Option<i32>,
}

/// GET /users/{id}/avatar：无需鉴权。v 等于当前版本才允许长期缓存。
async fn get_avatar(
    State(st): State<SharedState>,
    Path(id): Path<Uuid>,
    Query(q): Query<AvatarQuery>,
) -> AppResult<impl IntoResponse> {
    let row = sqlx::query_as::<_, (Option<String>, i32)>(
        "SELECT avatar_path, avatar_version FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("user not found"))?;
    let (path, version) = row;
    let stored = path.ok_or_else(|| AppError::not_found("no avatar"))?;
    if stored.contains('/') || stored.contains('\\') || stored.contains("..") {
        return Err(AppError::not_found("no avatar"));
    }
    let data = tokio::fs::read(avatar_file_path(&st.cfg.files_dir, &stored))
        .await
        .map_err(|_| AppError::not_found("no avatar"))?;
    let mime = sniff_avatar(&data)
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

// ---------- 文件上传/下载 ----------

/// 新附件只存 uuid 文件名；历史行可能是带目录的相对路径或绝对路径。
fn stored_file_path(files_dir: &str, stored: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(stored);
    if path.is_absolute() || path.components().count() > 1 {
        path.to_path_buf()
    } else {
        std::path::Path::new(files_dir).join(path)
    }
}

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

/// 413 配额提示：已用按 0.1 MB 精度展示
fn quota_message(used_bytes: i64, quota_mb: i64) -> String {
    format!(
        "存储空间已满（已用 {:.1} MB / 上限 {} MB）",
        used_bytes as f64 / 1024.0 / 1024.0,
        quota_mb
    )
}

/// multipart: conversation_id 字段 + file 字段。上传者必须是会话成员。
async fn upload_file(
    State(st): State<SharedState>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> AppResult<Json<AttachmentView>> {
    let uid = uid_of(&headers, &st).await?;
    // 存储配额（V2 P2-1）：读取中按累计字节早拒，最终以实际写入字节为准。
    // ponytail: 并发上传可略超配额（无锁读-改）；如需严格上限，按 uploader_id 加锁串行化上传。
    let quota_mb = settings::load(&st.pool).await?.storage_quota_mb;
    let quota_bytes = (quota_mb > 0).then(|| quota_mb * 1024 * 1024);
    let used_bytes = match quota_bytes {
        Some(_) => domain::storage_used(&st.pool, uid).await?,
        None => 0,
    };
    let mut conv: Option<Uuid> = None;
    let mut file_name = "file".to_string();
    let mut mime = "application/octet-stream".to_string();
    let mut buf: Vec<u8> = Vec::new();
    let mut got_file = false;
    let mut too_large = false;
    let mut over_quota = false;
    let mut duration_raw: Option<String> = None;

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
            Some("duration_ms") => {
                // 先记下原文，等 MIME 齐了再校验，避免非法时长把文件写进磁盘。
                duration_raw = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| AppError::bad_request("bad duration_ms"))?,
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
                    if too_large || over_quota {
                        continue; // 排空剩余 body，保证错误响应能送达
                    }
                    buf.extend_from_slice(&chunk);
                    if buf.len() > st.cfg.upload_max_bytes {
                        too_large = true;
                        buf.clear();
                    } else if let Some(qb) = quota_bytes {
                        if used_bytes + buf.len() as i64 > qb {
                            over_quota = true;
                            buf.clear();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if too_large {
        return Err(AppError::bad_request("file too large"));
    }
    if over_quota {
        return Err(AppError::payload_too_large(quota_message(
            used_bytes, quota_mb,
        )));
    }
    let conv = conv.ok_or_else(|| AppError::bad_request("missing conversation_id"))?;
    if !got_file || buf.is_empty() {
        return Err(AppError::bad_request("missing file"));
    }
    require_member(&st, conv, uid).await?;
    let duration_ms = match duration_raw {
        Some(raw) => {
            // 语音时长只接受音频附件；普通文件即使漏传字段也不写成语音。
            if !mime_base(&mime).starts_with("audio/") {
                return Err(AppError::bad_request(
                    "duration_ms is only allowed for audio",
                ));
            }
            Some(parse_duration_ms(raw.trim())?)
        }
        None => None,
    };

    let id = Uuid::new_v4();
    // 只存文件名。FILES_DIR 换位置时仍能拼回来；旧的绝对路径在下载时照旧使用。
    let stored = id.to_string();
    let path = stored_file_path(&st.cfg.files_dir, &stored);
    tokio::fs::write(&path, &buf)
        .await
        .map_err(|e| AppError::internal(format!("store file: {e}")))?;
    let size = buf.len() as i64;

    // 图片解码/缩略图（V2 P2-1）：CPU 密集放 blocking 池；解码失败只记日志、照常入库
    let meta = {
        let mime2 = mime.clone();
        tokio::task::spawn_blocking(move || crate::media::inspect_image(&mime2, &buf))
            .await
            .ok()
            .flatten()
    };
    if meta.is_none()
        && matches!(
            mime.as_str(),
            "image/jpeg" | "image/png" | "image/webp" | "image/gif"
        )
    {
        tracing::warn!(mime = %mime, "image decode failed, stored without dims/thumb");
    }
    let (width, height) = meta
        .as_ref()
        .map(|m| (Some(m.width), Some(m.height)))
        .unwrap_or((None, None));
    // 缩略图与原图同目录：存储名加 .thumb.jpg 后缀
    let mut thumb_path = None;
    if let Some(jpg) = meta.as_ref().and_then(|m| m.thumb_jpeg.as_ref()) {
        let tp = format!("{stored}.thumb.jpg");
        match tokio::fs::write(stored_file_path(&st.cfg.files_dir, &tp), jpg).await {
            Ok(()) => thumb_path = Some(tp),
            Err(e) => tracing::warn!(error = %e, "write thumb failed"),
        }
    }
    sqlx::query(
        "INSERT INTO attachments
             (id, uploader_id, conv_id, file_name, mime, size, storage_path, width, height, thumb_path,
              duration_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(uid)
    .bind(conv)
    .bind(&file_name)
    .bind(&mime)
    .bind(size)
    .bind(&stored)
    .bind(width)
    .bind(height)
    .bind(&thumb_path)
    .bind(duration_ms)
    .execute(&st.pool)
    .await?;
    Ok(Json(AttachmentView {
        id,
        file_name,
        mime,
        size,
        width,
        height,
        has_thumb: thumb_path.is_some(),
        duration_ms,
    }))
}

/// 语音时长：仅整数，1–300000 毫秒（5 分钟）。空串、小数、越界都拒绝。
fn parse_duration_ms(raw: &str) -> Result<i32, AppError> {
    let n: i32 = raw
        .parse()
        .map_err(|_| AppError::bad_request("bad duration_ms"))?;
    if !(1..=300_000).contains(&n) {
        return Err(AppError::bad_request("duration_ms must be 1-300000"));
    }
    Ok(n)
}

fn mime_base(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// 按 UUID 直链下载（能力 URL：id 不可猜测，仅会话内可见）。
#[derive(Deserialize)]
struct FileQuery {
    dl: Option<u8>,
    thumb: Option<u8>,
}

/// 允许 inline 预览的 MIME 白名单：仅渲染型媒体。SVG 可携带脚本，必须排除；
/// text/html、xhtml、xml 等一律走 attachment，避免在聊天 origin 上同源执行。
fn inline_safe(mime: &str) -> bool {
    let m = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    (m.starts_with("image/") && !m.contains("svg"))
        || m.starts_with("audio/")
        || m.starts_with("video/")
        || m == "application/pdf"
}

async fn download_file(
    State(st): State<SharedState>,
    Path(id): Path<Uuid>,
    Query(q): Query<FileQuery>,
) -> AppResult<impl IntoResponse> {
    let (file_name, mime, stored, thumb_stored) =
        sqlx::query_as::<_, (String, String, String, Option<String>)>(
            "SELECT file_name, mime, storage_path, thumb_path FROM attachments WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("file not found"))?;
    // ?thumb=1：有缩略图返回 JPEG 缩略图（缓存头与原图一致）；没有/读取失败回退原图。dl=1 忽略 thumb。
    if q.thumb == Some(1) && q.dl != Some(1) {
        if let Some(tp) = &thumb_stored {
            if let Ok(data) = tokio::fs::read(stored_file_path(&st.cfg.files_dir, tp)).await {
                let dispo = format!(
                    "inline; filename*=UTF-8''{}",
                    pct_encode(&format!("{file_name}.thumb.jpg"))
                );
                return Ok((
                    [
                        (header::CONTENT_TYPE, "image/jpeg".to_string()),
                        (header::CONTENT_DISPOSITION, dispo),
                        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                        (
                            header::CACHE_CONTROL,
                            "private, max-age=31536000, immutable".into(),
                        ),
                    ],
                    data,
                ));
            }
        }
    }
    let data = tokio::fs::read(stored_file_path(&st.cfg.files_dir, &stored))
        .await
        .map_err(|_| AppError::not_found("file not found"))?;
    // dl=1 → attachment 触发下载；inline 只留给安全媒体白名单（防 Stored XSS）
    let mode = if q.dl != Some(1) && inline_safe(&mime) {
        "inline"
    } else {
        "attachment"
    };
    let dispo = format!("{}; filename*=UTF-8''{}", mode, pct_encode(&file_name));
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, dispo),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".into(),
            ),
        ],
        data,
    ))
}

// ---------- ws ticket ----------

async fn ws_ticket(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<TicketRes>> {
    let authn = auth::require_auth(&st, &headers).await?;
    let ticket = st.issue_ticket(authn.user.id, authn.user.session_version as i64, authn.sid);
    Ok(Json(TicketRes {
        ticket,
        expires_in: 30,
    }))
}
#[derive(Deserialize)]
struct TransferReq {
    user_id: Uuid,
}

/// 群主转让：仅群主；目标必须是普通成员。全程持会话行锁，两步角色变更必须各自命中。
async fn transfer_ownership(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Json(req): Json<TransferReq>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    if uid == req.user_id {
        return Err(AppError::bad_request("already the owner"));
    }
    let mut tx = st.pool.begin().await?;
    let Some((kind, _, _)) = domain::lock_conversation(&mut tx, conv).await? else {
        return Err(AppError::forbidden("not a member of this conversation"));
    };
    // self 会话归属本人不可转让（唯一成员即 owner）
    if kind == "self" {
        return Err(AppError::bad_request(
            "self conversation cannot be transferred",
        ));
    }
    let me = domain::membership(&mut *tx, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    if me.role != "owner" {
        return Err(AppError::forbidden("only the owner can transfer ownership"));
    }
    let target = domain::membership(&mut *tx, conv, req.user_id)
        .await?
        .ok_or_else(|| AppError::not_found("member not found"))?;
    if target.role == "owner" {
        return Err(AppError::bad_request("target is already the owner"));
    }

    let res = sqlx::query(
        "UPDATE memberships SET role = 'member'
         WHERE conversation_id = $1 AND user_id = $2 AND role = 'owner'",
    )
    .bind(conv)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() != 1 {
        return Err(AppError::internal("ownership transfer failed"));
    }
    let res = sqlx::query(
        "UPDATE memberships SET role = 'owner'
         WHERE conversation_id = $1 AND user_id = $2 AND role = 'member'",
    )
    .bind(conv)
    .bind(req.user_id)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() != 1 {
        return Err(AppError::internal("ownership transfer failed"));
    }
    sqlx::query("UPDATE conversations SET owner_id = $2 WHERE id = $1")
        .bind(conv)
        .bind(req.user_id)
        .execute(&mut *tx)
        .await?;
    let members = domain::member_ids(&mut *tx, conv).await?;
    tx.commit().await?;

    notify(&st, &members, "conversations");
    Ok(StatusCode::NO_CONTENT)
}

// ---------- 后台管理（仅 admin，账号级角色，与会话内 owner 无关） ----------

/// 撤销某用户全部有效 refresh token（禁用/改密时用）
async fn revoke_all_refresh(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    uid: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(uid)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 撤销某用户全部登录会话（禁用/管理员重置密码时与 refresh 一并撤销）
async fn revoke_all_sessions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    uid: Uuid,
) -> AppResult<()> {
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL")
        .bind(uid)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn admin_overview(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<AdminOverview>> {
    auth::require_admin(&st, &headers).await?;
    let (user_count, admin_count, disabled_count) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT count(*),
                count(*) FILTER (WHERE role = 'admin'),
                count(*) FILTER (WHERE disabled_at IS NOT NULL)
           FROM users",
    )
    .fetch_one(&st.pool)
    .await?;
    let (dm_count, group_count) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE type = 'dm'),
                count(*) FILTER (WHERE type = 'group')
           FROM conversations",
    )
    .fetch_one(&st.pool)
    .await?;
    let message_count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages")
        .fetch_one(&st.pool)
        .await?;
    let (attachment_count, attachment_bytes) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*), COALESCE(sum(size), 0)::bigint FROM attachments",
    )
    .fetch_one(&st.pool)
    .await?;
    let online_users = st.conns.iter().filter(|e| !e.value().is_empty()).count() as i64;
    let online_conns = st.conns.iter().map(|e| e.value().len() as i64).sum();
    Ok(Json(AdminOverview {
        user_count,
        admin_count,
        disabled_count,
        message_count,
        dm_count,
        group_count,
        attachment_count,
        attachment_bytes,
        online_users,
        online_conns,
    }))
}

const ADMIN_PAGE: i64 = 50;

async fn admin_users(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<AdminUsersQuery>,
) -> AppResult<Json<Vec<AdminUserView>>> {
    auth::require_admin(&st, &headers).await?;
    let rows = if let Some(needle) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let esc = needle
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        sqlx::query_as::<_, UserRow>(
            "SELECT * FROM users
              WHERE username ILIKE '%' || $1 || '%' ESCAPE '\\'
                 OR display_name ILIKE '%' || $1 || '%' ESCAPE '\\'
              ORDER BY created_at LIMIT $2 OFFSET $3",
        )
        .bind(esc)
        .bind(ADMIN_PAGE)
        .bind(q.offset.max(0))
        .fetch_all(&st.pool)
        .await?
    } else {
        sqlx::query_as::<_, UserRow>("SELECT * FROM users ORDER BY created_at LIMIT $1 OFFSET $2")
            .bind(ADMIN_PAGE)
            .bind(q.offset.max(0))
            .fetch_all(&st.pool)
            .await?
    };
    // 每人附件已用存储（V2 P2-1）：按 storage_path 去重，转发副本不重复计
    let ids: Vec<Uuid> = rows.iter().map(|u| u.id).collect();
    let usage: std::collections::HashMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT uploader_id, SUM(size)::bigint
           FROM (SELECT DISTINCT uploader_id, storage_path, size
                   FROM attachments WHERE uploader_id = ANY($1)) d
          GROUP BY uploader_id",
    )
    .bind(&ids)
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .collect();
    Ok(Json(
        rows.iter()
            .map(|u| AdminUserView {
                id: u.id,
                username: u.username.clone(),
                display_name: u.display_name.clone(),
                role: u.role.clone(),
                disabled_at: u.disabled_at,
                last_seen_at: u.last_seen_at,
                created_at: u.created_at,
                online: st.is_online(u.id),
                storage_bytes: usage.get(&u.id).copied().unwrap_or(0),
                avatar_version: u.avatar_version,
                has_avatar: u.avatar_path.is_some(),
            })
            .collect(),
    ))
}

/// 禁用：顾问锁 + 行锁串行化 → 置标记并 bump 凭据版本 → 撤 refresh → 踢下线（帧 + 主动断连）
async fn admin_disable(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(target): Path<Uuid>,
) -> AppResult<StatusCode> {
    auth::require_admin(&st, &headers).await?;
    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADMIN_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let (role, already) = sqlx::query_as::<_, (String, bool)>(
        "SELECT role, disabled_at IS NOT NULL FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::not_found("user not found"))?;
    if role == "admin" {
        return Err(AppError::bad_request("管理员账号不可禁用"));
    }
    if !already {
        sqlx::query(
            "UPDATE users SET disabled_at = now(), session_version = session_version + 1
             WHERE id = $1",
        )
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    revoke_all_refresh(&mut tx, target).await?;
    revoke_all_sessions(&mut tx, target).await?;
    tx.commit().await?;
    if let Some(epoch) = st.kick_user(target, "disabled") {
        ws::publish_offline(&st, target, epoch).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_enable(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(target): Path<Uuid>,
) -> AppResult<StatusCode> {
    auth::require_admin(&st, &headers).await?;
    // 与 disable/set_role 共用顾问锁：disabled 变更串行化，恢复管理员可用性的时点可预期
    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADMIN_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = $1")
        .bind(target)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("user not found"));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 任命/撤销管理员。顾问锁串行化并发降权；末位保护按「可用（未禁用）管理员」计数；
/// 被禁用的账号不能被任命为管理员。
async fn admin_set_role(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(target): Path<Uuid>,
    Json(req): Json<SetRoleReq>,
) -> AppResult<StatusCode> {
    auth::require_admin(&st, &headers).await?;
    if req.role != "admin" && req.role != "user" {
        return Err(AppError::bad_request("role must be admin or user"));
    }
    let mut tx = st.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADMIN_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let (cur, disabled) = sqlx::query_as::<_, (String, bool)>(
        "SELECT role, disabled_at IS NOT NULL FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::not_found("user not found"))?;
    if req.role == "admin" {
        if disabled {
            return Err(AppError::bad_request("禁用账号不能设为管理员"));
        }
    } else if cur == "admin" {
        // 撤管理员：要求目标之外仍存在至少一名可用管理员，防止并发双降权把系统锁死
        let usable = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM users
              WHERE role = 'admin' AND disabled_at IS NULL AND id <> $1",
        )
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        if usable == 0 {
            return Err(AppError::bad_request("至少保留一名可用管理员"));
        }
    }
    sqlx::query("UPDATE users SET role = $2 WHERE id = $1")
        .bind(target)
        .bind(&req.role)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 重置他人密码：改哈希 + bump 凭据版本（旧 access token/WS ticket 即失效）
/// → 撤 refresh → 踢下线强制重登
async fn admin_reset_password(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(target): Path<Uuid>,
    Json(req): Json<ResetPasswordReq>,
) -> AppResult<StatusCode> {
    auth::require_admin(&st, &headers).await?;
    if !(6..=72).contains(&req.password.len()) {
        return Err(AppError::bad_request("password must be 6-72 chars"));
    }
    let hash = auth::hash_password(&req.password)?;
    let mut tx = st.pool.begin().await?;
    let res = sqlx::query(
        "UPDATE users SET password_hash = $2, session_version = session_version + 1
         WHERE id = $1",
    )
    .bind(target)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("user not found"));
    }
    revoke_all_refresh(&mut tx, target).await?;
    revoke_all_sessions(&mut tx, target).await?;
    tx.commit().await?;
    if let Some(epoch) = st.kick_user(target, "password_reset") {
        ws::publish_offline(&st, target, epoch).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
