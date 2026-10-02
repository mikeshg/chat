use super::*;

// ---------- friends ----------

#[derive(sqlx::FromRow)]
pub(super) struct FriendRow {
    id: Uuid,
    username: String,
    display_name: String,
    role: String,
    last_seen_at: Option<DateTime<Utc>>,
    since: DateTime<Utc>,
    avatar_version: i32,
    has_avatar: bool,
    is_bot: bool,
}

pub(super) async fn list_friends(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<FriendView>>> {
    let uid = uid_of(&headers, &st).await?;
    let rows = sqlx::query_as::<_, FriendRow>(
        "SELECT u.id, u.username::text AS username, u.display_name, u.role, u.last_seen_at,
                f.created_at AS since, u.avatar_version, u.avatar_path IS NOT NULL AS has_avatar,
                u.bot_owner_id IS NOT NULL AS is_bot
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
                    is_bot: r.is_bot,
                },
                since: r.since,
            })
            .collect(),
    ))
}

pub(super) async fn delete_friend(
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
pub(super) struct ReqRow {
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

pub(super) async fn list_requests(
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
                    is_bot: false,
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

pub(super) async fn create_request(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<FriendReqCreate>,
) -> AppResult<Json<Value>> {
    let uid = uid_of(&headers, &st).await?;
    let to = sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users WHERE username = $1 AND disabled_at IS NULL AND bot_owner_id IS NULL",
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
pub(super) async fn accept_internal(st: &SharedState, req_id: Uuid) -> AppResult<(Uuid, Uuid)> {
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

pub(super) async fn accept_request(
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

pub(super) async fn decline_request(
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
