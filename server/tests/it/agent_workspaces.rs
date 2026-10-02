use super::*;

// ===== Agent A27 =====

async fn a27_reply<S>(sock: &mut S) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..8 {
        let frames = until_frame(sock, "message", A01Dur::from_secs(20)).await;
        if let Some(m) = frames
            .iter()
            .rev()
            .find(|f| f["message"]["agent"].is_object())
        {
            return m["message"]["content"].as_str().unwrap_or("").to_string();
        }
    }
    panic!("没收到 Agent 回复");
}

async fn a27_req(
    method: reqwest::Method,
    url: String,
    u: &TestUser,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = http()
        .request(method, url)
        .header("authorization", authz(&u.access));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(json!(null)))
}

async fn a27_convs(base: &str, u: &TestUser) -> Vec<Value> {
    let (_, v) = a27_req(
        reqwest::Method::GET,
        format!("{base}/v1/conversations"),
        u,
        None,
    )
    .await;
    v.as_array().cloned().unwrap_or_default()
}

/// 工作区对话：用工作区目录和对话选的模型 / 模式；改模型后续上的会话按新值；
/// 专属会话不受影响，Agent 列表照常。
#[tokio::test]
async fn a27_session_uses_workspace_and_overrides() {
    use reqwest::Method as M;
    let base = spawn_named_db(&format!("chat_t_a27s_{}", uniq())).await;
    let a = register(&base, &format!("a27s_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "wsbot").await;
    let agent_id = agent["id"].as_str().unwrap();
    let home = agent["conversation_id"].as_str().unwrap().to_string();

    let dir = std::env::temp_dir().join(format!("a27ws_{}", uniq()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.to_string_lossy().to_string();
    let (s, ws) = a27_req(
        M::POST,
        format!("{base}/v1/agent-hosts/{host_id}/workspaces"),
        &a,
        Some(json!({ "path": path })),
    )
    .await;
    assert_eq!(s, 200, "{ws}");
    let ws_id = ws["id"].as_str().unwrap();
    let (s, created) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws_id}/sessions"),
        &a,
        Some(json!({"agent_id": agent_id, "name": "第一条", "model": "smart", "mode": "code"})),
    )
    .await;
    assert_eq!(s, 200, "{created}");
    let conv = created["conversation_id"].as_str().unwrap().to_string();

    // 列表：对话带 workspace_id / model / mode；专属会话仍然只有一条、Agent 列表不报错
    let convs = a27_convs(&base, &a).await;
    let row = convs.iter().find(|c| c["id"] == conv.as_str()).unwrap();
    assert_eq!(row["name"], "第一条");
    assert_eq!(row["agent"]["workspace_id"], ws_id);
    assert_eq!(row["agent"]["model"], "smart");
    let home_row = convs.iter().find(|c| c["id"] == home.as_str()).unwrap();
    assert!(home_row["agent"]["workspace_id"].is_null());
    let list = list_my_agents(&base, &a).await;
    assert_eq!(
        agent_named(&list, "wsbot")["conversation_id"],
        home.as_str()
    );

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "w1", "whoami").await;
    let text = a27_reply(&mut sa).await;
    assert!(text.contains("model=smart"), "{text}");
    assert!(text.contains("mode=code"), "{text}");
    assert!(text.contains(&format!("cwd={path}")), "{text}");

    // 换模型：下一条消息续上同一会话，按新模型
    let (s, _) = a27_req(
        M::PATCH,
        format!("{base}/v1/agent-sessions/{conv}"),
        &a,
        Some(json!({"model": "fast"})),
    )
    .await;
    assert_eq!(s, 204);
    send_ws(&mut sa, &conv, "w2", "whoami").await;
    let text = a27_reply(&mut sa).await;
    assert!(text.contains("model=fast"), "{text}");
    assert!(text.contains(&format!("cwd={path}")), "{text}");

    // 专属会话：工作目录不是工作区
    send_ws(&mut sa, &home, "w3", "whoami").await;
    let text = a27_reply(&mut sa).await;
    assert!(!text.contains(&format!("cwd={path}")), "{text}");

    h.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 工作区对话里的审批卡片就发在这条对话里，不进专属会话。
#[tokio::test]
async fn a27_session_approval_stays_in_session() {
    use reqwest::Method as M;
    let base = spawn_named_db(&format!("chat_t_a27p_{}", uniq())).await;
    let a = register(&base, &format!("a27p_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "permbot").await;
    let agent_id = agent["id"].as_str().unwrap();
    let home = agent["conversation_id"].as_str().unwrap().to_string();
    let (_, ws) = a27_req(
        M::POST,
        format!("{base}/v1/agent-hosts/{host_id}/workspaces"),
        &a,
        Some(json!({ "path": std::env::temp_dir().to_string_lossy() })),
    )
    .await;
    let ws_id = ws["id"].as_str().unwrap();
    let (_, created) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws_id}/sessions"),
        &a,
        Some(json!({ "agent_id": agent_id })),
    )
    .await;
    let conv = created["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "p1", "perm:execute:rm -rf /tmp/x").await;
    let mut card = None;
    for _ in 0..8 {
        let frames = until_frame(&mut sa, "message", A01Dur::from_secs(20)).await;
        if let Some(f) = frames.iter().find(|f| f["message"]["approval"].is_object()) {
            card = Some(f["message"].clone());
            break;
        }
    }
    let card = card.expect("应收到审批卡片");
    assert_eq!(card["conversation_id"], conv.as_str());
    assert_ne!(card["conversation_id"], home.as_str());
    h.shutdown().await;
}

/// 权限与边界：别人的电脑 404；Agent 和工作区不在同一台电脑 400；
/// 移除工作区后对话从列表消失，再加同一路径恢复；专属会话不能改名、不能删；工作区对话能删。
#[tokio::test]
async fn a27_workspace_scope_and_lifecycle() {
    use reqwest::Method as M;
    let base = spawn_named_db(&format!("chat_t_a27l_{}", uniq())).await;
    let a = register(&base, &format!("a27la_{}", uniq())).await;
    let b = register(&base, &format!("a27lb_{}", uniq())).await;
    let (h1, host1) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (h2, host2) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent1) = create_agent(&base, &a, &host1, "one").await;
    let agent1_id = agent1["id"].as_str().unwrap();
    let home = agent1["conversation_id"].as_str().unwrap().to_string();

    let ws_url_of = |h: &str| format!("{base}/v1/agent-hosts/{h}/workspaces");
    let (s, _) = a27_req(M::GET, ws_url_of(&host1), &b, None).await;
    assert_eq!(s, 404);
    let (s, _) = a27_req(
        M::POST,
        ws_url_of(&host1),
        &b,
        Some(json!({"path": "C:/x"})),
    )
    .await;
    assert_eq!(s, 404);
    let (s, _) = a27_req(M::POST, ws_url_of(&host1), &a, Some(json!({"path": "  "}))).await;
    assert_eq!(s, 400);

    let (_, ws2) = a27_req(
        M::POST,
        ws_url_of(&host2),
        &a,
        Some(json!({"path": "C:/p2"})),
    )
    .await;
    let ws2_id = ws2["id"].as_str().unwrap();
    let (s, _) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws2_id}/sessions"),
        &a,
        Some(json!({ "agent_id": agent1_id })),
    )
    .await;
    assert_eq!(s, 400, "Agent 在另一台电脑上");

    let (_, ws1) = a27_req(
        M::POST,
        ws_url_of(&host1),
        &a,
        Some(json!({"path": "C:/p1"})),
    )
    .await;
    let ws1_id = ws1["id"].as_str().unwrap().to_string();
    let (s, _) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws1_id}/sessions"),
        &b,
        Some(json!({ "agent_id": agent1_id })),
    )
    .await;
    assert_eq!(s, 404);
    let (_, created) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws1_id}/sessions"),
        &a,
        Some(json!({ "agent_id": agent1_id })),
    )
    .await;
    let conv = created["conversation_id"].as_str().unwrap().to_string();

    // 移除 → 列表里没有工作区和对话；再加同一路径 → 同一个工作区、对话回来
    let ws1_url = format!("{base}/v1/agent-workspaces/{ws1_id}");
    let (s, _) = a27_req(M::DELETE, ws1_url.clone(), &b, None).await;
    assert_eq!(s, 404);
    let (s, _) = a27_req(M::DELETE, ws1_url, &a, None).await;
    assert_eq!(s, 204);
    let (_, list) = a27_req(M::GET, ws_url_of(&host1), &a, None).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .all(|w| w["id"] != ws1_id.as_str()));
    assert!(!a27_convs(&base, &a)
        .await
        .iter()
        .any(|c| c["id"] == conv.as_str()));
    let (_, again) = a27_req(
        M::POST,
        ws_url_of(&host1),
        &a,
        Some(json!({"path": "C:/p1"})),
    )
    .await;
    assert_eq!(again["id"], ws1_id.as_str());
    assert!(a27_convs(&base, &a)
        .await
        .iter()
        .any(|c| c["id"] == conv.as_str()));

    // 专属会话：能改模型，不能改名、不能删
    let sess = |c: &str| format!("{base}/v1/agent-sessions/{c}");
    let (s, _) = a27_req(M::PATCH, sess(&home), &a, Some(json!({"model": "smart"}))).await;
    assert_eq!(s, 204);
    let (s, _) = a27_req(M::PATCH, sess(&home), &a, Some(json!({"name": "x"}))).await;
    assert_eq!(s, 400);
    let (s, _) = a27_req(M::DELETE, sess(&home), &a, None).await;
    assert_eq!(s, 400);
    // 别人的对话 404；工作区对话能改名、能删
    let (s, _) = a27_req(M::PATCH, sess(&conv), &b, Some(json!({"name": "x"}))).await;
    assert_eq!(s, 404);
    let (s, _) = a27_req(
        M::PATCH,
        sess(&conv),
        &a,
        Some(json!({"name": "改过的名字"})),
    )
    .await;
    assert_eq!(s, 204);
    let row = a27_convs(&base, &a)
        .await
        .into_iter()
        .find(|c| c["id"] == conv.as_str())
        .unwrap();
    assert_eq!(row["name"], "改过的名字");
    let (s, _) = a27_req(M::DELETE, sess(&conv), &a, None).await;
    assert_eq!(s, 204);
    assert!(!a27_convs(&base, &a)
        .await
        .iter()
        .any(|c| c["id"] == conv.as_str()));

    h1.shutdown().await;
    h2.shutdown().await;
}

/// 无工作区：每台电脑都有路径为空串的分组（排最前、不能移除）；
/// 它下面的对话在宿主程序目录下的 tmp/<对话 id> 里干活。
#[tokio::test]
async fn a27_scratch_session_runs_in_tmp_dir() {
    use reqwest::Method as M;
    let base = spawn_named_db(&format!("chat_t_a27t_{}", uniq())).await;
    let a = register(&base, &format!("a27t_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "tmpbot").await;
    let agent_id = agent["id"].as_str().unwrap();
    let url = format!("{base}/v1/agent-hosts/{host_id}/workspaces");
    let _ = a27_req(M::POST, url.clone(), &a, Some(json!({"path": "C:/zz"}))).await;
    let (s, list) = a27_req(M::GET, url.clone(), &a, None).await;
    assert_eq!(s, 200);
    let scratch = &list.as_array().unwrap()[0];
    assert_eq!(scratch["path"], "");
    let ws_id = scratch["id"].as_str().unwrap().to_string();
    // 再列一次不会多出第二个
    let (_, again) = a27_req(M::GET, url, &a, None).await;
    assert_eq!(again.as_array().unwrap().len(), 2);
    let (s, _) = a27_req(
        M::DELETE,
        format!("{base}/v1/agent-workspaces/{ws_id}"),
        &a,
        None,
    )
    .await;
    assert_eq!(s, 404, "无工作区分组不能移除");

    let (_, created) = a27_req(
        M::POST,
        format!("{base}/v1/agent-workspaces/{ws_id}/sessions"),
        &a,
        Some(json!({ "agent_id": agent_id })),
    )
    .await;
    let conv = created["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "t1", "whoami").await;
    let text = a27_reply(&mut sa).await;
    let cwd = text
        .lines()
        .find_map(|l| l.strip_prefix("cwd="))
        .unwrap_or_default()
        .replace('\\', "/");
    assert!(cwd.ends_with(&format!("tmp/{conv}")), "{text}");
    assert!(std::path::Path::new(&cwd).is_dir(), "{cwd}");
    let _ = std::fs::remove_dir_all(&cwd);
    h.shutdown().await;
}

/// 列目录：在线时回子目录；别人的电脑 404；离线 409。
#[tokio::test]
async fn a27_list_dirs_through_host() {
    use reqwest::Method as M;
    let base = spawn_named_db(&format!("chat_t_a27d_{}", uniq())).await;
    let a = register(&base, &format!("a27d_{}", uniq())).await;
    let b = register(&base, &format!("a27db_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let dir = std::env::temp_dir().join(format!("a27dir_{}", uniq()));
    std::fs::create_dir_all(dir.join("sub-one")).unwrap();
    let q = |p: &str| {
        format!(
            "{base}/v1/agent-hosts/{host_id}/dirs?path={}",
            urlencoding(p)
        )
    };
    let (s, v) = a27_req(M::GET, q(&dir.to_string_lossy()), &a, None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["dirs"][0]["name"], "sub-one");
    let (s, v) = a27_req(M::GET, q(""), &a, None).await;
    assert_eq!(s, 200);
    assert!(!v["dirs"].as_array().unwrap().is_empty());
    let (s, _) = a27_req(M::GET, q(""), &b, None).await;
    assert_eq!(s, 404);
    h.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(500)).await;
    let (s, _) = a27_req(M::GET, q(""), &a, None).await;
    assert_eq!(s, 409);
    let _ = std::fs::remove_dir_all(&dir);
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
