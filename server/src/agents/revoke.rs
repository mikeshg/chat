use super::*;

// ---------- 使用权收回（A08） ----------

#[derive(sqlx::FromRow)]
pub(super) struct RevokedInv {
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

/// 进行中的调用改为 cancelled/agent_access_revoked，推帧，通知宿主取消，并断开这些用户的宿主。
pub async fn revoke_access(st: &SharedState, user_ids: &[Uuid]) {
    if user_ids.is_empty() {
        return;
    }
    let invs = sqlx::query_as::<_, RevokedInv>(
        "UPDATE agent_invocations i
            SET status = 'cancelled', error = 'agent_access_revoked', finished_at = now()
           FROM agents a
          WHERE i.agent_id = a.id
            AND a.owner_id = ANY($1)
            AND i.finished_at IS NULL
            AND i.status IN ('queued', 'running', 'awaiting_approval')
         RETURNING i.id, i.agent_id, i.conversation_id, i.trigger_message_id, i.schedule_id,
                   i.status, i.error, i.created_at, i.started_at, i.finished_at,
                   i.parent_invocation_id, i.root_conversation_id, a.name AS agent_name, a.host_id",
    )
    .bind(user_ids)
    .fetch_all(&st.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "revoke agent access failed");
        Vec::new()
    });
    for inv in &invs {
        notify_host_cancel(st, inv.host_id, inv.id).await;
        let view = InvocationView {
            id: inv.id,
            agent_id: inv.agent_id,
            conversation_id: inv.conversation_id,
            trigger_message_id: inv.trigger_message_id,
            schedule_id: inv.schedule_id,
            attachment_ids: Vec::new(),
            status: inv.status.clone(),
            error: inv.error.clone(),
            created_at: inv.created_at,
            started_at: inv.started_at,
            finished_at: inv.finished_at,
            current_step: None,
            parent_invocation_id: inv.parent_invocation_id,
            root_conversation_id: inv.root_conversation_id,
            agent_name: inv.agent_name.clone(),
            result_preview: None,
        };
        let members = domain::member_ids(&st.pool, inv.conversation_id)
            .await
            .unwrap_or_default();
        let members = with_root_members(st, &members, &view).await;
        push_invocation(st, &members, &view);
        crate::approvals::expire_invocation(st, inv.id).await;
    }
    let roots: Vec<Uuid> = invs.iter().map(|i| i.id).collect();
    cascade_cancel(st, &roots, Some("agent_access_revoked")).await;
    for uid in user_ids {
        st.disconnect_user_hosts(*uid);
    }
}

/// 全局开关或默认变了：谁的生效值从有到无，就收回；变了的人重拉 /users/me。
pub async fn on_settings_saved(
    st: &SharedState,
    before: &crate::settings::ServerSettings,
    after: &crate::settings::ServerSettings,
) {
    if before.agent_enabled == after.agent_enabled
        && before.agent_default_allowed == after.agent_default_allowed
    {
        return;
    }
    let rows = match sqlx::query_as::<_, (Uuid, String, Option<bool>)>(
        "SELECT id, role, agent_access FROM users",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "agent access policy diff failed");
            return;
        }
    };
    let mut lost = Vec::new();
    let mut changed = Vec::new();
    for (id, role, ov) in rows {
        let old = crate::agent_access::effective(
            &role,
            before.agent_enabled,
            before.agent_default_allowed,
            ov,
        );
        let new = crate::agent_access::effective(
            &role,
            after.agent_enabled,
            after.agent_default_allowed,
            ov,
        );
        if old != new {
            changed.push(id);
        }
        if old && !new {
            lost.push(id);
        }
    }
    revoke_access(st, &lost).await;
    if !changed.is_empty() {
        crate::api::notify(st, &changed, "me");
    }
}
