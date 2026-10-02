use super::*;

// ---------- users ----------

pub(super) async fn get_me(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<UserPublic>> {
    let uid = uid_of(&headers, &st).await?;
    let row = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(self_public(&st, &row).await?))
}

/// GET /users/me/storage（V2 P2-1）：本人附件用量与配额；quota_bytes 为 null 表示不限
pub(super) async fn my_storage(
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

pub(super) async fn update_me(
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
    Ok(Json(self_public(&st, &row).await?))
}

#[derive(Deserialize)]
pub(super) struct SearchQuery {
    q: String,
}

/// 用户搜索：username/display_name 模糊匹配（不含自己），精确命中排前
pub(super) async fn search_user(
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
