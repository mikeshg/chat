use super::*;

// ---------- Agent A03：宿主事件流（invocation_event） ----------

/// stream 增量的最小推送间隔：同一次调用每 200ms 至多一帧 delta
pub(super) const STREAM_FLUSH_MS: u64 = 200;

/// 宿主回传的 invocation_event：只受理派给本宿主连接的进行中调用；
/// 事件类型不明/字段残缺一律忽略（不致命，不影响后续事件）。
pub async fn handle_event(st: &SharedState, host_id: Uuid, v: &serde_json::Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let ev = &v["event"];
    // steps 模式的步骤消息要进 DashMap 守卫外写库
    let mut step_msg: Option<(Uuid, Uuid, Uuid, String)> = None;
    {
        let Some(mut run) = st.inv_runs.get_mut(&inv_id) else {
            return; // 未知/已终结的调用：伪造或迟到的事件
        };
        if run.host_id != host_id {
            return;
        }
        match ev["type"].as_str() {
            Some("text") => {
                if let Some(t) = ev["text"].as_str() {
                    if !t.is_empty() && run.mode == "stream" {
                        run.stream_buf.push_str(t);
                        maybe_flush_stream(st, &mut run, inv_id);
                    }
                }
            }
            Some("step_start") => {
                let id = ev["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    return;
                }
                let title: String = ev["title"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                let kind: String = ev["kind"]
                    .as_str()
                    .unwrap_or("other")
                    .chars()
                    .take(40)
                    .collect();
                // 同 id 重复 start：去掉旧记录，以最新一次为准
                run.open.retain(|(x, _, _)| x != &id);
                run.open.push((id.clone(), title.clone(), kind.clone()));
                match run.mode.as_str() {
                    "stream" => push_stream(
                        st,
                        &mut run,
                        inv_id,
                        Some(StreamStepView {
                            id,
                            title,
                            kind,
                            ok: None,
                        }),
                    ),
                    "status" => push_current_step(st, &run),
                    _ => {}
                }
            }
            Some("step_end") => {
                let id = ev["id"].as_str().unwrap_or_default();
                let ok = ev["ok"].as_bool().unwrap_or(false);
                // 未知或重复的 step_end（没开过这步）忽略
                let Some(pos) = run.open.iter().position(|(x, _, _)| x == id) else {
                    return;
                };
                let (_, title, kind) = run.open.remove(pos);
                run.steps.push(AgentStepView {
                    id: id.to_string(),
                    title: title.clone(),
                    kind: kind.clone(),
                    ok,
                });
                match run.mode.as_str() {
                    "stream" => push_stream(
                        st,
                        &mut run,
                        inv_id,
                        Some(StreamStepView {
                            id: id.to_string(),
                            title,
                            kind,
                            ok: Some(ok),
                        }),
                    ),
                    "status" => push_current_step(st, &run),
                    // steps：每步完成落一条 Agent 消息
                    "steps" => {
                        let title = if title.is_empty() {
                            "未命名步骤".to_string()
                        } else {
                            title
                        };
                        let icon = if ok { "✓" } else { "✗" };
                        step_msg = Some((
                            run.conversation_id,
                            run.owner,
                            run.agent_id,
                            format!("{icon} {}", title.chars().take(300).collect::<String>()),
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if let Some((conv, owner, agent_id, text)) = step_msg {
        post_agent_text(st, conv, owner, agent_id, &text, None).await;
    }
}

/// stream 模式：积压 delta 距上次推送 ≥200ms 立即推，否则挂延迟冲刷任务。
pub(super) fn maybe_flush_stream(st: &SharedState, run: &mut InvRun, inv_id: Uuid) {
    if run.stream_buf.is_empty() {
        return;
    }
    if run.last_push.elapsed() >= Duration::from_millis(STREAM_FLUSH_MS) {
        push_stream(st, run, inv_id, None);
    } else if !run.flushing {
        run.flushing = true;
        let st2 = st.clone();
        tokio::spawn(async move { stream_flush_task(st2, inv_id).await });
    }
}

/// 延迟冲刷：每 200ms 检查一次缓冲，有货就推；清空后任务退出。
pub(super) async fn stream_flush_task(st: SharedState, inv_id: Uuid) {
    loop {
        tokio::time::sleep(Duration::from_millis(STREAM_FLUSH_MS)).await;
        let Some(mut run) = st.inv_runs.get_mut(&inv_id) else {
            return; // 调用已终结
        };
        if run.stream_buf.is_empty() {
            run.flushing = false;
            return;
        }
        push_stream(&st, &mut run, inv_id, None);
    }
}

/// 发一帧 agent_stream：带 step 时顺带冲刷积压 delta（保证文本先于步骤到达）。
pub(super) fn push_stream(st: &SharedState, run: &mut InvRun, inv_id: Uuid, step: Option<StreamStepView>) {
    let delta = if run.stream_buf.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut run.stream_buf))
    };
    if delta.is_none() && step.is_none() {
        return;
    }
    let frame = serde_json::to_string(&WsOut::AgentStream {
        invocation_id: inv_id,
        conversation_id: run.conversation_id,
        delta,
        step,
    })
    .unwrap();
    // 接收者在成员变更提交后重算，这里不查库（流式大约每 200ms 一帧）。
    st.send_to_users(&run.member_ids, &frame);
    run.last_push = Instant::now();
}

/// status 模式：步骤开/收把 current_step 填进调用快照再推状态帧。
pub(super) fn push_current_step(st: &SharedState, run: &InvRun) {
    let mut inv = run.inv.clone();
    inv.current_step = run.open.last().map(|(_, t, _)| t.clone());
    push_invocation(st, &run.member_ids, &inv);
}

#[derive(sqlx::FromRow)]
pub(super) struct FinishHead {
    pub(super) status: String,
    pub(super) owner_id: Uuid,
    pub(super) conversation_id: Uuid,
    pub(super) agent_id: Uuid,
    pub(super) host_id: Uuid,
    pub(super) schedule_id: Option<Uuid>,
    pub(super) schedule_name: Option<String>,
    pub(super) context_key: Option<String>,
    /// 派发时记下的代次。NULL = 迁移前已经派发的调用。
    pub(super) context_epoch: Option<i64>,
}

/// 宿主回传的 invocation_finish：校验该调用确实派给这条宿主连接。
/// 行锁内推进终态。取消已经落库时这里读到的不是 running，直接忽略，不发回复。
/// ok:true → 先写 Agent 消息（stream/status 模式的定稿消息带上步骤记录）再置 succeeded；
/// ok:false → failed + error（截 500 字符）。
/// 定时调用的回复在正文前加一行「⏰ 定时任务「名字」」，上下文键是 schedule:<id>。
pub async fn handle_finish(st: &SharedState, host_id: Uuid, v: &serde_json::Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    let mut tx = match st.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "invocation_finish 开事务失败");
            return;
        }
    };
    let row = sqlx::query_as::<_, FinishHead>(
        "SELECT i.status, a.owner_id, i.conversation_id, a.id AS agent_id, a.host_id,
                i.schedule_id, s.name AS schedule_name, i.context_key, i.context_epoch
           FROM agent_invocations i
           JOIN agents a ON a.id = i.agent_id
           LEFT JOIN agent_schedules s ON s.id = i.schedule_id
          WHERE i.id = $1
          FOR UPDATE OF i",
    )
    .bind(inv_id)
    .fetch_optional(&mut *tx)
    .await;
    let Ok(Some(row)) = row else {
        return;
    };
    let FinishHead {
        status,
        owner_id: owner,
        conversation_id: conv,
        agent_id,
        host_id: real_host,
        schedule_id,
        schedule_name,
        context_key: stored_key,
        context_epoch,
    } = row;
    // 已取消 / 已超时 / 已结束，或不是这条宿主的调用：不发回复。
    // awaiting_approval 仍是进行中（进程崩了也会回 finish）。
    if (status != "running" && status != "awaiting_approval") || real_host != host_id {
        return;
    }
    let context_key = stored_key
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match schedule_id {
            Some(sid) => format!("schedule:{sid}"),
            None => conv.to_string(),
        });

    // A03：摘下运行时状态；终结前的滞留增量在定稿消息前冲刷出去
    let run = st.inv_runs.remove(&inv_id).map(|(_, r)| r);

    if v["ok"].as_bool() == Some(true) {
        // 失败的 finish 不进这个分支，因此不会覆盖已记住的运行时会话。
        let mut text = v["text"].as_str().unwrap_or_default().to_string();
        if !text.is_empty() {
            if let Some(name) = schedule_name.as_deref().filter(|s| !s.is_empty()) {
                text = format!("⏰ 定时任务「{name}」\n{text}");
            }
        }
        // A05：配额不够没发出去的产出文件，在正文末尾说明
        let skipped = v["files_skipped"].as_i64().unwrap_or(0).max(0);
        if skipped > 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("（{skipped} 个文件因存储空间不足未发送）"));
        }
        // 断网重试到期等原因没传上去的产出，不能当作没产出
        let failed = v["files_failed"].as_i64().unwrap_or(0).max(0);
        if failed > 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("（{failed} 个文件上传失败未发送）"));
        }
        // 步骤记录：finish 帧带的 steps 优先（宿主权威记录），缺省退回事件累积
        let steps: Vec<AgentStepView> = match v["steps"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|s| serde_json::from_value(s.clone()).ok())
                .collect(),
            None => run.as_ref().map(|r| r.steps.clone()).unwrap_or_default(),
        };
        let mode = run.as_ref().map(|r| r.mode.as_str()).unwrap_or("stream");
        if let Some(r) = &run {
            if !r.stream_buf.is_empty() && r.mode == "stream" {
                let frame = serde_json::to_string(&WsOut::AgentStream {
                    invocation_id: inv_id,
                    conversation_id: r.conversation_id,
                    delta: Some(r.stream_buf.clone()),
                    step: None,
                })
                .unwrap();
                st.send_to_users(&r.member_ids, &frame);
            }
        }
        // steps 模式的正文不带步骤记录（步骤消息已逐条落库）；
        // stream/status 的定稿消息带 agent_steps（空集不落列）。
        let steps_json = match mode {
            "steps" => None,
            _ if steps.is_empty() => None,
            _ => serde_json::to_value(&steps).ok(),
        };
        // 持锁写回复，取消的 UPDATE 会等这把锁，提交后看到的已是终态
        post_agent_text(st, conv, owner, agent_id, &text, steps_json).await;
        // A05：正文之后按序发产出附件；files 里只认本次调用凭据上传的附件
        post_agent_files(st, conv, owner, agent_id, inv_id, &v["files"]).await;
        // 代次取调用行（上面 FOR UPDATE 锁住的那一列），不看内存里的运行时快照。
        if let Some(sid) = v["session_id"].as_str().filter(|s| !s.is_empty()) {
            if let Err(e) =
                persist_runtime_session(&mut tx, agent_id, &context_key, sid, context_epoch).await
            {
                tracing::error!(error = %e, "agent_context upsert failed");
                return;
            }
        }
        // 成功时记下完整回复（截 100k 字符），供互调查询。失败不写。
        let stored: String = text.chars().take(100_000).collect();
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'succeeded', error = NULL, finished_at = now(), result_text = $2
              WHERE id = $1 AND status IN ('running', 'awaiting_approval')
             {INV_RETURNING}"
        ))
        .bind(inv_id)
        .bind(&stored)
        .fetch_optional(&mut *tx)
        .await;
        if tx.commit().await.is_err() {
            return;
        }
        if let Ok(Some(mut inv)) = updated {
            inv.result_preview =
                result_preview_text(inv.parent_invocation_id, &inv.status, &stored);
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
            let member_ids = with_root_members(st, &member_ids, &inv).await;
            push_invocation(st, &member_ids, &inv);
        }
        crate::approvals::expire_invocation(st, inv_id).await;
    } else {
        let err = v["error"].as_str().unwrap_or("agent error");
        let err: String = err.chars().take(500).collect();
        let updated = sqlx::query_as::<_, InvocationView>(&format!(
            "UPDATE agent_invocations
                SET status = 'failed', error = $2, finished_at = now()
              WHERE id = $1 AND status IN ('running', 'awaiting_approval')
             {INV_RETURNING}"
        ))
        .bind(inv_id)
        .bind(&err)
        .fetch_optional(&mut *tx)
        .await;
        if tx.commit().await.is_err() {
            return;
        }
        if let Ok(Some(inv)) = updated {
            let member_ids = domain::member_ids(&st.pool, conv)
                .await
                .unwrap_or_else(|_| vec![owner]);
            let member_ids = with_root_members(st, &member_ids, &inv).await;
            push_invocation(st, &member_ids, &inv);
        }
        crate::approvals::expire_invocation(st, inv_id).await;
    }
}

/// Agent A05：finish.files 给出的附件逐个发成 Agent 的附件消息（排在正文之后）。
/// 只认 attachments.agent_invocation_id = 本次调用的行——别调用/普通上传/不存在的
/// id 一律静默忽略；文件顺序与 files 数组一致（unnest ORDINALITY 还原）。
pub(super) async fn post_agent_files(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    agent_id: Uuid,
    inv_id: Uuid,
    files: &Value,
) {
    let ids: Vec<Uuid> = files
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return;
    }
    let rows = sqlx::query_scalar::<_, Uuid>(
        "SELECT a.id
           FROM unnest($2::uuid[]) WITH ORDINALITY u(id, ord)
           JOIN attachments a ON a.id = u.id
          WHERE a.agent_invocation_id = $1
          ORDER BY u.ord",
    )
    .bind(inv_id)
    .bind(&ids)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_default();
    for att_id in rows {
        let posted = crate::ws::post_and_broadcast(
            st,
            domain::PostMessage {
                attachment_id: Some(att_id),
                agent_id: Some(agent_id),
                ..domain::PostMessage::text(conv, owner, "")
            },
            Vec::new(),
        )
        .await;
        match posted {
            Ok(_) => {}
            Err(PostError::Deny { code, message }) => {
                tracing::warn!(code, message, "agent file message rejected");
            }
            Err(PostError::Db(e)) => {
                tracing::error!(error = %e, "agent file message insert failed");
            }
        }
    }
}

/// Agent 正文拆条写库 + 逐条广播。空正文不发消息（调用仍算成功）。
/// steps 只附在最后一个分片消息上（A03：定稿消息的步骤记录）。
pub async fn post_agent_text(
    st: &SharedState,
    conv: Uuid,
    owner: Uuid,
    agent_id: Uuid,
    text: &str,
    agent_steps: Option<serde_json::Value>,
) {
    // 预计算分片：非空白片才发；最后一片挂 agent_steps
    let mut chunks: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take = rest
            .char_indices()
            .nth(4000)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(take);
        rest = tail;
        if !head.trim().is_empty() {
            chunks.push(head);
        }
    }
    let last = chunks.len().saturating_sub(1);
    for (i, chunk) in chunks.iter().enumerate() {
        let posted = crate::ws::post_and_broadcast(
            st,
            domain::PostMessage {
                agent_id: Some(agent_id),
                agent_steps: if i == last { agent_steps.clone() } else { None },
                ..domain::PostMessage::text(conv, owner, chunk)
            },
            Vec::new(),
        )
        .await;
        match posted {
            Ok(_) => {}
            Err(PostError::Deny { code, message }) => {
                tracing::warn!(code, message, "agent message rejected");
                break;
            }
            Err(PostError::Db(e)) => {
                tracing::error!(error = %e, "agent message insert failed");
                break;
            }
        }
    }
}
