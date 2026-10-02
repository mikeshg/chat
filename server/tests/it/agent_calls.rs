use super::*;

// ===== Agent A14 =====
// 同一主人的两个宿主：Agent a 在宿主 1，Agent b 在宿主 2。假 agent 带 --mcp=http。

fn a14_args() -> Vec<String> {
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    args
}

struct A14Env {
    base: String,
    user: TestUser,
    h1: agent_host::HostHandle,
    h2: agent_host::HostHandle,
    host2: String,
    install2: uuid::Uuid,
    a_id: String,
    b_id: String,
    a_conv: String,
    b_conv: String,
}

async fn a14_pair(tag: &str) -> A14Env {
    let base = spawn_named_db(&format!("chat_it_a14{tag}_{}", uniq())).await;
    let user = register(&base, &format!("a14{tag}_{}", uniq())).await;
    let (h1, host1) = spawn_host(&base, &user, uuid::Uuid::new_v4()).await;
    let install2 = uuid::Uuid::new_v4();
    let (h2, host2) = spawn_host(&base, &user, install2).await;
    let (st, a) = create_agent_with(&base, &user, &host1, "a", a14_args()).await;
    assert_eq!(st, 200, "{a}");
    let (st, b) = create_agent_with(&base, &user, &host2, "b", a14_args()).await;
    assert_eq!(st, 200, "{b}");
    A14Env {
        base,
        user,
        h1,
        h2,
        host2,
        install2,
        a_id: a["id"].as_str().unwrap().to_string(),
        b_id: b["id"].as_str().unwrap().to_string(),
        a_conv: a["conversation_id"].as_str().unwrap().to_string(),
        b_conv: b["conversation_id"].as_str().unwrap().to_string(),
    }
}

async fn a14_user_ws(env: &A14Env) -> RawHostWs {
    let (s, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    s
}

/// 等到某条 Agent 消息满足 pred，返回正文。
async fn wait_agent_msg(
    base: &str,
    u: &TestUser,
    conv: &str,
    pred: impl Fn(&str) -> bool,
    timeout: A01Dur,
) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let msgs = conv_messages(base, u, conv).await;
        if let Some(hit) = msgs.iter().find_map(|m| {
            if !m["agent"].is_object() {
                return None;
            }
            let c = m["content"].as_str().unwrap_or("");
            pred(c).then(|| c.to_string())
        }) {
            return hit;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等 Agent 消息超时：{msgs:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

async fn agent_host_online(base: &str, u: &TestUser, agent_id: &str) -> bool {
    let list: Value = http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == agent_id)
        .unwrap()["host_online"]
        .as_bool()
        .unwrap()
}

/// 1. a 同步调用 b，回复是 b 的正文；b 的专属会话有「被 @a 调用」记录；父调用 id 对得上。
#[tokio::test]
async fn a14_sync_call_cross_host() {
    let env = a14_pair("sync").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "c1", "call:b:echo:hi").await;
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c == "hi",
        A01Dur::from_secs(45),
    )
    .await;
    assert_eq!(reply, "hi");

    let bmsgs = conv_messages(&env.base, &env.user, &env.b_conv).await;
    assert!(
        bmsgs.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .contains("📨 被 @a 调用")),
        "b 的专属会话应有被调用记录: {bmsgs:?}"
    );
    assert!(
        bmsgs
            .iter()
            .any(|m| m["agent"].is_object() && m["content"] == "hi"),
        "b 自己的回复应是 hi: {bmsgs:?}"
    );

    let a_rows = list_inv(&env.base, &env.user, &env.a_conv).await;
    let parent = a_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_id"] == env.a_id)
        .unwrap();
    assert!(parent["parent_invocation_id"].is_null());
    let b_rows = list_inv(&env.base, &env.user, &env.b_conv).await;
    let child = b_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["parent_invocation_id"] == parent["id"])
        .expect("子调用应挂在父调用上");
    assert_eq!(child["agent_name"], "b");
    assert_eq!(child["root_conversation_id"], env.a_conv.as_str());
    assert_eq!(child["status"], "succeeded");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 2. 并行派出两个 sleep，总耗时明显小于串行。
#[tokio::test]
async fn a14_parallel_calls() {
    let env = a14_pair("par").await;
    let (st, cagent) = create_agent_with(&env.base, &env.user, &env.host2, "c", a14_args()).await;
    assert_eq!(st, 200, "{cagent}");

    let mut sa = a14_user_ws(&env).await;
    let t0 = std::time::Instant::now();
    send_ws(&mut sa, &env.a_conv, "p1", "callasync:b,c:sleep:3000").await;
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("done"),
        A01Dur::from_secs(30),
    )
    .await;
    let elapsed = t0.elapsed();
    assert_eq!(reply, "done|done", "{reply}");
    assert!(
        elapsed < A01Dur::from_millis(8000),
        "并行两次 3s 应明显短于串行（6s+），实际 {elapsed:?}"
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

fn ctx_of(reply: &str) -> String {
    let rest = reply.split("ctx=").nth(1).unwrap_or("");
    rest.split_whitespace().next().unwrap_or("").to_string()
}

/// 3. 带上下文号接着聊；新开的上下文 recall 是空的。
#[tokio::test]
async fn a14_context_resume() {
    let env = a14_pair("ctx").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "k1", "callctx:b:new:echo:one").await;
    let first = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.starts_with("ctx=") && c.contains("result=one"),
        A01Dur::from_secs(45),
    )
    .await;
    let ctx = ctx_of(&first);
    assert!(!ctx.is_empty(), "{first}");

    send_ws(
        &mut sa,
        &env.a_conv,
        "k2",
        &format!("callctx:b:{ctx}:recall"),
    )
    .await;
    let second = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("result=") && c.contains("echo:one"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(second.contains(&format!("ctx={ctx}")), "{second}");

    send_ws(&mut sa, &env.a_conv, "k3", "callctx:b:new:recall").await;
    let third = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("result=(empty)"),
        A01Dur::from_secs(45),
    )
    .await;
    assert_ne!(ctx_of(&third), ctx, "新开的上下文号不应和上次相同: {third}");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

async fn put_agent_settings(base: &str, u: &TestUser, body: Value) -> (u16, Value) {
    let r = http()
        .put(format!("{base}/v1/users/me/agent-settings"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

/// 4. 深度上限 1：a→b 可以，b→c 被 400，b 的回复里能看到工具错误。
#[tokio::test]
async fn a14_depth_limit() {
    let env = a14_pair("dep").await;
    let (st, cagent) = create_agent_with(&env.base, &env.user, &env.host2, "c", a14_args()).await;
    assert_eq!(st, 200, "{cagent}");
    let c_conv = cagent["conversation_id"].as_str().unwrap().to_string();

    let settings: Value = http()
        .get(format!("{}/v1/users/me/agent-settings", env.base))
        .header("authorization", authz(&env.user.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["max_call_depth"], 3);
    let (st, body) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 0})).await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"], "bad_max_call_depth");
    let (st, _) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 11})).await;
    assert_eq!(st, 400);
    let (st, body) = put_agent_settings(&env.base, &env.user, json!({"max_call_depth": 1})).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["max_call_depth"], 1);
    assert_eq!(body["output_mode"], "stream");

    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "d1", "call:b:call:c:echo:hi").await;
    let b_err = wait_agent_msg(
        &env.base,
        &env.user,
        &env.b_conv,
        |c| c.contains("depth_exceeded"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(b_err.contains("depth_exceeded"), "{b_err}");
    let c_rows = list_inv(&env.base, &env.user, &c_conv).await;
    assert!(
        c_rows.as_array().unwrap().is_empty(),
        "超深度不应创建 c 的调用: {c_rows}"
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 5. b 只读，写文件要审批；卡片在 b 的专属会话；允许后 a 拿到结果。
#[tokio::test]
async fn a14_callee_approval() {
    let env = a14_pair("appr").await;
    assert_eq!(
        http()
            .get(format!("{}/v1/agents", env.base))
            .header("authorization", authz(&env.user.access))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == env.b_id)
            .unwrap()["permission_tier"],
        "read_only"
    );
    let mut sa = a14_user_ws(&env).await;
    send_ws(
        &mut sa,
        &env.a_conv,
        "a1",
        "call:b:perm:edit:C:/outside/a14.txt",
    )
    .await;
    let card = wait_pending_approval(&env.base, &env.user, &env.b_conv).await;
    assert!(
        card["content"].as_str().unwrap_or("").contains("a14.txt"),
        "{card}"
    );
    let a_msgs = conv_messages(&env.base, &env.user, &env.a_conv).await;
    assert!(
        a_msgs.iter().all(|m| m["kind"] != "approval"),
        "审批卡片不应出现在 a 的会话: {a_msgs:?}"
    );
    let id = card["approval"]["id"].as_str().unwrap();
    let (st, body) = decide_approval(&env.base, &env.user, id, "allow").await;
    assert_eq!(st, 200, "{body}");
    let reply = wait_agent_msg(
        &env.base,
        &env.user,
        &env.a_conv,
        |c| c.contains("allowed:allow"),
        A01Dur::from_secs(45),
    )
    .await;
    assert!(reply.contains("allowed:allow"), "{reply}");
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 6. b 的宿主离线 → 子调用排队；a 还在等；宿主回来后 a 拿到结果。
#[tokio::test]
async fn a14_offline_callee_queues() {
    let A14Env {
        base,
        user,
        h1,
        h2,
        install2,
        b_id,
        a_conv,
        b_conv,
        ..
    } = a14_pair("off").await;
    h2.shutdown().await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(10);
    while agent_host_online(&base, &user, &b_id).await {
        assert!(std::time::Instant::now() < deadline, "宿主 2 没有离线");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &user).await)
        .await
        .unwrap();
    send_ws(&mut sa, &a_conv, "o1", "call:b:echo:from-offline").await;
    let queued = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let rows = list_inv(&base, &user, &b_conv).await;
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "queued")
        {
            break;
        }
        assert!(std::time::Instant::now() < queued, "子调用没有排队: {rows}");
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    let a_rows = list_inv(&base, &user, &a_conv).await;
    // A15：同一列表里还有 root 落在本会话的子调用，且按创建时间倒序排在前面。
    let parent = a_rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_name"] == "a" && r["parent_invocation_id"].is_null())
        .expect("父调用应还在 a 的会话里");
    assert_eq!(parent["status"], "running", "{a_rows}");

    let (h2, _) = spawn_host(&base, &user, install2).await;
    let reply = wait_agent_msg(
        &base,
        &user,
        &a_conv,
        |c| c == "from-offline",
        A01Dur::from_secs(45),
    )
    .await;
    assert_eq!(reply, "from-offline");
    h1.shutdown().await;
    h2.shutdown().await;
}

/// 7. 取消父调用，子调用级联 cancelled。
#[tokio::test]
async fn a14_cancel_cascades() {
    let env = a14_pair("can").await;
    let mut sa = a14_user_ws(&env).await;
    send_ws(&mut sa, &env.a_conv, "x1", "call:b:sleep:20000").await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let parent_id = loop {
        let rows = list_inv(&env.base, &env.user, &env.b_conv).await;
        let child = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["status"] == "running");
        if let Some(child) = child {
            break child["parent_invocation_id"].as_str().unwrap().to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "子调用没有跑起来: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    let (st, body) = cancel_inv(&env.base, &env.user, &parent_id).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "cancelled");
    let deadline = std::time::Instant::now() + A01Dur::from_secs(15);
    loop {
        let rows = list_inv(&env.base, &env.user, &env.b_conv).await;
        if rows
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "cancelled")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "子调用没有被级联取消: {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

/// 8. 查别人派出的子调用 404；调别人的 Agent / 不存在的名字 404。
#[tokio::test]
async fn a14_tool_scope_404() {
    let base = spawn_named_db(&format!("chat_it_a14nf_{}", uniq())).await;
    let owner = register(&base, &format!("a14n_{}", uniq())).await;
    let other = register(&base, &format!("a14o_{}", uniq())).await;
    let (mut sh_a, host_a) = spawn_raw_host(&base, &owner).await;
    let (_sh_b, host_b) = spawn_raw_host(&base, &owner).await;
    let (_sh_o, host_o) = spawn_raw_host(&base, &other).await;
    let (st, agent_a) = create_agent(&base, &owner, &host_a, "a").await;
    assert_eq!(st, 200, "{agent_a}");
    let (st, _) = create_agent(&base, &owner, &host_b, "b").await;
    assert_eq!(st, 200);
    let (st, _) = create_agent(&base, &other, &host_o, "foreign").await;
    assert_eq!(st, 200);
    let conv = agent_a["conversation_id"].as_str().unwrap();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &owner).await)
        .await
        .unwrap();
    send_ws(&mut su, conv, "t1", "echo:hold").await;
    let start1 = until_frame(&mut sh_a, "invocation_start", A01Dur::from_secs(10)).await;
    let token1 = start1
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap()["tools_token"]
        .as_str()
        .unwrap()
        .to_string();
    send_ws(&mut su, conv, "t2", "echo:hold2").await;
    let start2 = until_frame(&mut sh_a, "invocation_start", A01Dur::from_secs(10)).await;
    let token2 = start2
        .iter()
        .rev()
        .find(|f| f["type"] == "invocation_start")
        .unwrap()["tools_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(token1, token2);

    let auth1 = format!("Invocation {token1}");
    let call = |auth: &str, name: &str| {
        let base = base.clone();
        let auth = auth.to_string();
        let name = name.to_string();
        async move {
            http()
                .post(format!("{base}/v1/agent-tools/invocations"))
                .header("authorization", auth)
                .json(&json!({"agent_name": name, "prompt": "echo:x"}))
                .send()
                .await
                .unwrap()
        }
    };
    let r = call(&auth1, "foreign").await;
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "agent not found");
    let r = call(&auth1, "no-such").await;
    assert_eq!(r.status(), 404);
    let r = call(&auth1, "B").await;
    assert_eq!(r.status(), 200, "大小写不敏感，应能调到自己的 b");
    let created: Value = r.json().await.unwrap();
    let child = created["invocation_id"].as_str().unwrap();

    let r = http()
        .get(format!("{base}/v1/agent-tools/invocations/{child}?wait=0"))
        .header("authorization", format!("Invocation {token2}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "invocation not found"
    );

    let r = http()
        .get(format!("{base}/v1/agent-tools/invocations/{child}?wait=0"))
        .header("authorization", &auth1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let view: Value = r.json().await.unwrap();
    assert_eq!(view["invocation_id"], child);

    let r = http()
        .get(format!(
            "{base}/v1/agent-tools/invocations/{child}?wait=601"
        ))
        .header("authorization", &auth1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "bad_wait");
}

/// 9. 根会话（群）里的另一名成员收到子调用帧，带 parent_invocation_id 和 agent_name。
#[tokio::test]
async fn a14_root_members_see_child_frame() {
    let env = a14_pair("root").await;
    let friend = register(&env.base, &format!("a14f_{}", uniq())).await;
    befriend(&env.base, &env.user, &friend).await;
    let group: Value = http()
        .post(format!("{}/v1/conversations/group", env.base))
        .header("authorization", authz(&env.user.access))
        .json(&json!({"name": "a14", "member_ids": [friend.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = group["id"].as_str().unwrap().to_string();
    let (mut sf, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &friend).await)
        .await
        .unwrap();
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "g1",
            "content": "call:b:echo:hi",
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + A01Dur::from_secs(45);
    let mut seen = Vec::new();
    let hit = 'find: loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "群成员没收到子调用帧: {seen:?}");
        let frames = until_frame(&mut sf, "agent_invocation", left).await;
        for f in frames {
            if f["type"] != "agent_invocation" {
                continue;
            }
            seen.push(f.clone());
            let inv = &f["invocation"];
            if inv["agent_name"] == "b" && inv["parent_invocation_id"].is_string() {
                break 'find f;
            }
        }
    };
    assert_eq!(hit["invocation"]["agent_name"], "b");
    assert!(hit["invocation"]["parent_invocation_id"].is_string());
    assert_eq!(hit["invocation"]["root_conversation_id"], gid.as_str());
    assert_eq!(hit["invocation"]["conversation_id"], env.b_conv.as_str());
    let dump = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/wt/a14-frames.json");
    let _ = std::fs::create_dir_all(dump.parent().unwrap());
    let _ = std::fs::write(
        &dump,
        serde_json::to_string_pretty(&seen).unwrap_or_default(),
    );
    env.h1.shutdown().await;
    env.h2.shutdown().await;
}

// ===== Agent A15 =====
// 群里 @a → a 调 b。群成员的历史接口能看到子调用；成功帧只带前 2000 字预览。

fn a15_preview_empty(inv: &Value) -> bool {
    inv.get("result_preview")
        .map(|v| v.is_null())
        .unwrap_or(true)
}

/// 读到这一轮里子调用和它的父调用都 succeeded。返回期间收到的 agent_invocation 帧。
async fn a15_until_pair<S>(sock: &mut S, timeout: A01Dur) -> Vec<Value>
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等子调用终态超时: {seen:?}");
        let frames = until_frame(sock, "agent_invocation", left).await;
        for f in frames {
            if f["type"] == "agent_invocation" {
                seen.push(f);
            }
        }
        let parents: Vec<String> = seen
            .iter()
            .filter_map(|f| {
                let i = &f["invocation"];
                if i["agent_name"] == "b" && i["status"] == "succeeded" {
                    i["parent_invocation_id"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        let parent_done = seen.iter().any(|f| {
            let i = &f["invocation"];
            i["status"] == "succeeded" && parents.iter().any(|id| i["id"] == id.as_str())
        });
        if !parents.is_empty() && parent_done {
            return seen;
        }
    }
}

/// 群成员拉得到子调用；子调用成功帧的预览是正文，父调用和进行中帧的预览为空。
/// 超长回复（含一个 emoji）在帧和历史里都只留 2000 个字。
#[tokio::test]
async fn a15_group_child_preview() {
    let env = a14_pair("nest").await;
    let friend = register(&env.base, &format!("a15f_{}", uniq())).await;
    befriend(&env.base, &env.user, &friend).await;
    let stranger = register(&env.base, &format!("a15s_{}", uniq())).await;
    let group: Value = http()
        .post(format!("{}/v1/conversations/group", env.base))
        .header("authorization", authz(&env.user.access))
        .json(&json!({"name": "a15", "member_ids": [friend.id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = group["id"].as_str().unwrap().to_string();

    // 成员校验不变：群外的人、以及不是被调方专属会话成员的人，都进不去那条会话的列表。
    let denied = http()
        .get(format!(
            "{}/v1/conversations/{gid}/agent-invocations",
            env.base
        ))
        .header("authorization", authz(&stranger.access))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let denied_b = http()
        .get(format!(
            "{}/v1/conversations/{}/agent-invocations",
            env.base, env.b_conv
        ))
        .header("authorization", authz(&friend.access))
        .send()
        .await
        .unwrap();
    assert_eq!(denied_b.status(), 403);

    let (mut sf, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &friend).await)
        .await
        .unwrap();
    tokio::time::sleep(A01Dur::from_millis(300)).await;
    let (mut so, _) = tokio_tungstenite::connect_async(ws_url(&env.base, &env.user).await)
        .await
        .unwrap();
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "a15g1",
            "content": "call:b:echo:hi",
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let seen = a15_until_pair(&mut sf, A01Dur::from_secs(45)).await;
    for f in &seen {
        let inv = &f["invocation"];
        let child_ok = inv["parent_invocation_id"].is_string() && inv["status"] == "succeeded";
        if !child_ok {
            assert!(
                a15_preview_empty(inv),
                "非成功子调用的 result_preview 应为空: {inv}"
            );
        }
        assert!(
            inv.get("result_text").is_none(),
            "帧里不应出现完整 result_text: {inv}"
        );
    }
    let child = seen
        .iter()
        .find(|f| {
            let i = &f["invocation"];
            i["agent_name"] == "b" && i["status"] == "succeeded"
        })
        .expect("子调用成功帧")["invocation"]
        .clone();
    assert_eq!(child["result_preview"], "hi");
    let parent_id = child["parent_invocation_id"].as_str().unwrap();
    let parent = seen
        .iter()
        .find(|f| f["invocation"]["id"] == parent_id && f["invocation"]["status"] == "succeeded")
        .expect("父调用成功帧");
    assert!(
        a15_preview_empty(&parent["invocation"]),
        "非子调用的 result_preview 应为空: {}",
        parent["invocation"]
    );

    let rows = list_inv(&env.base, &friend, &gid).await;
    let listed = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == child["id"])
        .expect("群历史应包含子调用");
    assert_eq!(listed["parent_invocation_id"], parent_id);
    assert_eq!(listed["root_conversation_id"], gid.as_str());
    assert_eq!(listed["result_preview"], "hi");
    assert_eq!(listed["agent_name"], "b");
    let listed_parent = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == parent_id)
        .expect("群历史应包含父调用");
    assert!(a15_preview_empty(listed_parent), "{listed_parent}");

    // 边界：2001 个字（中间有一个 emoji）只留前 2000 个，不按字节截断。
    let mut long = "你".repeat(1998);
    long.push('😀');
    long.push('Y');
    long.push('Z');
    assert_eq!(long.chars().count(), 2001);
    so.send(WsMsg::Text(
        json!({
            "type": "send_message",
            "conversation_id": gid,
            "client_msg_id": "a15g2",
            "content": format!("call:b:echo:{long}"),
            "agent_targets": [env.a_id],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let seen_long = a15_until_pair(&mut sf, A01Dur::from_secs(45)).await;
    let long_child = seen_long
        .iter()
        .find(|f| {
            let i = &f["invocation"];
            i["agent_name"] == "b"
                && i["status"] == "succeeded"
                && i["result_preview"].as_str().unwrap_or("").chars().count() > 2
        })
        .expect("超长子调用成功帧")["invocation"]
        .clone();
    let preview = long_child["result_preview"].as_str().unwrap();
    assert_eq!(preview.chars().count(), 2000, "预览应按字截断");
    assert!(
        preview.ends_with('Y'),
        "第 2000 字应是 Y，而不是把 emoji 拆开"
    );
    assert!(!preview.ends_with('Z'), "第 2001 字不应进帧");
    assert!(preview.contains('😀'));
    let long_parent_id = long_child["parent_invocation_id"].as_str().unwrap();
    let long_parent = seen_long
        .iter()
        .find(|f| {
            f["invocation"]["id"] == long_parent_id && f["invocation"]["status"] == "succeeded"
        })
        .expect("超长调用的父调用帧");
    assert!(a15_preview_empty(&long_parent["invocation"]));

    let rows = list_inv(&env.base, &friend, &gid).await;
    let listed_long = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == long_child["id"])
        .expect("刷新后仍能看到超长子调用");
    assert_eq!(listed_long["result_preview"].as_str(), Some(preview));
    assert_eq!(listed_long["parent_invocation_id"], long_parent_id);

    env.h1.shutdown().await;
    env.h2.shutdown().await;
}
