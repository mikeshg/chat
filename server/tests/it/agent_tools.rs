use super::*;

// ===== Agent A13 =====
// 工具注入通道 + 读当前会话历史：
// - invocation_start 下发调用专用凭据 tools_token（kind="invocation" 的 JWT）；
// - 宿主本地 MCP 服务凭它访问 GET /v1/agent-tools/history（Authorization: Invocation <token>）；
// - 凭据只在调用进行中（running/awaiting_approval）且主人未禁用时有效，且不能访问普通接口。
// 凭据相关测试用裸宿主 WS（不经过 agent-host 库）抓取并扣住 tools_token；
// 端到端测试走真 agent-host + node 假 agent（--mcp=http）。

/// 建一个 Agent 并发一条消息，让调用挂在 running（宿主不发 finish）。
/// 返回 (宿主WS, 主人WS, 调用id, tools_token, 专属会话id, agent_id)。
pub(crate) async fn a13_running_invocation(
    base: &str,
    u: &TestUser,
) -> (RawHostWs, RawHostWs, String, String, String, String) {
    let (mut sh, host_id) = spawn_raw_host(base, u).await;
    let (st, agent) = create_agent(base, u, &host_id, "tooler").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(base, u).await)
        .await
        .unwrap();
    send_ws(&mut su, &conv, &format!("a13-{}", uniq()), "echo:hold").await;
    let frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start = frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();
    let token = start["tools_token"]
        .as_str()
        .expect("invocation_start 应带 tools_token")
        .to_string();
    (sh, su, inv_id, token, conv, agent_id)
}

/// GET /v1/agent-tools/history；auth 是完整的 Authorization 头值。
async fn tool_history(base: &str, auth: &str, query: &str) -> reqwest::Response {
    http()
        .get(format!("{base}/v1/agent-tools/history{query}"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
}

/// 自签调用凭据（测试库 jwt_secret = "test-secret"）。
fn a13_mint(secret: &str, claims: Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// 主路径：真宿主 + 假 agent（--mcp=http）。两条普通消息后发 tool:read_history，
/// Agent 的回复里应包含前两条消息正文（工具结果经 MCP 服务 ← GET /v1/agent-tools/history）。
#[tokio::test]
async fn a13_tool_read_history_end_to_end() {
    let base = spawn_named_db(&format!("chat_t_a13e2e_{}", uniq())).await;
    let a = register(&base, &format!("a13e_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    // --mcp=http：假 agent 在 initialize 里声明 agentCapabilities.mcpCapabilities.http
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": "reader",
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let agent: Value = r.json().await.unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "h1", "第一条历史消息").await;
    send_ws(&mut sa, &conv, "h2", "第二条历史消息").await;
    send_ws(&mut sa, &conv, "h3", "tool:read_history").await;

    // 三条消息各触发一个调用；等到 3 个 succeeded，期间收集 message 帧
    let mut frames: Vec<Value> = Vec::new();
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    while frames
        .iter()
        .filter(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .count()
        < 3
    {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "三次调用未全部成功: {frames:?}");
        frames.extend(until_frame(&mut sa, "agent_invocation", left).await);
    }
    frames.extend(frames_for(&mut sa, A01Dur::from_millis(500)).await);

    // 第三个调用的 Agent 回复 = 工具结果文本，须包含前两条消息正文
    let found = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .map(|f| f["message"]["content"].as_str().unwrap_or_default())
        .any(|c| c.contains("第一条历史消息") && c.contains("第二条历史消息"));
    assert!(found, "回复应包含两条历史正文: {frames:?}");
    ha.shutdown().await;
}

/// 凭据生命周期：running 中可用（返回的是发起会话的消息，带分页参数语义）；
/// 调用结束（宿主 finish）后同一 token → 401。
#[tokio::test]
async fn a13_tools_token_dies_with_invocation() {
    let base = spawn_named_db(&format!("chat_t_a13tok_{}", uniq())).await;
    let a = register(&base, &format!("a13t_{}", uniq())).await;
    let (mut sh, mut su, inv_id, token, conv, _agent_id) = a13_running_invocation(&base, &a).await;

    // running 中：200，返回发起会话的消息
    let r = tool_history(&base, &format!("Invocation {token}"), "").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{page}");
    assert_eq!(msgs[0]["content"], "echo:hold");
    assert_eq!(msgs[0]["is_agent"], false);
    assert_eq!(msgs[0]["seq"].as_i64().unwrap(), 1);
    assert_eq!(page["has_more"], false);

    // 再发一条（第二个调用并行挂着）：limit=1 只回最新一条且 has_more；
    // before_seq=2 只回第一条
    send_ws(&mut su, &conv, "a13-2", "echo:more").await;
    let _ = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let r = tool_history(&base, &format!("Invocation {token}"), "?limit=1").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{page}");
    assert_eq!(msgs[0]["content"], "echo:more");
    assert_eq!(page["has_more"], true);
    let r = tool_history(&base, &format!("Invocation {token}"), "?before_seq=2").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["messages"][0]["seq"].as_i64().unwrap(), 1);

    // 结束第一个调用 → 它的凭据立刻失效（轮询等 finish 落库）
    host_finish(&mut sh, &inv_id).await;
    let mut dead = false;
    for _ in 0..50 {
        if tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status()
            == 401
        {
            dead = true;
            break;
        }
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    assert!(dead, "调用结束后 tools_token 应 401");
}

/// 两种凭据互不通用：access token 走 Invocation 通道 → 401；
/// 调用凭据访问普通 REST（Bearer 与 Invocation 两种写法）→ 401。
#[tokio::test]
async fn a13_token_kind_isolation() {
    let base = spawn_named_db(&format!("chat_t_a13iso_{}", uniq())).await;
    let a = register(&base, &format!("a13i_{}", uniq())).await;
    let (mut sh, _su, inv_id, token, _conv, _agent_id) = a13_running_invocation(&base, &a).await;

    // 普通 access token 不能当调用凭据用（无论 scheme）
    for auth in [format!("Invocation {}", a.access), authz(&a.access)] {
        assert_eq!(
            tool_history(&base, &auth, "").await.status(),
            401,
            "auth={auth}"
        );
    }
    // 调用凭据不能访问普通接口（无论 scheme）
    for auth in [authz(&token), format!("Invocation {token}")] {
        let r = http()
            .get(format!("{base}/v1/conversations"))
            .header("authorization", auth.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "auth={auth}");
    }
    host_finish(&mut sh, &inv_id).await;
}

/// 伪造（错签名/缺 kind/错 kind/指向不存在的调用）与过期凭据 → 401；
/// 主人被禁用后，仍挂在 running 的调用凭据 → 401。
#[tokio::test]
async fn a13_invocation_token_rejects_forgery_and_disabled() {
    let base = spawn_named_db(&format!("chat_t_a13sec_{}", uniq())).await;
    // 首个注册用户是管理员；主人单开（管理员账号不可被禁用，不能拿主人当第一用户）
    let admin = register(&base, &format!("a13adm_{}", uniq())).await;
    let a = register(&base, &format!("a13s_{}", uniq())).await;
    let (mut sh, _su, inv_id, token, _conv, agent_id) = a13_running_invocation(&base, &a).await;

    let now = chrono::Utc::now().timestamp();
    let claims = json!({"kind":"invocation","sub":inv_id,"owner":a.id,
                        "agent":agent_id,"depth":0,"iat":now,"exp":now+3600});
    // 对照组：真 token 与正确自签的 token 都是 200（凭据不登记，签名合法即可）
    assert_eq!(
        tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status(),
        200
    );
    assert_eq!(
        tool_history(
            &base,
            &format!("Invocation {}", a13_mint("test-secret", claims.clone())),
            ""
        )
        .await
        .status(),
        200
    );

    let mut expired = claims.clone();
    expired["exp"] = json!(now - 3600);
    let mut no_kind = claims.clone();
    no_kind.as_object_mut().unwrap().remove("kind");
    let mut wrong_kind = claims.clone();
    wrong_kind["kind"] = json!("access");
    let mut ghost = claims.clone();
    ghost["sub"] = json!(uuid::Uuid::new_v4());
    for (name, t) in [
        ("bad_sig", a13_mint("other-secret", claims.clone())),
        ("expired", a13_mint("test-secret", expired)),
        ("no_kind", a13_mint("test-secret", no_kind)),
        ("wrong_kind", a13_mint("test-secret", wrong_kind)),
        ("ghost_invocation", a13_mint("test-secret", ghost)),
    ] {
        assert_eq!(
            tool_history(&base, &format!("Invocation {t}"), "")
                .await
                .status(),
            401,
            "{name}"
        );
    }

    // 主人被禁用 → 调用仍是 running，凭据已死
    let r = http()
        .post(format!("{base}/v1/admin/users/{}/disable", a.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(
        tool_history(&base, &format!("Invocation {token}"), "")
            .await
            .status(),
        401
    );
    host_finish(&mut sh, &inv_id).await;
}

/// 接口只读发起调用的那个会话：没有会话参数可用来构造越权；
/// 主人在其它会话里的消息不会出现在返回里。
#[tokio::test]
async fn a13_history_scoped_to_invoking_conversation() {
    let base = spawn_named_db(&format!("chat_t_a13scp_{}", uniq())).await;
    let a = register(&base, &format!("a13x_{}", uniq())).await;
    let b = register(&base, &format!("a13y_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    // DM 里放一条不该出现在工具结果里的消息，等它落库（收到 message 帧即已提交）
    send_ws(&mut sa, &dm, "dm1", "不该被读到的私聊正文").await;
    let _ = next_of(&mut sa, "message").await;

    let (mut sh, _su, inv_id, token, _conv, _agent_id) = a13_running_invocation(&base, &a).await;
    let r = tool_history(&base, &format!("Invocation {token}"), "?limit=200").await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let msgs = page["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "应只返回发起会话的消息: {msgs:?}");
    assert_eq!(msgs[0]["content"], "echo:hold");
    host_finish(&mut sh, &inv_id).await;
}

// ===== Agent A16 =====
// 搜索 / 读取主人参与的其他会话，并以当前 Agent 身份往这些会话发消息。
// 假 agent：tool:list / tool:search:<词> / tool:read:<会话> / tool:send:<会话>:<正文>
// / tool:sendfile:<会话>:<文件名>:<内容>。发消息在 read_only 档先走审批。

async fn a16_mcp_agent(
    base: &str,
    u: &TestUser,
    name: &str,
    tier: &str,
) -> (agent_host::HostHandle, String, String) {
    let (h, host_id) = spawn_host(base, u, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .json(&json!({
            "name": name,
            "host_id": host_id,
            "runtime": {"command": "node", "args": args},
            "cwd": "",
            "permission_tier": tier,
        }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let agent: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    (h, agent_id, conv)
}

async fn a16_group(base: &str, owner: &TestUser, peer: &TestUser, name: &str) -> String {
    let r = http()
        .post(format!("{base}/v1/conversations/group"))
        .header("authorization", authz(&owner.access))
        .json(&json!({"name": name, "member_ids": [peer.id]}))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let detail: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(status, 200, "{detail}");
    detail["id"].as_str().unwrap().to_string()
}

async fn a16_wait_reply(base: &str, u: &TestUser, conv: &str, needle: &str) -> String {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(t) = msgs.iter().rev().find_map(|m| {
            let c = m["content"].as_str().unwrap_or("");
            if m["agent"].is_object() && m["kind"] != "approval" && c.contains(needle) {
                Some(c.to_string())
            } else {
                None
            }
        }) {
            return t;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等回复含「{needle}」超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_msg(
    base: &str,
    u: &TestUser,
    conv: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = msgs.iter().find(|m| pred(m)) {
            return m.clone();
        }
        assert!(std::time::Instant::now() < deadline, "等消息超时：{msgs:?}");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_pending(base: &str, u: &TestUser, conv: &str) -> Value {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(m) = msgs
            .iter()
            .rev()
            .find(|m| m["kind"] == "approval" && m["approval"]["status"] == "pending")
        {
            return m.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等审批卡片超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn a16_wait_succeeded(base: &str, u: &TestUser, conv: &str) {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let rows = list_inv(base, u, conv).await;
        if latest_inv_status(&rows) == Some("succeeded") {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "调用未成功：{rows}");
        tokio::time::sleep(A01Dur::from_millis(80)).await;
    }
}

fn a16_line<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("输出里没有「{needle}」：{text}"))
}

async fn a16_tool_get(base: &str, auth: &str, path: &str) -> reqwest::Response {
    http()
        .get(format!("{base}{path}"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
}

async fn a16_tool_post(base: &str, auth: &str, path: &str, body: &Value) -> (u16, Value) {
    let r = http()
        .post(format!("{base}{path}"))
        .header("authorization", auth)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let v = r.json().await.unwrap_or(json!(null));
    (status, v)
}

async fn a16_upload_to(
    base: &str,
    auth: &str,
    conv: Option<&str>,
    name: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut url = format!("{base}/v1/agent-tools/files");
    if let Some(c) = conv {
        url.push_str(&format!("?conversation_id={c}"));
    }
    let part = reqwest::multipart::Part::bytes(body)
        .file_name(name.to_string())
        .mime_str("text/plain")
        .unwrap();
    http()
        .post(url)
        .header("authorization", auth)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

/// 1：列出主人参与的群 / 私聊 / 文件传输助手 / 专属会话，不含主人不在的群。
#[tokio::test]
async fn a16_tool_list_only_owner_conversations() {
    let base = spawn_named_db(&format!("chat_it_a16a_{}", uniq())).await;
    let a = register(&base, &format!("a16a_{}", uniq())).await;
    let b = register(&base, &format!("a16b_{}", uniq())).await;
    let c = register(&base, &format!("a16c_{}", uniq())).await;
    let d = register(&base, &format!("a16d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let tag = uniq();
    let gname = format!("可见群{tag}");
    let sname = format!("秘密群{tag}");
    let group = a16_group(&base, &a, &b, &gname).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, &sname).await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "lister", "read_only").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "g1", "群里打个招呼").await;
    let _ = next_of(&mut sa, "message").await;
    send_ws(&mut sa, &conv, "cmd", "tool:list").await;
    let reply = a16_wait_reply(&base, &a, &conv, &group).await;

    let grow = a16_line(&reply, &group);
    assert!(grow.contains("kind=group"), "{grow}");
    assert!(grow.contains(&format!("title={gname}")), "{grow}");
    assert!(grow.contains("members=2"), "{grow}");
    assert!(grow.contains("last_seq=1"), "{grow}");
    assert!(grow.contains("readonly=false"), "{grow}");

    let dm = a16_line(&reply, "kind=dm");
    assert!(dm.contains(&format!("title={}", b.username)), "{dm}");
    assert!(dm.contains("members=2"), "{dm}");
    assert!(dm.contains("readonly=false"), "{dm}");

    let self_line = a16_line(&reply, "kind=self");
    assert!(self_line.contains("title=文件传输助手"), "{self_line}");
    assert!(self_line.contains("members=1"), "{self_line}");
    assert!(self_line.contains("readonly=false"), "{self_line}");

    let agent_line = a16_line(&reply, "kind=agent");
    assert!(agent_line.contains("title=lister"), "{agent_line}");
    assert!(agent_line.contains("readonly=false"), "{agent_line}");

    assert!(!reply.contains(&sname), "不应列出主人不在的群：{reply}");
    assert!(!reply.contains(&secret), "不应出现外人的群 id：{reply}");
    // 刚说过话的群比没有消息的文件传输助手更靠前
    assert!(
        reply.find(&group).unwrap() < reply.find("文件传输助手").unwrap(),
        "最近活跃的群应排在文件传输助手之前：{reply}"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 2：搜得到主人在群里看得到的消息；搜不到主人不在的群里的消息。
#[tokio::test]
async fn a16_tool_search_respects_membership() {
    let base = spawn_named_db(&format!("chat_it_a16b_{}", uniq())).await;
    let a = register(&base, &format!("a16e_{}", uniq())).await;
    let b = register(&base, &format!("a16f_{}", uniq())).await;
    let c = register(&base, &format!("a16g_{}", uniq())).await;
    let d = register(&base, &format!("a16h_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let tag = uniq();
    let gname = format!("项目群{tag}");
    let sname = format!("密室{tag}");
    let group = a16_group(&base, &a, &b, &gname).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, &sname).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "v1", "甲乙共有词-可见甲").await;
    let _ = next_of(&mut sa, "message").await;
    let (mut sc, _) = tokio_tungstenite::connect_async(ws_url(&base, &c).await)
        .await
        .unwrap();
    send_ws(&mut sc, &secret, "s1", "甲乙共有词-机密乙").await;
    let _ = next_of(&mut sc, "message").await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "finder", "read_only").await;
    send_ws(&mut sa, &conv, "cmd", "tool:search:甲乙共有词").await;
    let reply = a16_wait_reply(&base, &a, &conv, "可见甲").await;
    assert!(reply.contains(&gname), "应带上会话标题：{reply}");
    assert!(reply.contains("可见甲"), "{reply}");
    assert!(!reply.contains("机密乙"), "不应搜到主人不在的群：{reply}");
    assert!(!reply.contains(&sname), "{reply}");
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 3：读得到自己群的历史；读主人不在的会话，工具回错误文本（403）。
#[tokio::test]
async fn a16_tool_read_history_and_forbidden() {
    let base = spawn_named_db(&format!("chat_it_a16c_{}", uniq())).await;
    let a = register(&base, &format!("a16i_{}", uniq())).await;
    let b = register(&base, &format!("a16j_{}", uniq())).await;
    let c = register(&base, &format!("a16k_{}", uniq())).await;
    let d = register(&base, &format!("a16l_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "可读群").await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, "不可读群").await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &group, "h1", "历史甲读得到").await;
    let _ = next_of(&mut sa, "message").await;
    let (mut sc, _) = tokio_tungstenite::connect_async(ws_url(&base, &c).await)
        .await
        .unwrap();
    send_ws(&mut sc, &secret, "h2", "历史乙读不到").await;
    let _ = next_of(&mut sc, "message").await;

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "reader16", "read_only").await;
    send_ws(&mut sa, &conv, "r1", &format!("tool:read:{group}")).await;
    let ok = a16_wait_reply(&base, &a, &conv, "历史甲读得到").await;
    assert!(ok.contains("[seq "), "应是可读的历史行：{ok}");
    assert!(!ok.contains("历史乙读不到"), "{ok}");

    send_ws(&mut sa, &conv, "r2", &format!("tool:read:{secret}")).await;
    let err = a16_wait_reply(&base, &a, &conv, "tool error").await;
    assert!(err.contains("403"), "{err}");
    assert!(err.contains("not a member"), "{err}");
    assert!(!err.contains("历史乙读不到"), "{err}");
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 4：workspace 直接发。群里出现 Agent 的消息，成员收到并计未读。
#[tokio::test]
async fn a16_workspace_send_reaches_members() {
    let base = spawn_named_db(&format!("chat_it_a16d_{}", uniq())).await;
    let a = register(&base, &format!("a16m_{}", uniq())).await;
    let b = register(&base, &format!("a16n_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "收件群").await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "speaker", "workspace").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "s1",
        &format!("tool:send:{group}:hello-from-agent"),
    )
    .await;

    let frames = until_frame(&mut sb, "message", A01Dur::from_secs(45)).await;
    let hit = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["content"] == "hello-from-agent")
        .unwrap_or_else(|| panic!("成员应收到消息：{frames:?}"));
    assert_eq!(hit["message"]["agent"]["id"], agent_id.as_str());
    assert_eq!(hit["message"]["agent"]["name"], "speaker");
    assert_eq!(a11_unread(&base, &b, &group).await, 1, "群成员未读 +1");

    let own = conv_messages(&base, &a, &conv).await;
    assert!(
        own.iter().all(|m| m["kind"] != "approval"),
        "workspace 不应产生审批卡片：{own:?}"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 5：read_only 先在专属会话出审批卡片。批准后群里出现消息；拒绝则没有消息，回复是拒绝提示。
#[tokio::test]
async fn a16_read_only_send_needs_approval() {
    let base = spawn_named_db(&format!("chat_it_a16e_{}", uniq())).await;
    let a = register(&base, &format!("a16o_{}", uniq())).await;
    let b = register(&base, &format!("a16p_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gname = "审批群";
    let group = a16_group(&base, &a, &b, gname).await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "asker", "read_only").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let allow_text = "批准后的hello";
    send_ws(
        &mut sa,
        &conv,
        "p1",
        &format!("tool:send:{group}:{allow_text}"),
    )
    .await;
    let card = a16_wait_pending(&base, &a, &conv).await;
    assert_eq!(card["agent"]["id"], agent_id.as_str());
    let req = &card["approval"]["request"];
    assert_eq!(req["kind"], "send_message");
    assert!(
        req["locations"].as_array().is_some_and(|a| a.is_empty()),
        "{req}"
    );
    let title = req["title"].as_str().unwrap_or("");
    assert!(title.contains(&format!("发消息到「{gname}」")), "{title}");
    assert!(title.contains(allow_text), "{title}");
    let opts = card["approval"]["options"].as_array().unwrap();
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "allow_once" && o["kind"] == "allow_once"));
    assert!(opts
        .iter()
        .any(|o| o["optionId"] == "reject_once" && o["kind"] == "reject_once"));
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(
        latest_inv_status(&rows),
        Some("awaiting_approval"),
        "{rows}"
    );
    assert!(
        conv_messages(&base, &a, &group)
            .await
            .iter()
            .all(|m| m["content"] != allow_text),
        "批准前群里不应有这条消息"
    );

    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id, "allow_once").await;
    assert_eq!(st, 200, "{body}");
    let _ = a16_wait_reply(&base, &a, &conv, "已发送").await;
    let sent = a16_wait_msg(&base, &a, &group, |m| m["content"] == allow_text).await;
    assert_eq!(sent["agent"]["id"], agent_id.as_str());
    assert_eq!(sent["agent"]["name"], "asker");
    a16_wait_succeeded(&base, &a, &conv).await;

    let deny_text = "不该发出去的no";
    send_ws(
        &mut sa,
        &conv,
        "p2",
        &format!("tool:send:{group}:{deny_text}"),
    )
    .await;
    let card = a16_wait_pending(&base, &a, &conv).await;
    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&base, &a, id, "reject_once").await;
    assert_eq!(st, 200, "{body}");
    let reply = a16_wait_reply(&base, &a, &conv, "主人拒绝了发送").await;
    assert!(reply.contains("主人拒绝了发送"), "{reply}");
    assert!(
        conv_messages(&base, &a, &group)
            .await
            .iter()
            .all(|m| m["content"] != deny_text),
        "拒绝后群里不应有这条消息"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 6：sendfile 在群里留下 Agent 的文件消息，下载内容为写入的字节，计入主人配额。
#[tokio::test]
async fn a16_sendfile_counts_toward_owner_quota() {
    let base = spawn_named_db(&format!("chat_it_a16f_{}", uniq())).await;
    let a = register(&base, &format!("a16q_{}", uniq())).await;
    let b = register(&base, &format!("a16r_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "文件群").await;
    let (host, agent_id, conv) = a16_mcp_agent(&base, &a, "filer", "workspace").await;
    let used0: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "f1",
        &format!("tool:sendfile:{group}:a.txt:hi"),
    )
    .await;
    let m = a16_wait_msg(&base, &a, &group, |m| {
        m["kind"] == "file" && m["agent"].is_object()
    })
    .await;
    assert_eq!(m["agent"]["id"], agent_id.as_str());
    assert_eq!(m["attachment"]["file_name"], "a.txt");
    assert_eq!(m["attachment"]["mime"], "text/plain");
    let att = m["attachment"]["id"].as_str().unwrap();
    let data = http()
        .get(format!("{base}/v1/files/{att}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&data[..], b"hi");
    let used1: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        used1["used_bytes"].as_i64().unwrap(),
        used0["used_bytes"].as_i64().unwrap() + 2,
        "文件字节计入主人配额"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;
}

/// 7：只读私聊拒绝；别的调用的附件、以及没指定到目标会话的附件都拒绝；调用结束凭据 401。
#[tokio::test]
async fn a16_send_rejects_readonly_foreign_attachment_and_dead_token() {
    let base = spawn_named_db(&format!("chat_it_a16g_{}", uniq())).await;
    let a = register(&base, &format!("a16s_{}", uniq())).await;
    let b = register(&base, &format!("a16t_{}", uniq())).await;
    let c = register(&base, &format!("a16u_{}", uniq())).await;
    let d = register(&base, &format!("a16v_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let group = a16_group(&base, &a, &b, "边界群").await;
    let dm = dm_conv(&base, &a).await;
    befriend(&base, &c, &d).await;
    let secret = a16_group(&base, &c, &d, "外人群").await;
    let r = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let (host, _agent_id, conv) = a16_mcp_agent(&base, &a, "guard", "workspace").await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(
        &mut sa,
        &conv,
        "d1",
        &format!("tool:send:{dm}:不许发到只读"),
    )
    .await;
    let reply = a16_wait_reply(&base, &a, &conv, "只读").await;
    assert!(reply.contains("tool error"), "{reply}");
    assert!(
        conv_messages(&base, &a, &dm)
            .await
            .iter()
            .all(|m| m["content"] != "不许发到只读"),
        "只读私聊不应落下 Agent 消息"
    );
    a16_wait_succeeded(&base, &a, &conv).await;
    host.shutdown().await;

    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "rawbox").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let aconv = agent["conversation_id"].as_str().unwrap();
    send_ws(&mut sa, aconv, "t1", "hold-1").await;
    let f1 = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s1 = f1.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv1 = s1["invocation_id"].as_str().unwrap().to_string();
    let tok1 = s1["tools_token"].as_str().unwrap().to_string();
    let auth1 = format!("Invocation {tok1}");
    send_ws(&mut sa, aconv, "t2", "hold-2").await;
    let f2 = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s2 = f2.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let tok2 = s2["tools_token"].as_str().unwrap().to_string();
    let auth2 = format!("Invocation {tok2}");

    // 主人不是成员：上传和读历史都是 403
    let r = a16_upload_to(&base, &auth1, Some(&secret), "x.txt", b"no".to_vec()).await;
    assert_eq!(r.status(), 403);
    let r = a16_tool_get(
        &base,
        &auth1,
        &format!("/v1/agent-tools/conversations/{secret}/messages"),
    )
    .await;
    assert_eq!(r.status(), 403);
    let err: Value = r.json().await.unwrap();
    assert!(
        err["error"].as_str().unwrap_or("").contains("not a member"),
        "{err}"
    );

    // 空关键词拒绝；没有命中时是空列表
    let r = a16_tool_get(&base, &auth1, "/v1/agent-tools/search?q=").await;
    assert_eq!(r.status(), 400);
    let r = a16_tool_get(
        &base,
        &auth1,
        "/v1/agent-tools/search?q=zzzznotfound&limit=5",
    )
    .await;
    assert_eq!(r.status(), 200);
    let hits: Value = r.json().await.unwrap();
    assert!(hits.as_array().is_some_and(|a| a.is_empty()), "{hits}");

    // 缺省上传进发起会话，不能拿去别的会话发
    let r = a16_upload_to(&base, &auth1, None, "local.txt", b"loc".to_vec()).await;
    assert_eq!(r.status(), 200);
    let att_local = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = a16_tool_post(
        &base,
        &auth1,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "会话不对的附件", "attachment_ids": [att_local]}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(conv_messages(&base, &a, &group)
        .await
        .iter()
        .all(|m| m["content"] != "会话不对的附件"));

    // 指定了目标会话的附件，别的调用不能用
    let r = a16_upload_to(&base, &auth1, Some(&group), "ok.txt", b"ok".to_vec()).await;
    assert_eq!(r.status(), 200);
    let att_ok = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = a16_tool_post(
        &base,
        &auth2,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "借来的附件", "attachment_ids": [att_ok]}),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(conv_messages(&base, &a, &group)
        .await
        .iter()
        .all(|m| m["content"] != "借来的附件"));

    let send_body =
        json!({"conversation_id": group, "text": "直发一条", "attachment_ids": [att_ok]});
    let (st, body) = a16_tool_post(&base, &auth1, "/v1/agent-tools/messages", &send_body).await;
    assert_eq!(st, 200, "{body}");
    let message_id = body["message_id"].as_str().unwrap().to_string();
    let sent = a16_wait_msg(&base, &a, &group, |m| m["content"] == "直发一条").await;
    assert_eq!(sent["id"], message_id.as_str());
    assert_eq!(sent["agent"]["id"], agent_id);
    assert_eq!(sent["attachment"]["id"], att_ok.as_str());

    host_finish(&mut sh, &inv1).await;
    let mut dead = false;
    for _ in 0..50 {
        if a16_tool_get(&base, &auth1, "/v1/agent-tools/conversations")
            .await
            .status()
            == 401
        {
            dead = true;
            break;
        }
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    assert!(dead, "调用结束后列会话应 401");
    assert_eq!(
        a16_tool_get(&base, &auth1, "/v1/agent-tools/search?q=直发一条")
            .await
            .status(),
        401
    );
    assert_eq!(
        a16_tool_get(
            &base,
            &auth1,
            &format!("/v1/agent-tools/conversations/{group}/messages")
        )
        .await
        .status(),
        401
    );
    let (st, _) = a16_tool_post(
        &base,
        &auth1,
        "/v1/agent-tools/messages",
        &json!({"conversation_id": group, "text": "太晚了"}),
    )
    .await;
    assert_eq!(st, 401);
}
