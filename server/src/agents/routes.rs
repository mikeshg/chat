use super::*;

// ---------- REST ----------

pub async fn list_hosts(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentHostView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, label, platform, last_seen_at FROM agent_hosts
          WHERE user_id = $1 ORDER BY created_at, id",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, label, platform, last_seen_at)| AgentHostView {
                id,
                label,
                platform,
                online: st.host_online(id),
                last_seen_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct PatchHostReq {
    pub(super) label: String,
}

/// 主人给自己的宿主改名。去首尾空白，1–60 字；别人的或不存在的宿主 404。
/// 成功后给主人的普通连接推 agent_host_status（带新 label）。
pub async fn patch_host(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchHostReq>,
) -> AppResult<Json<AgentHostView>> {
    let uid = uid(&headers, &st).await?;
    let label = req.label.trim();
    let n = label.chars().count();
    if !(1..=60).contains(&n) {
        return Err(AppError::bad_request("bad_label"));
    }
    let label = label.to_string();
    let row = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "UPDATE agent_hosts SET label = $1
          WHERE id = $2 AND user_id = $3
          RETURNING id, label, platform, last_seen_at",
    )
    .bind(&label)
    .bind(id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?;
    let Some((id, label, platform, last_seen_at)) = row else {
        return Err(AppError::not_found("host not found"));
    };
    let online = st.host_online(id);
    let payload = serde_json::to_string(&WsOut::AgentHostStatus {
        host_id: id,
        online,
        label: Some(label.clone()),
    })
    .unwrap();
    st.send_to_user(uid, &payload);
    Ok(Json(AgentHostView {
        id,
        label,
        platform,
        online,
        last_seen_at,
    }))
}

/// AgentView 的查询列（list / patch 共用）
pub(super) const AGENT_SELECT: &str = "SELECT a.id, a.name, a.host_id, a.runtime, a.cwd, a.created_at,
        (SELECT c.id FROM conversations c
          WHERE c.agent_id = a.id AND c.type = 'agent' AND c.workspace_id IS NULL) AS conversation_id,
        a.output_mode, a.queue_timeout_secs,
        a.extra_dirs, a.model, a.mode, a.instructions, a.env,
        a.avatar_version, a.reported_options,
        (a.deleted_at IS NOT NULL) AS deleted,
        a.permission_tier
   FROM agents a";

#[derive(sqlx::FromRow)]
pub(super) struct AgentDbRow {
    pub(super) id: Uuid,
    pub(super) name: String,
    pub(super) host_id: Uuid,
    pub(super) runtime: Value,
    pub(super) cwd: String,
    pub(super) created_at: DateTime<Utc>,
    pub(super) conversation_id: Option<Uuid>,
    pub(super) output_mode: Option<String>,
    pub(super) queue_timeout_secs: Option<i32>,
    pub(super) extra_dirs: Vec<String>,
    pub(super) model: String,
    pub(super) mode: String,
    pub(super) instructions: String,
    pub(super) env: Value,
    pub(super) avatar_version: i32,
    pub(super) reported_options: Value,
    pub(super) deleted: bool,
    pub(super) permission_tier: String,
}

pub(super) fn agent_view(st: &SharedState, r: AgentDbRow) -> AgentView {
    AgentView {
        id: r.id,
        name: r.name,
        host_id: r.host_id,
        host_online: st.host_online(r.host_id),
        runtime: r.runtime,
        cwd: r.cwd,
        conversation_id: r.conversation_id.unwrap_or_default(),
        created_at: r.created_at,
        output_mode: r.output_mode,
        queue_timeout_secs: r.queue_timeout_secs,
        extra_dirs: r.extra_dirs,
        model: r.model,
        mode: r.mode,
        instructions: r.instructions,
        env: r.env,
        reported_options: r.reported_options,
        avatar_version: r.avatar_version,
        deleted: r.deleted,
        permission_tier: r.permission_tier,
    }
}

pub async fn list_agents(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<AgentView>>> {
    let uid = uid(&headers, &st).await?;
    let rows = sqlx::query_as::<_, AgentDbRow>(&format!(
        "{AGENT_SELECT} WHERE a.owner_id = $1 AND a.deleted_at IS NULL ORDER BY a.created_at, a.id"
    ))
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.into_iter().map(|r| agent_view(&st, r)).collect()))
}

/// Agent 名规则：1–32 字符，只允许字母/数字/`_`/`-`，首字符不能是 `-`。
/// 与 DB CHECK 同规则；REST 先拦一遍给出稳定的 bad_name。
pub(super) fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=32).contains(&n)
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub async fn create_agent(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CreateAgentReq>,
) -> AppResult<Json<AgentView>> {
    let ctx = auth::require_auth(&st, &headers).await?;
    let uid = ctx.user.id;
    if !crate::agent_access::user_allows(&st.pool, uid).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    if !valid_name(&req.name) {
        return Err(AppError::bad_request("bad_name"));
    }
    let (runtime, cwd) = normalize_runtime(&req.runtime, &req.cwd)?;
    let permission_tier = normalize_tier(req.permission_tier.as_deref())?;
    let model = check_token(&req.model, "bad_model")?;
    let mode = check_token(&req.mode, "bad_mode")?;

    let mut tx = st.pool.begin().await?;
    // 宿主必须属于当前用户；否则 404（不暴露他人资源存在性）
    let host_user =
        sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1 FOR UPDATE")
            .bind(req.host_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found("host not found"))?;
    if host_user != uid {
        return Err(AppError::not_found("host not found"));
    }
    // 只能加到「这次登录」正在跑桌面端的那台电脑上。编辑等其他接口不走这里。
    if !st.host_is_local_session(req.host_id, ctx.sid) {
        return Err(AppError::forbidden("host_not_local"));
    }

    let agent_id = match sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agents (owner_id, host_id, name, runtime, cwd, permission_tier, model, mode)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(uid)
    .bind(req.host_id)
    .bind(&req.name)
    .bind(&runtime)
    .bind(&cwd)
    .bind(&permission_tier)
    .bind(&model)
    .bind(&mode)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        Err(sqlx::Error::Database(db)) if db.constraint() == Some("uq_agents_owner_name") => {
            return Err(AppError::conflict("name_taken"));
        }
        Err(e) => return Err(e.into()),
    };
    // 专属会话：type='agent'、owner=主人、唯一成员、name 恒 NULL、agent_id 绑定
    let conv_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO conversations (type, owner_id, agent_id) VALUES ('agent', $1, $2)
         RETURNING id",
    )
    .bind(uid)
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO memberships (conversation_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(conv_id)
    .bind(uid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // 主人的普通连接刷新会话列表（宿主连接不订阅业务帧）
    crate::api::notify(&st, &[uid], "conversations");
    Ok(Json(AgentView {
        id: agent_id,
        name: req.name,
        host_id: req.host_id,
        host_online: st.host_online(req.host_id),
        runtime,
        cwd,
        conversation_id: conv_id,
        created_at: Utc::now(),
        output_mode: None,
        queue_timeout_secs: None,
        extra_dirs: Vec::new(),
        model,
        mode,
        instructions: String::new(),
        env: json!({}),
        reported_options: json!({}),
        avatar_version: 0,
        deleted: false,
        permission_tier,
    }))
}

/// command / args / cwd 与登记表 id / version 的格式校验。登记表不联网。
pub(super) fn normalize_runtime(rt: &AgentRuntimeReq, cwd: &str) -> AppResult<(Value, String)> {
    let cwd = cwd.trim().to_string();
    if cwd.chars().count() > 500 {
        return Err(AppError::bad_request("bad_runtime"));
    }
    let value = match rt {
        AgentRuntimeReq::Registry { registry } => {
            let id = check_pin(&registry.id, 128)?;
            let version = check_pin(&registry.version, 64)?;
            json!({"registry": {"id": id, "version": version}})
        }
        AgentRuntimeReq::Command { command, args } => {
            let command = command.trim();
            if command.is_empty() || command.chars().count() > 500 {
                return Err(AppError::bad_request("bad_runtime"));
            }
            if args.len() > 50 || args.iter().any(|a| a.chars().count() > 500) {
                return Err(AppError::bad_request("bad_runtime"));
            }
            json!({"command": command, "args": args})
        }
        // A28：内置 agent；builtin 只允许 true
        AgentRuntimeReq::Builtin { builtin } => {
            if !builtin {
                return Err(AppError::bad_request("bad_runtime"));
            }
            json!({"builtin": true})
        }
    };
    Ok((value, cwd))
}

/// 登记表 id / version：非空、不带路径，避免安装目录被写成别的地方。
pub(super) fn check_pin(s: &str, max: usize) -> AppResult<String> {
    let s = s.trim();
    if s.is_empty()
        || s.chars().count() > max
        || s.chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\' || c == '\0')
        || s.contains("..")
    {
        return Err(AppError::bad_request("bad_runtime"));
    }
    Ok(s.to_string())
}

pub(super) fn valid_tier(tier: &str) -> bool {
    matches!(tier, "read_only" | "workspace" | "full")
}

/// 档位从低到高：read_only < workspace < full
pub(super) fn tier_rank(tier: &str) -> u8 {
    match tier {
        "full" => 2,
        "workspace" => 1,
        _ => 0,
    }
}

/// patch_agent 锁行时读出的现值：归属 + 决定宿主执行什么的字段
#[derive(sqlx::FromRow)]
pub(super) struct ExecRow {
    pub(super) owner_id: Uuid,
    pub(super) deleted_at: Option<DateTime<Utc>>,
    pub(super) host_id: Uuid,
    pub(super) runtime: Value,
    pub(super) cwd: String,
    pub(super) extra_dirs: Vec<String>,
    pub(super) env: Value,
    pub(super) permission_tier: String,
}

/// 缺省 = 只读。非法值 400。
pub(super) fn normalize_tier(tier: Option<&str>) -> AppResult<String> {
    match tier.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("read_only".into()),
        Some(t) if valid_tier(t) => Ok(t.to_string()),
        Some(_) => Err(AppError::bad_request("bad_permission_tier")),
    }
}

pub(super) fn check_instructions(s: &str) -> AppResult<()> {
    if s.chars().count() > 8000 {
        return Err(AppError::bad_request("bad_instructions"));
    }
    Ok(())
}

/// 空串 = 用 agent 默认。过长拒绝。
pub(crate) fn check_token(s: &str, code: &'static str) -> AppResult<String> {
    let t = s.trim();
    if t.chars().count() > 200 {
        return Err(AppError::bad_request(code));
    }
    Ok(t.to_string())
}

/// 去掉空行后 ≤ 10 个，每个 ≤ 500 字符。
pub(super) fn check_extra_dirs(dirs: &[String]) -> AppResult<Vec<String>> {
    let dirs: Vec<String> = dirs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if dirs.len() > 10 || dirs.iter().any(|s| s.chars().count() > 500) {
        return Err(AppError::bad_request("bad_extra_dirs"));
    }
    Ok(dirs)
}

/// 字符串到字符串，≤ 50 项，键 ≤ 100、值 ≤ 4000。空键拒绝。
pub(super) fn check_env(v: &Value) -> AppResult<Value> {
    let obj = v
        .as_object()
        .ok_or_else(|| AppError::bad_request("bad_env"))?;
    if obj.len() > 50 {
        return Err(AppError::bad_request("bad_env"));
    }
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        if k.is_empty() || k.chars().count() > 100 {
            return Err(AppError::bad_request("bad_env"));
        }
        let s = val
            .as_str()
            .ok_or_else(|| AppError::bad_request("bad_env"))?;
        if s.chars().count() > 4000 {
            return Err(AppError::bad_request("bad_env"));
        }
        out.insert(k.clone(), Value::String(s.to_string()));
    }
    Ok(Value::Object(out))
}
