use super::*;

// ---------- Agent A07：登记表转发 ----------

pub(super) async fn require_own_host(st: &SharedState, uid: Uuid, host_id: Uuid) -> AppResult<()> {
    let owner = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM agent_hosts WHERE id = $1")
        .bind(host_id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if owner != uid {
        return Err(AppError::not_found("host not found"));
    }
    Ok(())
}

/// GET /v1/agent-hosts/{id}/registry：向宿主要列表，最多等 20 秒。
pub async fn registry_list(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    let uid = uid(&headers, &st).await?;
    require_own_host(&st, uid, id).await?;
    if !st.host_online(id) {
        return Err(AppError::conflict("host_offline"));
    }
    let request_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.registry_wait.insert(request_id, tx);
    let payload = json!({"type": "registry_list", "request_id": request_id}).to_string();
    if !st.send_to_host(id, &payload) {
        st.registry_wait.remove(&request_id);
        return Err(AppError::conflict("host_offline"));
    }
    let v = match tokio::time::timeout(Duration::from_secs(20), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            st.registry_wait.remove(&request_id);
            return Err(AppError::gateway_timeout("registry_timeout"));
        }
    };
    let entries = v.get("entries").cloned().unwrap_or_else(|| json!([]));
    let error = v.get("error").filter(|e| !e.is_null()).cloned();
    Ok(Json(json!({"entries": entries, "error": error})))
}

/// POST /v1/agent-hosts/{id}/registry/{entry_id}/install → 202，安装进度走 WS。
pub async fn registry_install(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((id, entry_id)): Path<(Uuid, String)>,
    Json(body): Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let uid = uid(&headers, &st).await?;
    require_own_host(&st, uid, id).await?;
    check_pin(&entry_id, 128)?;
    let version = body.get("version").and_then(Value::as_str).unwrap_or("");
    let version = check_pin(version, 64)?;
    if !st.host_online(id) {
        return Err(AppError::conflict("host_offline"));
    }
    let request_id = Uuid::new_v4();
    let payload = json!({
        "type": "registry_install",
        "request_id": request_id,
        "entry_id": entry_id,
        "version": version,
    })
    .to_string();
    if !st.send_to_host(id, &payload) {
        return Err(AppError::conflict("host_offline"));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"ok": true}))))
}

/// 宿主回 registry_result：配对到正在等的 GET。迟到的回包没有接收方就丢掉。
pub fn handle_registry_result(st: &SharedState, v: &Value) {
    let Some(id) = v["request_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return;
    };
    if let Some((_, tx)) = st.registry_wait.remove(&id) {
        let _ = tx.send(v.clone());
    }
}

/// 安装进度原样转给主人的普通连接，并带上 host_id。
pub fn handle_registry_progress(st: &SharedState, host_id: Uuid, v: &Value) {
    let Some(owner) = st.hosts.get(&host_id).map(|c| c.user_id) else {
        return;
    };
    let mut frame = json!({
        "type": "registry_install_progress",
        "host_id": host_id,
        "entry_id": v["entry_id"],
        "stage": v["stage"],
    });
    if let Some(err) = v
        .get("error")
        .and_then(|e| e.as_str())
        .filter(|s| !s.is_empty())
    {
        frame["error"] = json!(err);
    }
    st.send_to_user(owner, &frame.to_string());
}
