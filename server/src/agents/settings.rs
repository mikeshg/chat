use super::*;

// ---------- 设置：个人默认 + 单个 Agent 覆盖 ----------

pub(super) const OUTPUT_MODES: [&str; 3] = ["stream", "status", "steps"];
/// 排队超时上限 30 天；0 = 永不
pub(super) const MAX_QUEUE_TIMEOUT_SECS: i32 = 30 * 24 * 3600;

pub(super) fn valid_output_mode(m: &str) -> bool {
    OUTPUT_MODES.contains(&m)
}

pub(super) fn valid_queue_timeout(secs: i32) -> bool {
    (0..=MAX_QUEUE_TIMEOUT_SECS).contains(&secs)
}

pub(super) async fn user_settings(pool: &PgPool, uid: Uuid) -> Result<AgentUserSettings, sqlx::Error> {
    Ok(sqlx::query_as::<_, AgentUserSettings>(
        "SELECT output_mode, queue_timeout_secs, max_call_depth
           FROM agent_user_settings WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?
    .unwrap_or(AgentUserSettings {
        output_mode: "stream".into(),
        queue_timeout_secs: 86400,
        max_call_depth: 3,
    }))
}

/// 某个 Agent 的生效设置：Agent 覆盖 → 主人的个人默认 → 系统默认
pub async fn effective_settings(
    pool: &PgPool,
    agent_id: Uuid,
) -> Result<AgentUserSettings, sqlx::Error> {
    sqlx::query_as::<_, AgentUserSettings>(
        "SELECT COALESCE(a.output_mode, s.output_mode, 'stream') AS output_mode,
                COALESCE(a.queue_timeout_secs, s.queue_timeout_secs, 86400) AS queue_timeout_secs,
                COALESCE(s.max_call_depth, 3) AS max_call_depth
           FROM agents a
           LEFT JOIN agent_user_settings s ON s.user_id = a.owner_id
          WHERE a.id = $1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
}

pub async fn get_user_settings(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<AgentUserSettings>> {
    let uid = uid(&headers, &st).await?;
    Ok(Json(user_settings(&st.pool, uid).await?))
}

pub async fn put_user_settings(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<UpdateAgentUserSettingsReq>,
) -> AppResult<Json<AgentUserSettings>> {
    let uid = uid(&headers, &st).await?;
    let mut cur = user_settings(&st.pool, uid).await?;
    if let Some(m) = req.output_mode {
        if !valid_output_mode(&m) {
            return Err(AppError::bad_request("bad_output_mode"));
        }
        cur.output_mode = m;
    }
    if let Some(t) = req.queue_timeout_secs {
        if !valid_queue_timeout(t) {
            return Err(AppError::bad_request("bad_queue_timeout"));
        }
        cur.queue_timeout_secs = t;
    }
    if let Some(d) = req.max_call_depth {
        if !(1..=10).contains(&d) {
            return Err(AppError::bad_request("bad_max_call_depth"));
        }
        cur.max_call_depth = d;
    }
    sqlx::query(
        "INSERT INTO agent_user_settings (user_id, output_mode, queue_timeout_secs, max_call_depth)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id) DO UPDATE
            SET output_mode = $2, queue_timeout_secs = $3, max_call_depth = $4, updated_at = now()",
    )
    .bind(uid)
    .bind(&cur.output_mode)
    .bind(cur.queue_timeout_secs)
    .bind(cur.max_call_depth)
    .execute(&st.pool)
    .await?;
    Ok(Json(cur))
}

/// PATCH /v1/agents/{id}：缺省字段不改。已删除 / 别人的 Agent → 404。
/// 带 host_id → 400 host_immutable（宿主在添加时选定，不能换）。
pub async fn patch_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchAgentReq>,
) -> AppResult<Json<AgentView>> {
    let ctx = auth::require_auth(&st, &headers).await?;
    let uid = ctx.user.id;
    if !crate::agent_access::user_allows(&st.pool, uid).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    if let Some(Some(m)) = &req.output_mode {
        if !valid_output_mode(m) {
            return Err(AppError::bad_request("bad_output_mode"));
        }
    }
    if let Some(Some(t)) = req.queue_timeout_secs {
        if !valid_queue_timeout(t) {
            return Err(AppError::bad_request("bad_queue_timeout"));
        }
    }
    let name = match &req.name {
        Some(n) if !valid_name(n) => return Err(AppError::bad_request("bad_name")),
        Some(n) => Some(n.clone()),
        None => None,
    };
    // runtime 与 cwd 各自缺省 = 不改，避免只改命令时把工作目录清掉
    let new_runtime = match &req.runtime {
        Some(rt) => Some(normalize_runtime(rt, "")?.0),
        None => None,
    };
    let new_cwd = match &req.cwd {
        Some(cwd) => {
            let cwd = cwd.trim().to_string();
            if cwd.chars().count() > 500 {
                return Err(AppError::bad_request("bad_runtime"));
            }
            Some(cwd)
        }
        None => None,
    };
    let extra_dirs = match &req.extra_dirs {
        Some(d) => Some(check_extra_dirs(d)?),
        None => None,
    };
    let model = match &req.model {
        Some(m) => Some(check_token(m, "bad_model")?),
        None => None,
    };
    let mode = match &req.mode {
        Some(m) => Some(check_token(m, "bad_mode")?),
        None => None,
    };
    if let Some(s) = &req.instructions {
        check_instructions(s)?;
    }
    let env = match &req.env {
        Some(v) => Some(check_env(v)?),
        None => None,
    };
    let permission_tier = match req.permission_tier.as_deref() {
        Some(t) => Some(normalize_tier(Some(t))?),
        None => None,
    };

    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, ExecRow>(
        "SELECT owner_id, deleted_at, host_id, runtime, cwd, extra_dirs, env, permission_tier
           FROM agents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    // 已删除与不存在、别人的 Agent 一样 404，不暴露存在性
    let Some(cur) = row.filter(|r| r.owner_id == uid && r.deleted_at.is_none()) else {
        return Err(AppError::not_found("agent not found"));
    };
    if req.host_id {
        return Err(AppError::bad_request("host_immutable"));
    }
    // 决定宿主上跑什么、在哪跑、能做多少的字段，和添加一样只能在那台电脑本机改：
    // 否则任何拿到登录凭据的客户端都能把命令换掉，再发条消息就在宿主上执行。
    // 前端保存时整表单提交，所以只拦真正变化的字段；降权限档位任何客户端都可以。
    let changes_exec = new_runtime.as_ref().is_some_and(|v| *v != cur.runtime)
        || new_cwd.as_ref().is_some_and(|v| *v != cur.cwd)
        || extra_dirs.as_ref().is_some_and(|v| *v != cur.extra_dirs)
        || env.as_ref().is_some_and(|v| *v != cur.env)
        || permission_tier
            .as_deref()
            .is_some_and(|t| tier_rank(t) > tier_rank(&cur.permission_tier));
    if changes_exec && !st.host_is_local_session(cur.host_id, ctx.sid) {
        return Err(AppError::forbidden("host_not_local"));
    }
    if let Some(m) = req.output_mode {
        sqlx::query("UPDATE agents SET output_mode = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(t) = req.queue_timeout_secs {
        sqlx::query("UPDATE agents SET queue_timeout_secs = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(t)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(n) = name {
        match sqlx::query("UPDATE agents SET name = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(n)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.constraint() == Some("uq_agents_owner_name") => {
                return Err(AppError::conflict("name_taken"));
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(runtime) = new_runtime {
        sqlx::query("UPDATE agents SET runtime = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(runtime)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(cwd) = new_cwd {
        sqlx::query("UPDATE agents SET cwd = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(cwd)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(dirs) = extra_dirs {
        sqlx::query("UPDATE agents SET extra_dirs = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(dirs)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(m) = model {
        sqlx::query("UPDATE agents SET model = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(m) = mode {
        sqlx::query("UPDATE agents SET mode = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(s) = req.instructions {
        sqlx::query("UPDATE agents SET instructions = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(s)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(env) = env {
        sqlx::query("UPDATE agents SET env = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(env)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(tier) = permission_tier {
        sqlx::query("UPDATE agents SET permission_tier = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(tier)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(load_agent_view(&st, id).await?))
}

pub(super) async fn load_agent_view(st: &SharedState, id: Uuid) -> AppResult<AgentView> {
    let row = sqlx::query_as::<_, AgentDbRow>(&format!("{AGENT_SELECT} WHERE a.id = $1"))
        .bind(id)
        .fetch_one(&st.pool)
        .await?;
    Ok(agent_view(st, row))
}

/// DELETE /v1/agents/{id}：软删除。进行中的调用改 failed/agent_deleted，并通知宿主取消。
pub async fn delete_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    let mut tx = st.pool.begin().await?;
    let row = sqlx::query_as::<_, (Uuid, Uuid, Option<DateTime<Utc>>)>(
        "SELECT owner_id, host_id, deleted_at FROM agents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner, host_id, None)) = row else {
        return Err(AppError::not_found("agent not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("agent not found"));
    }
    sqlx::query("UPDATE agents SET deleted_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = 'failed', error = 'agent_deleted',
                finished_at = now()
          WHERE agent_id = $1 AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    for inv in &invs {
        let cancel = json!({"type": "invocation_cancel", "invocation_id": inv.id}).to_string();
        st.send_to_host(host_id, &cancel);
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_else(|_| vec![owner]);
        let members = with_root_members(&st, &members, inv).await;
        push_invocation(&st, &members, inv);
        crate::approvals::expire_invocation(&st, inv.id).await;
    }
    // 删掉的 Agent 自己的调用已失败；它派出的未结束子调用一并取消
    let roots: Vec<Uuid> = invs.iter().map(|i| i.id).collect();
    cascade_cancel(&st, &roots, None).await;
    // 会话列表要带上 deleted；Agent 列表面板重拉
    crate::api::notify(&st, &[owner], "conversations");
    crate::api::notify(&st, &[owner], "agents");
    Ok(StatusCode::NO_CONTENT)
}

/// 试读可能要冷启动 npx 包，给足时间。
pub(super) const PROBE_WAIT: Duration = Duration::from_secs(60);

/// POST /v1/agents/{id}/probe：让宿主重新读一次可选模型 / 模式，等宿主回 probe_result
/// 再返回 {modes, models, error}。离线 409，超时 504。结果由宿主另发的 agent_options 落库。
pub async fn probe_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let uid = uid(&headers, &st).await?;
    let cfg = load_cfg(&st.pool, id)
        .await?
        .filter(|a| a.owner == uid && a.deleted_at.is_none())
        .ok_or_else(|| AppError::not_found("agent not found"))?;
    if !st.host_online(cfg.host_id) {
        return Err(AppError::conflict("host_offline"));
    }
    let request_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.registry_wait.insert(request_id, tx);
    let payload = json!({
        "type": "agent_probe",
        "request_id": request_id,
        "agent": agent_snapshot(&st.pool, &cfg).await,
    })
    .to_string();
    if !st.send_to_host(cfg.host_id, &payload) {
        st.registry_wait.remove(&request_id);
        return Err(AppError::conflict("host_offline"));
    }
    let v = match tokio::time::timeout(PROBE_WAIT, rx).await {
        Ok(Ok(v)) => v,
        _ => {
            st.registry_wait.remove(&request_id);
            return Err(AppError::gateway_timeout("probe_timeout"));
        }
    };
    let mut out = clean_reported(&v);
    out["error"] = match v["error"].as_str() {
        Some(e) => json!(e.chars().take(500).collect::<String>()),
        None => Value::Null,
    };
    Ok(Json(out))
}

/// 宿主上报 agent_options：写入 reported_options，并让主人重拉 Agent 列表。
pub async fn handle_options(st: &SharedState, host_id: Uuid, v: &Value) {
    let Some(agent_id) = v["agent_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
        return;
    };
    let stored = clean_reported(v);
    let owner = sqlx::query_scalar::<_, Uuid>(
        "UPDATE agents SET reported_options = $2, updated_at = now()
          WHERE id = $1 AND host_id = $3 AND deleted_at IS NULL
         RETURNING owner_id",
    )
    .bind(agent_id)
    .bind(&stored)
    .bind(host_id)
    .fetch_optional(&st.pool)
    .await;
    if let Ok(Some(owner)) = owner {
        crate::api::notify(st, &[owner], "agents");
    }
}

pub(super) fn clean_reported(v: &Value) -> Value {
    json!({
        "modes": clean_option_list(&v["modes"]),
        "models": clean_option_list(&v["models"]),
    })
}

pub(super) fn clean_option_list(v: &Value) -> Vec<Value> {
    v.as_array()
        .map(|a| {
            a.iter()
                .take(100)
                .filter_map(|item| {
                    let id: String = item["id"].as_str()?.chars().take(200).collect();
                    if id.is_empty() {
                        return None;
                    }
                    let name: String = item["name"]
                        .as_str()
                        .unwrap_or(&id)
                        .chars()
                        .take(200)
                        .collect();
                    let name = if name.is_empty() { id.clone() } else { name };
                    Some(json!({"id": id, "name": name}))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// GET /v1/agent-builtin/models（A28）：内置 Agent 的可选模型。
/// 端点×模型展开成 [{id: "<端点id>/<模型>", name: "<端点名> · <模型>"}]；
/// 不带 base_url / api_key。无使用权 403（与普通 Agent REST 同一条闸）。
pub async fn builtin_models(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<Value>>> {
    uid(&headers, &st).await?;
    let eps = crate::settings::load_builtin_endpoints(&st.pool).await?;
    let mut out = Vec::new();
    for e in &eps {
        for m in &e.models {
            out.push(json!({
                "id": format!("{}/{}", e.id, m),
                "name": format!("{} · {}", e.name, m),
            }));
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(crate) struct AvatarQuery {
    pub(super) v: Option<i32>,
}

/// PUT /v1/agents/{id}/avatar：PNG / JPEG / WebP，规则与用户头像相同。成功版本 +1。
pub async fn put_agent_avatar(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    mut mp: Multipart,
) -> AppResult<Json<AgentView>> {
    let uid = uid(&headers, &st).await?;
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
            if buf.len() > crate::api::AVATAR_MAX_BYTES {
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
    let img =
        crate::api::sniff_avatar(&buf).ok_or_else(|| AppError::bad_request("unsupported image"))?;

    let cur = sqlx::query_as::<_, (Uuid, i32, Option<String>, Option<DateTime<Utc>>)>(
        "SELECT owner_id, avatar_version, avatar_path, deleted_at FROM agents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some((owner, version, old_path, None)) = cur else {
        return Err(AppError::not_found("agent not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("agent not found"));
    }
    let next = version + 1;
    let stored = format!(
        "{id}-{next}-{}.{}",
        &Uuid::new_v4().simple().to_string()[..8],
        img.ext
    );
    let dir = std::path::Path::new(&st.cfg.files_dir).join("avatars");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("create avatar dir: {e}")))?;
    let path = dir.join(&stored);
    if let Err(e) = tokio::fs::write(&path, &buf).await {
        return Err(AppError::internal(format!("store avatar: {e}")));
    }
    let updated = sqlx::query(
        "UPDATE agents SET avatar_path = $2, avatar_version = avatar_version + 1, updated_at = now()
          WHERE id = $1 AND avatar_version = $3 AND deleted_at IS NULL AND owner_id = $4",
    )
    .bind(id)
    .bind(&stored)
    .bind(version)
    .bind(uid)
    .execute(&st.pool)
    .await?;
    if updated.rows_affected() == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::conflict("avatar changed, retry"));
    }
    crate::api::spawn_avatar_cleanup(&st, old_path);
    crate::api::notify(&st, &[uid], "agents");
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(load_agent_view(&st, id).await?))
}

/// GET /v1/agents/{id}/avatar：无需鉴权。v 等于当前版本才长期缓存。已删除的 Agent 头像仍可取。
pub(crate) async fn get_agent_avatar(
    State(st): State<SharedState>,
    Path(id): Path<Uuid>,
    Query(q): Query<AvatarQuery>,
) -> AppResult<impl IntoResponse> {
    let row = sqlx::query_as::<_, (Option<String>, i32)>(
        "SELECT avatar_path, avatar_version FROM agents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("agent not found"))?;
    let (path, version) = row;
    let stored = path.ok_or_else(|| AppError::not_found("no avatar"))?;
    if stored.contains('/') || stored.contains('\\') || stored.contains("..") {
        return Err(AppError::not_found("no avatar"));
    }
    let data = tokio::fs::read(crate::api::avatar_file_path(&st.cfg.files_dir, &stored))
        .await
        .map_err(|_| AppError::not_found("no avatar"))?;
    let mime = crate::api::sniff_avatar(&data)
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

#[derive(Deserialize)]
pub struct InvQuery {
    pub(super) limit: Option<i64>,
}

/// 子调用成功帧 / 历史里带的回复摘要。按 Unicode 标量截断，和库里的 left() 对齐。
pub(super) const RESULT_PREVIEW_CHARS: usize = 2000;

pub(super) fn result_preview_text(parent: Option<Uuid>, status: &str, text: &str) -> Option<String> {
    if parent.is_none() || status != "succeeded" {
        return None;
    }
    Some(text.chars().take(RESULT_PREVIEW_CHARS).collect())
}

/// GET /conversations/{id}/agent-invocations：刷新后找回「跑完了吗」。
/// 仅成员可读（专属会话成员=主人）；按 created_at DESC、limit 默认 50。
/// 同时返回 root_conversation_id 等于该会话的子调用，刷新后能把嵌套卡片补回来。
pub async fn list_invocations(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
    Query(q): Query<InvQuery>,
) -> AppResult<Json<Vec<InvocationView>>> {
    let uid = uid(&headers, &st).await?;
    domain::membership(&st.pool, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    let mut rows = sqlx::query_as::<_, InvocationView>(
        "SELECT i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                i.status, i.error, i.attachment_ids, i.created_at, i.started_at, i.finished_at,
                i.parent_invocation_id, i.root_conversation_id, a.name AS agent_name,
                CASE
                  WHEN i.parent_invocation_id IS NOT NULL AND i.status = 'succeeded'
                  THEN left(i.result_text, 2000)
                  ELSE NULL
                END AS result_preview
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.conversation_id = $1 OR i.root_conversation_id = $1
          ORDER BY i.created_at DESC, i.id DESC LIMIT $2",
    )
    .bind(conv)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&st.pool)
    .await?;
    // A03：running 调用的 current_step 只存内存，历史读取时回填
    for inv in &mut rows {
        if inv.finished_at.is_none() {
            inv.current_step = st
                .inv_runs
                .get(&inv.id)
                .and_then(|r| r.open.last().map(|(_, t, _)| t.clone()));
        }
    }
    Ok(Json(rows))
}

/// POST /conversations/{id}/agent-context/reset（专属会话，或请求者自己的 Agent 在私聊 / 群里的上下文）
/// 把对应上下文的运行时会话清掉，代次加一。仅成员；非 agent 会话 400；没有 Agent 也 204。
pub async fn reset_context(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(conv): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid(&headers, &st).await?;
    domain::membership(&st.pool, conv, uid)
        .await?
        .ok_or_else(|| AppError::forbidden("not a member of this conversation"))?;
    let row = sqlx::query_as::<_, (String, Option<Uuid>)>(
        "SELECT type, agent_id FROM conversations WHERE id = $1",
    )
    .bind(conv)
    .fetch_optional(&st.pool)
    .await?;
    let Some((ty, agent_id)) = row else {
        return Err(AppError::bad_request("not an agent conversation"));
    };
    match (ty.as_str(), agent_id) {
        // 专属会话：只动它那个 Agent、键为本会话的上下文。schedule:<id> 不在这里。
        ("agent", Some(agent_id)) => {
            clear_context(&st.pool, agent_id, &conv.to_string()).await?;
        }
        // 私聊 / 群：只清请求者自己的 Agent 在本会话的上下文，别人的 Agent 不受影响
        ("dm" | "group", _) => {
            clear_own_contexts(&st.pool, uid, &conv.to_string()).await?;
        }
        _ => return Err(AppError::bad_request("not an agent conversation")),
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "timed_out")
}

/// POST /v1/agent-invocations/{id}/cancel
/// 只有该调用所属 Agent 的主人能取消；别人或不存在一律 404。
/// queued / running / awaiting_approval → cancelled。
/// 只要取消成功就通知宿主，不看更新前读到的状态：排队转运行可能已经把 start 发出去了。
/// 宿主离线时这一帧送不出去，重连核对时再补。已是终态 → 409 already_finished。
pub async fn cancel_invocation(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<InvocationView>> {
    let uid = uid(&headers, &st).await?;
    let row = sqlx::query_as::<_, (String, Uuid, Uuid, Uuid)>(
        "SELECT i.status, a.owner_id, a.host_id, i.conversation_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some((status, owner, host_id, conv)) = row else {
        return Err(AppError::not_found("invocation not found"));
    };
    if owner != uid {
        return Err(AppError::not_found("invocation not found"));
    }
    if terminal_status(&status) {
        return Err(AppError::conflict("already_finished"));
    }
    let inv = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'cancelled', error = NULL, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
            AND status IN ('queued', 'running', 'awaiting_approval')
         {INV_RETURNING}"
    ))
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let Some(inv) = inv else {
        return Err(AppError::conflict("already_finished"));
    };
    // 更新前的 status 只用来拒绝已经结束的；通知与否看这次 UPDATE 是否成功。
    notify_host_cancel(&st, host_id, id).await;
    let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
    let members = with_root_members(&st, &members, &inv).await;
    push_invocation(&st, &members, &inv);
    crate::approvals::expire_invocation(&st, id).await;
    // 未结束的后代一并取消（排队的也算）
    cascade_cancel(&st, &[id], None).await;
    Ok(Json(inv))
}
