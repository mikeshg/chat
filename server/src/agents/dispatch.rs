use super::*;

// ---------- 调用派发（挂在 broadcast_posted 上） ----------

#[derive(Clone)]
pub(super) struct AgentRow {
    pub(super) id: Uuid,
    pub(super) owner: Uuid,
    pub(super) host_id: Uuid,
    pub(super) name: String,
    pub(super) runtime: Value,
    pub(super) cwd: String,
    pub(super) extra_dirs: Vec<String>,
    pub(super) model: String,
    pub(super) mode: String,
    pub(super) instructions: String,
    pub(super) env: Value,
    pub(super) permission_tier: String,
    pub(super) deleted_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
pub(super) struct AgentCfgRow {
    pub(super) id: Uuid,
    pub(super) owner_id: Option<Uuid>,
    pub(super) host_id: Uuid,
    pub(super) name: String,
    pub(super) runtime: Value,
    pub(super) cwd: String,
    pub(super) extra_dirs: Vec<String>,
    pub(super) model: String,
    pub(super) mode: String,
    pub(super) instructions: String,
    pub(super) env: Value,
    pub(super) permission_tier: String,
    pub(super) deleted_at: Option<DateTime<Utc>>,
}

pub(super) fn row_to_agent(r: AgentCfgRow) -> AgentRow {
    AgentRow {
        id: r.id,
        owner: r.owner_id.unwrap_or_default(),
        host_id: r.host_id,
        name: r.name,
        runtime: r.runtime,
        cwd: r.cwd,
        extra_dirs: r.extra_dirs,
        model: r.model,
        mode: r.mode,
        instructions: r.instructions,
        env: r.env,
        permission_tier: r.permission_tier,
        deleted_at: r.deleted_at,
    }
}

pub(super) const AGENT_CFG_SQL: &str = "SELECT a.id, a.owner_id, a.host_id, a.name, a.runtime, a.cwd,
        a.extra_dirs, a.model, a.mode, a.instructions, a.env, a.permission_tier, a.deleted_at";

pub(super) async fn load_cfg(pool: &PgPool, id: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, AgentCfgRow>(&format!("{AGENT_CFG_SQL} FROM agents a WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|r| r.map(row_to_agent))
}

/// 一条语句读出运行时会话和代次。没有行 = 新会话、代次 0。空串会话也当没有。
/// 查询失败同样当新会话，避免把读失败变成「续上一个没拿到的旧会话」。
pub(super) async fn read_context_epoch(
    pool: &PgPool,
    agent_id: Uuid,
    context_key: &str,
) -> (Option<String>, i64) {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT runtime_session_id, epoch FROM agent_contexts
          WHERE agent_id = $1 AND context_key = $2",
    )
    .bind(agent_id)
    .bind(context_key)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some((sid, epoch))) => {
            let session = if sid.is_empty() { None } else { Some(sid) };
            (session, epoch)
        }
        Ok(None) => (None, 0),
        Err(e) => {
            tracing::error!(error = %e, "读取 Agent 上下文失败");
            (None, 0)
        }
    }
}

/// 下发 invocation_start 之前把这次读到的代次记在调用行上。完成时只认这一列。
pub(super) async fn stamp_context_epoch(pool: &PgPool, inv_id: Uuid, epoch: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE agent_invocations SET context_epoch = $2 WHERE id = $1")
        .bind(inv_id)
        .bind(epoch)
        .execute(pool)
        .await?;
    Ok(())
}

/// 开新话题：没有行就插入空会话、代次 1；有行则清空会话并把代次加一。
pub(super) async fn clear_context(pool: &PgPool, agent_id: Uuid, context_key: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO agent_contexts (agent_id, context_key, runtime_session_id, epoch, updated_at)
         VALUES ($1, $2, '', 1, now())
         ON CONFLICT (agent_id, context_key)
         DO UPDATE SET runtime_session_id = '',
                       epoch = agent_contexts.epoch + 1,
                       updated_at = now()",
    )
    .bind(agent_id)
    .bind(context_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// 私聊 / 群：请求者名下每个 Agent 各清一条，同一条语句完成。
pub(super) async fn clear_own_contexts(pool: &PgPool, owner: Uuid, context_key: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO agent_contexts (agent_id, context_key, runtime_session_id, epoch, updated_at)
         SELECT id, $2, '', 1, now() FROM agents WHERE owner_id = $1
         ON CONFLICT (agent_id, context_key)
         DO UPDATE SET runtime_session_id = '',
                       epoch = agent_contexts.epoch + 1,
                       updated_at = now()",
    )
    .bind(owner)
    .bind(context_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// 成功的 finish 才写回。代次来自调用行（不是内存）。
/// NULL 是迁移前派发的老调用，照常覆盖。有代次则只在上下文代次仍相等时覆盖，且不改代次本身。
pub(super) async fn persist_runtime_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: Uuid,
    context_key: &str,
    session_id: &str,
    context_epoch: Option<i64>,
) -> Result<(), sqlx::Error> {
    match context_epoch {
        Some(epoch) => {
            sqlx::query(
                "INSERT INTO agent_contexts
                     (agent_id, context_key, runtime_session_id, epoch, updated_at)
                 VALUES ($1, $2, $3, $4, now())
                 ON CONFLICT (agent_id, context_key)
                 DO UPDATE SET runtime_session_id = EXCLUDED.runtime_session_id,
                               updated_at = now()
                 WHERE agent_contexts.epoch = $4",
            )
            .bind(agent_id)
            .bind(context_key)
            .bind(session_id)
            .bind(epoch)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO agent_contexts
                     (agent_id, context_key, runtime_session_id, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (agent_id, context_key)
                 DO UPDATE SET runtime_session_id = EXCLUDED.runtime_session_id,
                               updated_at = now()",
            )
            .bind(agent_id)
            .bind(context_key)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn agent_of_conv(pool: &PgPool, conv: Uuid) -> Result<Option<AgentRow>, sqlx::Error> {
    sqlx::query_as::<_, AgentCfgRow>(&format!(
        "{AGENT_CFG_SQL}
           FROM conversations c JOIN agents a ON a.id = c.agent_id
          WHERE c.id = $1 AND c.type = 'agent'"
    ))
    .bind(conv)
    .fetch_optional(pool)
    .await
    .map(|r| r.map(row_to_agent))
}

/// invocation_start / agent_probe 共用的配置快照。改配置在下一次调用生效。
/// A28：内置 Agent 追加 builtin.endpoints（每次调用从 server_settings 现读，含 api_key）。
pub(super) async fn agent_snapshot(pool: &PgPool, a: &AgentRow) -> Value {
    let mut v = json!({
        "id": a.id,
        "name": a.name,
        "runtime": a.runtime,
        "cwd": a.cwd,
        "extra_dirs": a.extra_dirs,
        "model": a.model,
        "mode": a.mode,
        "instructions": a.instructions,
        "env": a.env,
        "permission_tier": a.permission_tier,
    });
    if a.runtime["builtin"].as_bool() == Some(true) {
        let endpoints = crate::settings::load_builtin_endpoints(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = ?e, "load builtin_endpoints failed");
                Vec::new()
            });
        v["builtin"] = json!({"endpoints": endpoints});
    }
    v
}

/// 新文字消息落库后的唯一挂点。
/// 专属会话：主人发的普通文字调用它的 Agent（prompt 用全文）。
/// 私聊 / 群：显式 agent_targets（属于发送者且未删除）∪ 手打 @token
/// （先匹配发送者自己的 Agent，不区分大小写）。别人的 id 与非主人的同名都不触发。
/// Agent 自己的回写与幂等重放不触发。
pub async fn maybe_invoke(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    if !posted.inserted || posted.message.agent.is_some() || posted.message.kind != "text" {
        return;
    }
    match agent_of_conv(&st.pool, conv).await {
        Ok(Some(agent)) => {
            // 只有主人本人的消息触发；专属会话唯一成员即主人，显式校验防结构漂移
            if agent.deleted_at.is_some() || posted.message.sender_id != agent.owner {
                return;
            }
            dispatch(
                st,
                agent,
                conv,
                posted.message.id,
                posted.message.content.clone(),
                &posted.member_ids,
            )
            .await;
        }
        Ok(None) => {
            invoke_in_place(st, conv, posted).await;
        }
        Err(e) => tracing::error!(error = %e, "读取会话 Agent 失败"),
    }
}

/// 私聊 / 群：为每个解析出的 Agent 建一次调用。prompt 去掉用来点名的 @token。
pub(super) async fn invoke_in_place(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    let kind: Option<String> = sqlx::query_scalar("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
    if kind.as_deref() != Some("dm") && kind.as_deref() != Some("group") {
        return;
    }
    let mine = sender_agents(&st.pool, posted.message.sender_id).await;
    let targets = resolve_agent_targets(&mine, &posted.message.content, &posted.agent_targets);
    if targets.is_empty() {
        return;
    }
    let names: Vec<String> = targets.iter().map(|a| a.name.clone()).collect();
    let prompt = prompt_without_agent_tokens(&posted.message.content, &names);
    for agent in targets {
        dispatch(
            st,
            agent,
            conv,
            posted.message.id,
            prompt.clone(),
            &posted.member_ids,
        )
        .await;
    }
}

pub(super) async fn sender_agents(pool: &PgPool, owner: Uuid) -> Vec<AgentRow> {
    sqlx::query_as::<_, AgentCfgRow>(&format!(
        "{AGENT_CFG_SQL}
           FROM agents a
          WHERE a.owner_id = $1 AND a.deleted_at IS NULL
          ORDER BY a.created_at, a.id"
    ))
    .bind(owner)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(row_to_agent)
    .collect()
}

/// 显式 id 在前，手打且尚未指定的 @token 在后。同 id 只留一次。
pub(super) fn resolve_agent_targets(mine: &[AgentRow], content: &str, explicit: &[Uuid]) -> Vec<AgentRow> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in explicit {
        if let Some(agent) = mine.iter().find(|a| a.id == *id) {
            if seen.insert(agent.id) {
                out.push(agent.clone());
            }
        }
    }
    let taken: std::collections::HashSet<String> =
        out.iter().map(|a| a.name.to_ascii_lowercase()).collect();
    for tok in domain::scan_at_tokens(content) {
        if taken.contains(&tok.name.to_ascii_lowercase()) {
            continue;
        }
        if let Some(agent) = mine.iter().find(|a| a.name.eq_ignore_ascii_case(&tok.name)) {
            if seen.insert(agent.id) {
                out.push(agent.clone());
            }
        }
    }
    out
}

/// 去掉点名用的 @Agent（及紧跟的一个空格）。剥完只剩空白时退回原文。
pub(super) fn prompt_without_agent_tokens(content: &str, names: &[String]) -> String {
    let tokens = domain::scan_at_tokens(content);
    let mut out = String::new();
    let mut cursor = 0;
    for tok in &tokens {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&tok.name)) {
            continue;
        }
        out.push_str(&content[cursor..tok.start]);
        let mut next = tok.end;
        if content.as_bytes().get(next) == Some(&b' ') {
            next += 1;
        }
        cursor = next;
    }
    out.push_str(&content[cursor..]);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        content.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(super) async fn dispatch(
    st: &SharedState,
    agent: AgentRow,
    conv: Uuid,
    trigger: Uuid,
    prompt: String,
    member_ids: &[Uuid],
) {
    // 专属会话在落库前就拒绝了。群 / 私聊里的 @ 走到这里时只是不建调用。
    match crate::agent_access::user_allows(&st.pool, agent.owner).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::error!(error = %e, "agent access check failed");
            return;
        }
    }
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    // A05：待交付附件的收集与调用入库放进同一事务、同一把会话行锁——
    // 并发触发的两条文字消息各自派发时，附件按 seq 唯一地分给其中一次。
    let inv: Result<InvocationView, sqlx::Error> = async {
        let mut tx = st.pool.begin().await?;
        let _ = domain::lock_conversation(&mut tx, conv).await?;
        let atts = collect_pending_attachments(&mut tx, conv, agent.owner, trigger).await?;
        let inv = if online {
            insert_running(
                &mut *tx,
                &agent,
                conv,
                Some(trigger),
                None,
                &atts,
                &InvLink::default(),
            )
            .await?
        } else {
            insert_queued(
                &mut *tx,
                &agent,
                conv,
                Some(trigger),
                None,
                timeout,
                &atts,
                &InvLink::default(),
            )
            .await?
        };
        tx.commit().await?;
        Ok(inv)
    }
    .await;
    // 在线先标 running 再下发，客户端看到的第一帧就是 running（不是先闪一下排队）。
    // 发不出去则改回 queued，与离线同语义。
    let Ok(inv) = inv else {
        tracing::error!("agent_invocation insert failed");
        return;
    };
    let key = conv.to_string();
    deliver_invocation(st, &agent, inv, &prompt, &key, member_ids).await;
}

/// 定时调用到点后的派发。context_key = schedule:<id>，没有触发消息。
pub(crate) struct ScheduleFire {
    pub schedule_id: Uuid,
    pub agent_id: Uuid,
    pub conversation_id: Uuid,
    pub prompt: String,
}

/// 定时调用：在推进计划的同一事务里建一条排队调用（指令存进调用行，计划之后被改被删都不影响）。
/// Agent 已删除时不建。
pub(crate) async fn insert_schedule_invocation(
    st: &SharedState,
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    job: &ScheduleFire,
) -> Result<Option<(Uuid, InvocationView)>, sqlx::Error> {
    let Some(agent) = load_cfg(&st.pool, job.agent_id)
        .await?
        .filter(|a| a.deleted_at.is_none())
    else {
        tracing::info!(schedule_id = %job.schedule_id, "定时调用跳过：Agent 已删除");
        return Ok(None);
    };
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let link = InvLink {
        prompt: Some(job.prompt.clone()),
        ..InvLink::default()
    };
    let inv = insert_queued(
        &mut **tx,
        &agent,
        job.conversation_id,
        None,
        Some(job.schedule_id),
        timeout,
        &[],
        &link,
    )
    .await?;
    Ok(Some((agent.host_id, inv)))
}

/// 提交后派发：宿主在线就走排队派发（和宿主上线清排队同一条路），否则只把排队状态推给成员。
pub(crate) async fn dispatch_schedule_invocation(
    st: &SharedState,
    host_id: Uuid,
    inv: &InvocationView,
) {
    if st.host_online(host_id) {
        flush_queued(st, host_id).await;
        return;
    }
    let members = domain::member_ids(&st.pool, inv.conversation_id)
        .await
        .unwrap_or_default();
    let members = with_root_members(st, &members, inv).await;
    push_invocation(st, &members, inv);
}

pub(super) async fn deliver_invocation(
    st: &SharedState,
    agent: &AgentRow,
    inv: InvocationView,
    prompt: &str,
    context_key: &str,
    member_ids: &[Uuid],
) {
    // 子调用的状态帧和流式帧都要带到根会话成员（被调方专属会话之外的人）
    let member_ids = with_root_members(st, member_ids, &inv).await;
    push_invocation(st, &member_ids, &inv);
    if inv.status != "running" {
        return;
    }
    // 一条 SELECT 读会话和代次，先写进调用行，再下发 invocation_start。
    let (session_id, epoch) = read_context_epoch(&st.pool, agent.id, context_key).await;
    if let Err(e) = stamp_context_epoch(&st.pool, inv.id, epoch).await {
        tracing::error!(error = %e, "记下调用上下文代次失败");
    }
    register_run(st, agent, &inv, &member_ids).await;
    let payload = invocation_start_payload(st, &inv, agent, context_key, prompt, session_id).await;
    match send_start_if_current(st, agent.host_id, inv.id, &payload).await {
        StartSend::Sent => {}
        // 取消已经落库：队列里的运行时条目摘掉，不再推流式帧。
        StartSend::Dead => {}
        StartSend::Offline => {
            st.inv_runs.remove(&inv.id);
            let timeout = queue_timeout_secs(&st.pool, agent.id).await;
            if let Some(q) = requeue(&st.pool, inv.id, timeout).await {
                push_invocation(st, &member_ids, &q);
            }
        }
    }
}

pub(super) enum StartSend {
    Sent,
    /// 调用已经不是进行中（取消、超时等抢先）
    Dead,
    /// 还是 running，但宿主连接没接住这一帧
    Offline,
}

/// 和 notify_host_cancel 共用 inv_gate：要么先发出 start，要么发现已终态后不再发。
pub(super) async fn send_start_if_current(
    st: &SharedState,
    host_id: Uuid,
    id: Uuid,
    payload: &str,
) -> StartSend {
    let _gate = st.inv_gate.lock().await;
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM agent_invocations WHERE id = $1 AND finished_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&st.pool)
    .await
    .ok()
    .flatten();
    if !matches!(
        status.as_deref(),
        Some("running") | Some("awaiting_approval")
    ) {
        st.inv_runs.remove(&id);
        return StartSend::Dead;
    }
    if st.send_to_host(host_id, payload) {
        StartSend::Sent
    } else {
        StartSend::Offline
    }
}

/// 取消通知。调用方已经把行改成终态。锁内摘掉运行时条目再发帧，
/// 这样不会出现「cancel 已入队、start 随后又发出去」。
pub(super) async fn notify_host_cancel(st: &SharedState, host_id: Uuid, id: Uuid) {
    let _gate = st.inv_gate.lock().await;
    st.inv_runs.remove(&id);
    let payload = json!({
        "type": "invocation_cancel",
        "invocation_id": id,
    })
    .to_string();
    let _ = st.send_to_host(host_id, &payload);
}

/// A03：登记进行中调用的运行时状态（输出方式、进行中步骤、流式缓冲）。
/// 必须在 invocation_start 下发前就绪——宿主事件随时可能先回来。
pub(super) async fn register_run(
    st: &SharedState,
    agent: &AgentRow,
    inv: &InvocationView,
    member_ids: &[Uuid],
) {
    let mode = effective_settings(&st.pool, agent.id)
        .await
        .map(|s| s.output_mode)
        .unwrap_or_else(|_| "stream".into());
    st.inv_runs.insert(
        inv.id,
        InvRun {
            host_id: agent.host_id,
            conversation_id: inv.conversation_id,
            member_ids: member_ids.to_vec(),
            owner: agent.owner,
            agent_id: agent.id,
            inv: inv.clone(),
            mode,
            open: Vec::new(),
            steps: Vec::new(),
            stream_buf: String::new(),
            // 首个增量立即可推（窗口从派发前起算）
            last_push: Instant::now()
                .checked_sub(Duration::from_millis(STREAM_FLUSH_MS))
                .unwrap_or_else(Instant::now),
            flushing: false,
        },
    );
}

/// A05：收集主人的待交付 image/file 附件，上限 10。
/// 「待交付」= 触发消息之前落库、且未被本会话任何调用带走（attachment_ids 不含它）——
/// 超上限留下的附件自然留给下一次调用。在会话行锁内跑，并发触发各自拿到不重叠清单。
pub(super) async fn collect_pending_attachments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    conv: Uuid,
    owner: Uuid,
    trigger: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT a.id
           FROM messages m
           JOIN attachments a ON a.id = m.attachment_id
          WHERE m.conversation_id = $1
            AND m.sender_id = $2
            AND m.agent_id IS NULL
            AND m.kind IN ('image', 'file')
            AND m.deleted_at IS NULL
            AND m.seq < (SELECT seq FROM messages WHERE id = $3)
            AND NOT EXISTS (
                  SELECT 1 FROM agent_invocations i
                   WHERE i.conversation_id = $1
                     AND a.id = ANY (i.attachment_ids)
                )
          ORDER BY m.seq
          LIMIT 10",
    )
    .bind(conv)
    .bind(owner)
    .bind(trigger)
    .fetch_all(&mut **tx)
    .await
}

/// invocation_start.attachments：按 attachment_ids 的顺序还原元数据
/// （附件行中途被删的条目直接缺席，宿主按拿到的清单下载）。
pub(super) async fn attachment_meta(pool: &PgPool, ids: &[Uuid]) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    sqlx::query_as::<_, (Uuid, String, String, i64)>(
        "SELECT a.id, a.file_name, a.mime, a.size
           FROM unnest($1::uuid[]) WITH ORDINALITY u(id, ord)
           JOIN attachments a ON a.id = u.id
          ORDER BY u.ord",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, name, mime, size)| json!({"id": id, "name": name, "mime": mime, "size": size}))
    .collect()
}

pub(super) async fn queue_timeout_secs(pool: &PgPool, agent_id: Uuid) -> i32 {
    effective_settings(pool, agent_id)
        .await
        .map(|s| s.queue_timeout_secs)
        .unwrap_or(86400)
}

pub(crate) const INV_RETURNING: &str =
    "RETURNING id, agent_id, conversation_id, trigger_message_id, schedule_id, status, error,
                   attachment_ids, created_at, started_at, finished_at, parent_invocation_id,
                   root_conversation_id,
                   (SELECT name FROM agents WHERE id = agent_invocations.agent_id) AS agent_name";

/// 子调用额外落库的关联。主人直接发起 / 定时调用用默认值（depth 0，无父无根）。
pub(super) struct InvLink {
    pub(super) parent: Option<Uuid>,
    pub(super) depth: i32,
    pub(super) root: Option<Uuid>,
    /// `call:<上下文号>`。空则收尾时按会话或定时任务推导。
    pub(super) context_key: Option<String>,
    /// 离线补发用的完整指令。触发消息只是截断后的记录，不能当指令。
    pub(super) prompt: Option<String>,
}

impl Default for InvLink {
    fn default() -> Self {
        Self {
            parent: None,
            depth: 0,
            root: None,
            context_key: None,
            prompt: None,
        }
    }
}

pub(super) async fn insert_running(
    ex: impl sqlx::PgExecutor<'_>,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Option<Uuid>,
    schedule: Option<Uuid>,
    atts: &[Uuid],
    link: &InvLink,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, schedule_id, status, started_at,
             attachment_ids, parent_invocation_id, depth, root_conversation_id, context_key,
             prompt)
         VALUES ($1, $2, $3, $4, 'running', now(), $5, $6, $7, $8, $9, $10)
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(schedule)
    .bind(atts)
    .bind(link.parent)
    .bind(link.depth)
    .bind(link.root)
    .bind(&link.context_key)
    .bind(&link.prompt)
    .fetch_one(ex)
    .await
}

pub(super) async fn insert_queued(
    ex: impl sqlx::PgExecutor<'_>,
    agent: &AgentRow,
    conv: Uuid,
    trigger: Option<Uuid>,
    schedule: Option<Uuid>,
    timeout_secs: i32,
    atts: &[Uuid],
    link: &InvLink,
) -> Result<InvocationView, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "INSERT INTO agent_invocations
            (agent_id, conversation_id, trigger_message_id, schedule_id, status, queue_deadline,
             attachment_ids, parent_invocation_id, depth, root_conversation_id, context_key,
             prompt)
         VALUES ($1, $2, $3, $4, 'queued',
                 CASE WHEN $5 > 0 THEN now() + make_interval(secs => $5::double precision) END,
                 $6, $7, $8, $9, $10, $11)
         {INV_RETURNING}"
    ))
    .bind(agent.id)
    .bind(conv)
    .bind(trigger)
    .bind(schedule)
    .bind(timeout_secs)
    .bind(atts)
    .bind(link.parent)
    .bind(link.depth)
    .bind(link.root)
    .bind(&link.context_key)
    .bind(&link.prompt)
    .fetch_one(ex)
    .await
}

/// invocation_start 帧：Agent 配置快照（A06）+ 该会话记住的运行时会话（A02）
/// + 本次调用的工具凭据（A13，只在调用进行中有效）+ 待交付附件元数据（A05）。
/// 即时派发与宿主上线后清排队共用。
pub(super) async fn invocation_start_payload(
    st: &SharedState,
    inv: &InvocationView,
    agent: &AgentRow,
    context_key: &str,
    prompt: &str,
    session_id: Option<String>,
) -> String {
    // 深度以调用行为准（主人直接发起是 0）。签发失败时不带凭据，调用照常跑。
    let depth: i32 = sqlx::query_scalar("SELECT depth FROM agent_invocations WHERE id = $1")
        .bind(inv.id)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    let tools_token = crate::auth::mint_invocation(
        inv.id,
        agent.owner,
        agent.id,
        depth as i64,
        &st.cfg.jwt_secret,
    )
    .map_err(|e| tracing::error!(error = ?e, "tools_token mint failed"))
    .ok();
    // 根会话里有主人以外的人（群 / 私聊，或由它们派生的互调）：别人的消息进了上下文，
    // 宿主据此让读主人其他会话、建定时任务先走审批。查不到时按共享处理，宁可多问。
    let shared: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM agent_invocations i
               JOIN memberships m
                 ON m.conversation_id = COALESCE(i.root_conversation_id, i.conversation_id)
              WHERE i.id = $1 AND m.user_id <> $2)",
    )
    .bind(inv.id)
    .bind(agent.owner)
    .fetch_one(&st.pool)
    .await
    .unwrap_or(true);
    let attachments = attachment_meta(&st.pool, &inv.attachment_ids).await;
    let mut snapshot = agent_snapshot(&st.pool, agent).await;
    // A27：在 Agent 自己的对话里直接发的消息，用这条对话选的模型 / 模式，
    // 工作区对话再用工作区目录。定时、互调的上下文键不是会话 id，不走这里。
    if context_key == inv.conversation_id.to_string() {
        let conv = sqlx::query_as::<_, (Option<String>, String, String)>(
            "SELECT w.path, c.agent_model, c.agent_mode
               FROM conversations c
               LEFT JOIN agent_workspaces w ON w.id = c.workspace_id
              WHERE c.id = $1 AND c.agent_id = $2",
        )
        .bind(inv.conversation_id)
        .bind(agent.id)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
        if let Some((path, model, mode)) = conv {
            match path.as_deref() {
                // 无工作区：电脑自己放到程序目录下的 tmp/<对话 id>
                Some("") => snapshot["scratch"] = json!(inv.conversation_id),
                Some(path) => snapshot["cwd"] = json!(path),
                None => {}
            }
            if !model.is_empty() {
                snapshot["model"] = json!(model);
            }
            if !mode.is_empty() {
                snapshot["mode"] = json!(mode);
            }
        }
    }
    json!({
        "type": "invocation_start",
        "invocation_id": inv.id,
        "agent": snapshot,
        "prompt": prompt,
        "context": {"key": context_key, "session_id": session_id},
        "tools_token": tools_token,
        "attachments": attachments,
        "shared": shared,
    })
    .to_string()
}

/// 下发失败：从 running 改回 queued，重新计算截止时间。
pub(super) async fn requeue(pool: &PgPool, id: Uuid, timeout_secs: i32) -> Option<InvocationView> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'queued', error = NULL, started_at = NULL, finished_at = NULL,
                queue_deadline = CASE WHEN $2 > 0 THEN now() + make_interval(secs => $2::double precision) END
          WHERE id = $1 AND status = 'running' AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .bind(timeout_secs)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

#[derive(sqlx::FromRow)]
pub(super) struct QueuedStart {
    pub(super) id: Uuid,
    pub(super) conversation_id: Uuid,
    pub(super) agent_id: Uuid,
    pub(super) prompt: String,
    pub(super) trigger_message_id: Option<Uuid>,
    pub(super) schedule_id: Option<Uuid>,
    pub(super) context_key: Option<String>,
    pub(super) prompt_stored: bool,
}

/// 排队时 prompt 没落库。私聊 / 群上线补发时，按这条触发消息上的全部目标 Agent 再剥一次 @。
/// 专属会话保持原文，与在线派发一致。
pub(super) async fn prompt_for_flushed(pool: &PgPool, conv: Uuid, trigger: Uuid, content: &str) -> String {
    let kind: Option<String> = sqlx::query_scalar("SELECT type FROM conversations WHERE id = $1")
        .bind(conv)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    if kind.as_deref() == Some("agent") {
        return content.to_string();
    }
    let names = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT a.name
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE i.trigger_message_id = $1",
    )
    .bind(trigger)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    prompt_without_agent_tokens(content, &names)
}

/// host_ready 之后：该宿主上所有 queued 调用按 created_at 逐个改成 running 并下发。
/// 主人已没有使用权时整批跳过（进行中的应已取消，这里是兜底）。
pub async fn flush_queued(st: &SharedState, host_id: Uuid) {
    let owner = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
    if let Some(owner) = owner {
        if !crate::agent_access::user_allows(&st.pool, owner)
            .await
            .unwrap_or(false)
        {
            return;
        }
    }
    let rows = sqlx::query_as::<_, QueuedStart>(
        "SELECT i.id, i.conversation_id, a.id AS agent_id,
                COALESCE(i.prompt, m.content, s.prompt, '') AS prompt,
                i.trigger_message_id, i.schedule_id, i.context_key,
                (i.prompt IS NOT NULL) AS prompt_stored
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           LEFT JOIN messages m ON m.id = i.trigger_message_id
           LEFT JOIN agent_schedules s ON s.id = i.schedule_id
          WHERE a.host_id = $1 AND i.status = 'queued'
          ORDER BY i.created_at ASC, i.id ASC",
    )
    .bind(host_id)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for row in rows {
        let inv = match promote_queued(&st.pool, row.id).await {
            Ok(Some(inv)) => inv,
            Ok(None) => continue, // 清扫或取消抢先改了状态
            Err(e) => {
                tracing::warn!(error = %e, "排队调用转 running 失败");
                continue;
            }
        };
        let members = domain::member_ids(&st.pool, row.conversation_id)
            .await
            .unwrap_or_default();
        let agent = match load_cfg(&st.pool, row.agent_id).await {
            Ok(Some(a)) => a,
            _ => {
                let members = with_root_members(st, &members, &inv).await;
                push_invocation(st, &members, &inv);
                continue;
            }
        };
        // 定时任务被删掉之后，排队中的调用没有指令可发。
        // 子调用把完整指令存在 prompt 列，不走这条。
        if !row.prompt_stored
            && ((row.schedule_id.is_some() && row.prompt.is_empty())
                || (row.schedule_id.is_none() && row.trigger_message_id.is_none()))
        {
            let _ =
                finish_invocation(st, row.id, "failed", Some("schedule_deleted"), &members).await;
            continue;
        }
        let prompt = if row.prompt_stored || row.schedule_id.is_some() {
            row.prompt.clone()
        } else if let Some(trigger) = row.trigger_message_id {
            prompt_for_flushed(&st.pool, row.conversation_id, trigger, &row.prompt).await
        } else {
            String::new()
        };
        let context_key = row
            .context_key
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| match row.schedule_id {
                Some(id) => format!("schedule:{id}"),
                None => row.conversation_id.to_string(),
            });
        deliver_invocation(st, &agent, inv, &prompt, &context_key, &members).await;
    }
}

pub(super) async fn promote_queued(pool: &PgPool, id: Uuid) -> Result<Option<InvocationView>, sqlx::Error> {
    sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'running', started_at = now(), error = NULL, queue_deadline = NULL
          WHERE id = $1 AND status = 'queued' AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 后台清扫：间隔见 `AppState::queue_sweep_interval_ms`（默认 30s）。
/// 睡眠按 250ms 切片重读间隔，测试把原子量调小后最多一个切片就能生效。
pub fn spawn_queue_sweeper(st: SharedState) {
    tokio::spawn(async move {
        loop {
            wait_sweep_interval(&st).await;
            if let Err(e) = sweep_timed_out(&st).await {
                tracing::warn!(error = %e, "排队超时清扫失败");
            }
            if let Err(e) = sweep_host_grace(&st).await {
                tracing::warn!(error = %e, "宿主离线宽限清扫失败");
            }
        }
    });
}

pub(super) async fn wait_sweep_interval(st: &SharedState) {
    let started = Instant::now();
    loop {
        let ms = st.queue_sweep_interval_ms.load(Ordering::Relaxed).max(50);
        let target = Duration::from_millis(ms);
        if started.elapsed() >= target {
            return;
        }
        let left = target.saturating_sub(started.elapsed());
        tokio::time::sleep(left.min(Duration::from_millis(250))).await;
    }
}

pub(super) async fn sweep_timed_out(st: &SharedState) -> Result<(), sqlx::Error> {
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations
            SET status = 'timed_out', finished_at = now()
          WHERE status = 'queued'
            AND queue_deadline IS NOT NULL
            AND queue_deadline < now()
         {INV_RETURNING}"
    ))
    .fetch_all(&st.pool)
    .await?;
    for inv in invs {
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &inv).await;
        push_invocation(st, &members, &inv);
    }
    Ok(())
}

/// 宿主重连（host_ready 已入队之后）：
/// - 报告还在跑、库里也是进行中 → 保持；内存里没有运行时条目就重建；
/// - 没报告 → failed/host_disconnected，过期审批，推帧；
/// - 报告还在跑、库里已是终态 → 补发 invocation_cancel。
pub(crate) async fn on_host_ready(
    st: &SharedState,
    host_id: Uuid,
    reported: &[Uuid],
    approval_ids: &[String],
) {
    cancel_terminal_reported(st, host_id, reported).await;
    fail_unreported(st, host_id, reported).await;
    rebuild_reported(st, host_id, reported).await;
    crate::approvals::replay_decisions(st, host_id, approval_ids).await;
}

pub(super) async fn cancel_terminal_reported(st: &SharedState, host_id: Uuid, reported: &[Uuid]) {
    if reported.is_empty() {
        return;
    }
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT i.id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE a.host_id = $1
            AND i.id = ANY($2)
            AND i.status IN ('succeeded', 'failed', 'cancelled', 'timed_out')",
    )
    .bind(host_id)
    .bind(reported)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for id in ids {
        notify_host_cancel(st, host_id, id).await;
    }
}

pub(super) async fn fail_unreported(st: &SharedState, host_id: Uuid, reported: &[Uuid]) {
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations AS i
            SET status = 'failed', error = 'host_disconnected', finished_at = now()
           FROM agents a
          WHERE i.agent_id = a.id
            AND a.host_id = $1
            AND i.finished_at IS NULL
            AND i.status IN ('running', 'awaiting_approval')
            AND NOT (i.id = ANY($2))
         RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                   i.status, i.error, i.attachment_ids, i.created_at, i.started_at, i.finished_at,
                   i.parent_invocation_id, i.root_conversation_id,
                   (SELECT name FROM agents WHERE id = i.agent_id) AS agent_name"
    ))
    .bind(host_id)
    .bind(reported)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for inv in invs {
        st.inv_runs.remove(&inv.id);
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &inv).await;
        push_invocation(st, &members, &inv);
        crate::approvals::expire_invocation(st, inv.id).await;
    }
}

pub(super) async fn rebuild_reported(st: &SharedState, host_id: Uuid, reported: &[Uuid]) {
    if reported.is_empty() {
        return;
    }
    let invs = sqlx::query_as::<_, InvocationView>(
        "SELECT i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                i.status, i.error, i.attachment_ids, i.created_at, i.started_at, i.finished_at,
                i.parent_invocation_id, i.root_conversation_id,
                (SELECT name FROM agents WHERE id = i.agent_id) AS agent_name
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
          WHERE a.host_id = $1
            AND i.id = ANY($2)
            AND i.finished_at IS NULL
            AND i.status IN ('running', 'awaiting_approval')",
    )
    .bind(host_id)
    .bind(reported)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for inv in invs {
        if st.inv_runs.contains_key(&inv.id) {
            continue;
        }
        let Some(agent) = load_cfg(&st.pool, inv.agent_id).await.ok().flatten() else {
            continue;
        };
        if agent.host_id != host_id {
            continue;
        }
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &inv).await;
        register_run(st, &agent, &inv, &members).await;
    }
}

/// 宽限到期，或服务端重启后才发现宿主已经不在。起点只认 `offline_since`。
/// 在线宿主即使列里残留起点也不判失败，并把该列清掉（断开收尾和重连可能交错写上它）。
pub(super) async fn sweep_host_grace(st: &SharedState) -> Result<(), sqlx::Error> {
    stamp_unobserved_offline(st).await?;
    clear_online_offline_since(st).await?;
    let grace_ms = st.host_offline_grace_ms.load(Ordering::Relaxed).max(1);
    let grace_secs = grace_ms as f64 / 1000.0;
    let expired = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM agent_hosts
          WHERE offline_since IS NOT NULL
            AND offline_since < now() - make_interval(secs => $1::double precision)",
    )
    .bind(grace_secs)
    .fetch_all(&st.pool)
    .await?;
    let offline: Vec<Uuid> = expired
        .into_iter()
        .filter(|id| !st.host_online(*id))
        .collect();
    if offline.is_empty() {
        return Ok(());
    }
    let invs = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations AS i
            SET status = 'failed', error = 'host_disconnected', finished_at = now()
           FROM agents a
           JOIN agent_hosts h ON h.id = a.host_id
          WHERE i.agent_id = a.id
            AND a.host_id = ANY($2)
            AND i.finished_at IS NULL
            AND i.status IN ('running', 'awaiting_approval')
            AND h.offline_since IS NOT NULL
            AND h.offline_since < now() - make_interval(secs => $1::double precision)
         RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                   i.status, i.error, i.attachment_ids, i.created_at, i.started_at, i.finished_at,
                   i.parent_invocation_id, i.root_conversation_id,
                   (SELECT name FROM agents WHERE id = i.agent_id) AS agent_name"
    ))
    .bind(grace_secs)
    .bind(&offline)
    .fetch_all(&st.pool)
    .await?;
    for inv in invs {
        st.inv_runs.remove(&inv.id);
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &inv).await;
        push_invocation(st, &members, &inv);
        crate::approvals::expire_invocation(st, inv.id).await;
    }
    Ok(())
}

/// 在线却还带着离线起点：清掉，避免下一轮把进行中的调用判成断线失败。
pub(super) async fn clear_online_offline_since(st: &SharedState) -> Result<(), sqlx::Error> {
    let ids =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM agent_hosts WHERE offline_since IS NOT NULL")
            .fetch_all(&st.pool)
            .await?;
    for id in ids {
        if !st.host_online(id) {
            continue;
        }
        sqlx::query(
            "UPDATE agent_hosts SET offline_since = NULL
              WHERE id = $1 AND offline_since IS NOT NULL",
        )
        .bind(id)
        .execute(&st.pool)
        .await?;
    }
    Ok(())
}

/// 内存里没有这条宿主、库里却有进行中的调用、又没有离线起点：
/// 多半是进程重启前它还连着。从现在开始算宽限。
pub(super) async fn stamp_unobserved_offline(st: &SharedState) -> Result<(), sqlx::Error> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT a.host_id
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           JOIN agent_hosts h ON h.id = a.host_id
          WHERE i.finished_at IS NULL
            AND i.status IN ('running', 'awaiting_approval')
            AND h.offline_since IS NULL",
    )
    .fetch_all(&st.pool)
    .await?;
    for id in ids {
        if st.host_online(id) {
            continue;
        }
        sqlx::query(
            "UPDATE agent_hosts SET offline_since = now()
              WHERE id = $1 AND offline_since IS NULL",
        )
        .bind(id)
        .execute(&st.pool)
        .await?;
    }
    Ok(())
}

/// 状态推进 + 广播 agent_invocation 帧给会话成员。
/// 只从非终态推进（并发完成以先到者为准）。
pub async fn finish_invocation(
    st: &SharedState,
    id: Uuid,
    status: &str,
    error: Option<&str>,
    member_ids: &[Uuid],
) -> Result<Option<InvocationView>, sqlx::Error> {
    let inv = sqlx::query_as::<_, InvocationView>(&format!(
        "UPDATE agent_invocations SET status = $2, error = $3, finished_at = now()
          WHERE id = $1 AND finished_at IS NULL
         {INV_RETURNING}"
    ))
    .bind(id)
    .bind(status)
    .bind(error)
    .fetch_optional(&st.pool)
    .await?;
    if let Some(inv) = &inv {
        let members = with_root_members(st, member_ids, inv).await;
        push_invocation(st, &members, inv);
    }
    Ok(inv)
}

pub(super) fn push_invocation(st: &SharedState, member_ids: &[Uuid], inv: &InvocationView) {
    let p = serde_json::to_string(&WsOut::AgentInvocation {
        invocation: inv.clone(),
    })
    .unwrap();
    st.send_to_users(member_ids, &p);
}

/// 成员变更提交后重算进行中调用的接收者。
/// 命中调用会话或根会话是 `conv` 的运行；新名单是两侧当前成员的并集，
/// 所以人还留在另一侧会话时不会被拿掉。查询失败就保持原快照。
pub(crate) async fn refresh_run_members(st: &SharedState, conv: Uuid) {
    let ids: Vec<Uuid> = st
        .inv_runs
        .iter()
        .filter(|e| {
            let run = e.value();
            run.conversation_id == conv || run.inv.root_conversation_id == Some(conv)
        })
        .map(|e| *e.key())
        .collect();
    for id in ids {
        let Some(inv) = st.inv_runs.get(&id).map(|r| r.inv.clone()) else {
            continue;
        };
        if inv.conversation_id != conv && inv.root_conversation_id != Some(conv) {
            continue;
        }
        let Ok(mut members) = domain::member_ids(&st.pool, inv.conversation_id).await else {
            tracing::warn!(%conv, %id, "重算调用接收者时读成员失败");
            continue;
        };
        if let Some(root) = inv.root_conversation_id {
            if root != inv.conversation_id {
                let Ok(extra) = domain::member_ids(&st.pool, root).await else {
                    tracing::warn!(%conv, %id, "重算调用接收者时读根会话成员失败");
                    continue;
                };
                for uid in extra {
                    if !members.contains(&uid) {
                        members.push(uid);
                    }
                }
            }
        }
        if let Some(mut run) = st.inv_runs.get_mut(&id) {
            if run.conversation_id == inv.conversation_id {
                run.member_ids = members;
            }
        }
    }
}

/// 会话成员，再加上根会话里还没包括的人。根会话就是本会话时不重复查。
pub(crate) async fn with_root_members(
    st: &SharedState,
    member_ids: &[Uuid],
    inv: &InvocationView,
) -> Vec<Uuid> {
    let mut members = member_ids.to_vec();
    let Some(root) = inv.root_conversation_id else {
        return members;
    };
    if root == inv.conversation_id {
        return members;
    }
    if let Ok(extra) = domain::member_ids(&st.pool, root).await {
        for id in extra {
            if !members.contains(&id) {
                members.push(id);
            }
        }
    }
    members
}

/// 把 roots 的全部未结束后代改成 cancelled，推帧，并通知各自宿主。
/// roots 自己应已是终态（这里不再改它们）。
pub(crate) async fn cascade_cancel(st: &SharedState, roots: &[Uuid], error: Option<&str>) {
    if roots.is_empty() {
        return;
    }
    let rows = sqlx::query_as::<_, Cascaded>(
        "WITH RECURSIVE tree AS (
            SELECT id FROM agent_invocations
             WHERE parent_invocation_id = ANY($1) AND finished_at IS NULL
            UNION
            SELECT i.id FROM agent_invocations i
            JOIN tree t ON i.parent_invocation_id = t.id
             WHERE i.finished_at IS NULL
        )
        UPDATE agent_invocations AS i
           SET status = 'cancelled', error = $2, finished_at = now()
          FROM tree
         WHERE i.id = tree.id AND i.finished_at IS NULL
        RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                  i.status, i.error, i.created_at, i.started_at, i.finished_at,
                  i.parent_invocation_id, i.root_conversation_id,
                  (SELECT name FROM agents WHERE id = i.agent_id) AS agent_name,
                  (SELECT host_id FROM agents WHERE id = i.agent_id) AS host_id",
    )
    .bind(roots)
    .bind(error)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "级联取消子调用失败");
        Vec::new()
    });
    for row in rows {
        let id = row.id;
        let host_id = row.host_id;
        let conv = row.conversation_id;
        // 子调用可能刚被派发出去，读到的旧状态不能决定是否通知。
        notify_host_cancel(st, host_id, id).await;
        let members = domain::member_ids(&st.pool, conv).await.unwrap_or_default();
        let view = row.into_view();
        let members = with_root_members(st, &members, &view).await;
        push_invocation(st, &members, &view);
        crate::approvals::expire_invocation(st, id).await;
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct Cascaded {
    pub(super) id: Uuid,
    pub(super) agent_id: Uuid,
    pub(super) conversation_id: Uuid,
    pub(super) trigger_message_id: Option<Uuid>,
    pub(super) schedule_id: Option<Uuid>,
    pub(super) status: String,
    pub(super) error: Option<String>,
    pub(super) created_at: DateTime<Utc>,
    pub(super) started_at: Option<DateTime<Utc>>,
    pub(super) finished_at: Option<DateTime<Utc>>,
    pub(super) parent_invocation_id: Option<Uuid>,
    pub(super) root_conversation_id: Option<Uuid>,
    pub(super) agent_name: String,
    pub(super) host_id: Uuid,
}

impl Cascaded {
    fn into_view(self) -> InvocationView {
        InvocationView {
            id: self.id,
            agent_id: self.agent_id,
            conversation_id: self.conversation_id,
            trigger_message_id: self.trigger_message_id,
            schedule_id: self.schedule_id,
            attachment_ids: Vec::new(),
            status: self.status,
            error: self.error,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            current_step: None,
            parent_invocation_id: self.parent_invocation_id,
            root_conversation_id: self.root_conversation_id,
            agent_name: self.agent_name,
            result_preview: None,
        }
    }
}

/// 工具接口发起的子调用。找不到主人名下未删除的同名 Agent → not found。
pub(crate) struct ChildReq {
    pub owner: Uuid,
    pub parent_id: Uuid,
    pub parent_depth: i32,
    pub parent_conversation: Uuid,
    pub parent_root: Option<Uuid>,
    pub caller_name: String,
    pub agent_name: String,
    pub prompt: String,
    pub context_id: Option<Uuid>,
}

pub(crate) struct ChildRes {
    pub invocation_id: Uuid,
    pub context_id: Uuid,
    pub status: String,
}

pub(crate) async fn start_child_call(st: &SharedState, req: ChildReq) -> AppResult<ChildRes> {
    if req.prompt.is_empty() || req.prompt.chars().count() > 4000 {
        return Err(AppError::bad_request("bad_prompt"));
    }
    if !crate::agent_access::user_allows(&st.pool, req.owner).await? {
        return Err(AppError::forbidden("agent_access_denied"));
    }
    let found = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT a.id, c.id
           FROM agents a
           JOIN conversations c
             ON c.agent_id = a.id AND c.type = 'agent' AND c.workspace_id IS NULL
          WHERE a.owner_id = $1 AND a.deleted_at IS NULL AND lower(a.name) = lower($2)",
    )
    .bind(req.owner)
    .bind(&req.agent_name)
    .fetch_optional(&st.pool)
    .await?;
    let Some((callee_id, callee_conv)) = found else {
        return Err(AppError::not_found("agent not found"));
    };
    let max_depth: i32 =
        sqlx::query_scalar("SELECT max_call_depth FROM agent_user_settings WHERE user_id = $1")
            .bind(req.owner)
            .fetch_optional(&st.pool)
            .await?
            .unwrap_or(3);
    let depth = req.parent_depth.saturating_add(1);
    if depth > max_depth {
        return Err(AppError::bad_request("depth_exceeded"));
    }
    let agent = load_cfg(&st.pool, callee_id)
        .await?
        .filter(|a| a.deleted_at.is_none())
        .ok_or_else(|| AppError::not_found("agent not found"))?;
    let context_id = req.context_id.unwrap_or_else(Uuid::new_v4);
    let context_key = format!("call:{context_id}");
    let root = req.parent_root.unwrap_or(req.parent_conversation);
    let excerpt: String = req.prompt.chars().take(200).collect();
    let record = format!("📨 被 @{} 调用：{excerpt}", req.caller_name);
    let posted = crate::ws::post_and_broadcast(
        st,
        domain::PostMessage {
            agent_id: Some(agent.id),
            ..domain::PostMessage::text(callee_conv, req.owner, &record)
        },
        Vec::new(),
    )
    .await
    .map_err(|e| match e {
        PostError::Deny { message, .. } => AppError::bad_request(message),
        PostError::Db(err) => AppError::internal(err),
    })?;
    let trigger = posted.message.id;

    let link = InvLink {
        parent: Some(req.parent_id),
        depth,
        root: Some(root),
        context_key: Some(context_key.clone()),
        prompt: Some(req.prompt.clone()),
    };
    let timeout = queue_timeout_secs(&st.pool, agent.id).await;
    let online = st.host_online(agent.host_id);
    let inv = if online {
        insert_running(
            &st.pool,
            &agent,
            callee_conv,
            Some(trigger),
            None,
            &[],
            &link,
        )
        .await?
    } else {
        insert_queued(
            &st.pool,
            &agent,
            callee_conv,
            Some(trigger),
            None,
            timeout,
            &[],
            &link,
        )
        .await?
    };
    let members = domain::member_ids(&st.pool, callee_conv)
        .await
        .unwrap_or_default();
    let inv_id = inv.id;
    deliver_invocation(st, &agent, inv, &req.prompt, &context_key, &members).await;
    let status: String = sqlx::query_scalar("SELECT status FROM agent_invocations WHERE id = $1")
        .bind(inv_id)
        .fetch_one(&st.pool)
        .await?;
    Ok(ChildRes {
        invocation_id: inv_id,
        context_id,
        status,
    })
}
