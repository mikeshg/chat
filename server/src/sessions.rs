//! 登录会话（设备）管理与自助改密码（V2 P1-5）。
//! 一次登录 = 一条 sessions 行 = 一台设备；撤销 session 连带其全部 refresh token 与在线连接。
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::model::{AuthResponse, ChangePasswordReq, SessionView, UserRow};
use crate::state::SharedState;
use crate::ws;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// 去控制字符并按字符数截断（设备名/UA 落库前的统一清洗）
pub(crate) fn clip_text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// 登录/注册请求体里的 device_label → 清洗后截断 60 字符
pub(crate) fn device_label_of(label: Option<&str>) -> String {
    label.map(|s| clip_text(s, 60)).unwrap_or_default()
}

/// 请求头 User-Agent → 截断 300 字符
pub(crate) fn user_agent_of(headers: &HeaderMap) -> String {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| clip_text(s, 300))
        .unwrap_or_default()
}

/// 撤销某个 session 的全部未撤销 refresh token
async fn revoke_session_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now() WHERE session_id = $1 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 撤销某 session 行本身
async fn revoke_session_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Uuid,
) -> AppResult<()> {
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 自助改密码：验证旧密码 → 改哈希并 bump session_version → 撤销其他 session 及其
/// refresh token、踢掉其连接；当前 session 不变并拿到新签发的凭据。
/// 旧密码错误返回 403 而非 401（401 会让客户端误以为 token 失效去刷新）。
pub async fn change_password(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> AppResult<Json<AuthResponse>> {
    let authn = auth::require_auth(&st, &headers).await?;
    let uid = authn.user.id;
    crate::api::rate_limit(&st, format!("pw:{uid}"))?;
    if !(6..=72).contains(&req.new_password.len()) {
        return Err(AppError::bad_request("password must be 6-72 chars"));
    }

    let mut tx = st.pool.begin().await?;
    // 用户行锁：与登录/refresh/管理员重置共用同一加锁顺序（先用户行），串行化并发改密
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1 FOR UPDATE")
        .bind(uid)
        .fetch_one(&mut *tx)
        .await?;
    if !auth::verify_password(&row.password_hash, &req.old_password) {
        return Err(AppError::forbidden("原密码不正确"));
    }
    let hash = auth::hash_password(&req.new_password)?;
    let row = sqlx::query_as::<_, UserRow>(
        "UPDATE users SET password_hash = $2, session_version = session_version + 1
         WHERE id = $1 RETURNING *",
    )
    .bind(uid)
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await?;

    // 老 token 没有 sid：以本次改密为新登录起点补建一条 session
    let sid = match authn.sid {
        Some(sid) => sid,
        None => auth::create_session(&mut *tx, uid, "", &user_agent_of(&headers)).await?,
    };
    // 撤销其他 session 及其全部 refresh token（含未回填 session_id 的老 token）
    sqlx::query(
        "UPDATE sessions SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL AND id <> $2",
    )
    .bind(uid)
    .bind(sid)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL AND session_id IS DISTINCT FROM $2",
    )
    .bind(uid)
    .bind(sid)
    .execute(&mut *tx)
    .await?;
    let res = auth::issue_tokens(&mut tx, &st.cfg, &row, sid).await?;
    tx.commit().await?;

    // 其他 session 的连接收 force_logout 后断开；本 session 的旧连接 sv 已失效，
    // 下一帧复核自然断开（客户端改密成功后主动重建 WS）
    if let Some(epoch) = st.kick_other_sessions(uid, Some(sid), "password_changed") {
        ws::publish_offline(&st, uid, epoch).await;
    }
    Ok(Json(res))
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    device_label: String,
    user_agent: String,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
}

/// 登录设备列表：只列未撤销且仍有未过期 refresh token 的 session，最近使用排前
pub async fn list_sessions(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<SessionView>>> {
    let authn = auth::require_auth(&st, &headers).await?;
    let uid = authn.user.id;
    let rows = sqlx::query_as::<_, SessionRow>(
        "SELECT s.id, s.device_label, s.user_agent, s.created_at, s.last_used_at
           FROM sessions s
          WHERE s.user_id = $1 AND s.revoked_at IS NULL
            AND EXISTS(SELECT 1 FROM refresh_tokens rt
                        WHERE rt.session_id = s.id
                          AND rt.revoked_at IS NULL AND rt.expires_at > now())
          ORDER BY s.last_used_at DESC",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| SessionView {
                id: r.id,
                device_label: r.device_label,
                user_agent: r.user_agent,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
                current: authn.sid == Some(r.id),
                online: st.session_online(uid, r.id),
            })
            .collect(),
    ))
}

/// 移出自己的一台设备：撤销 session + 其 refresh token + 踢连接。非本人 session → 404。幂等。
pub async fn revoke_session(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(sid): Path<Uuid>,
) -> AppResult<StatusCode> {
    let authn = auth::require_auth(&st, &headers).await?;
    let uid = authn.user.id;
    if authn.sid == Some(sid) {
        return Err(AppError::bad_request("当前设备请直接退出登录"));
    }
    let mut tx = st.pool.begin().await?;
    let owner = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM sessions WHERE id = $1")
        .bind(sid)
        .fetch_optional(&mut *tx)
        .await?;
    // 不存在与属于他人同样 404：不暴露他人 session 的存在性
    let Some(owner) = owner else {
        return Err(AppError::not_found("session not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("session not found"));
    }
    revoke_session_row(&mut tx, sid).await?;
    revoke_session_tokens(&mut tx, sid).await?;
    tx.commit().await?;

    if let Some(epoch) = st.kick_session(uid, sid, "session_revoked") {
        ws::publish_offline(&st, uid, epoch).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// 一键退出其他所有设备：撤销除当前外的全部 session 及其 refresh token、踢连接。
pub async fn revoke_other_sessions(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<StatusCode> {
    let authn = auth::require_auth(&st, &headers).await?;
    let uid = authn.user.id;
    let mut tx = st.pool.begin().await?;
    // 用户行锁：与改密/refresh/禁用共用加锁顺序，串行化并发撤销
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE sessions SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL AND id IS DISTINCT FROM $2",
    )
    .bind(uid)
    .bind(authn.sid)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL AND session_id IS DISTINCT FROM $2",
    )
    .bind(uid)
    .bind(authn.sid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(epoch) = st.kick_other_sessions(uid, authn.sid, "session_revoked") {
        ws::publish_offline(&st, uid, epoch).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
