use super::*;

// ===== Agent A06 =====

pub(crate) async fn patch_agent(
    base: &str,
    u: &TestUser,
    id: &str,
    body: Value,
) -> reqwest::Response {
    http()
        .patch(format!("{base}/v1/agents/{id}"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

pub(crate) async fn list_my_agents(base: &str, u: &TestUser) -> Value {
    http()
        .get(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub(crate) fn agent_named<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("列表里没有 {name}: {list}"))
}

/// PATCH 各字段回读可见；非法值 400；host_id 400；重名 409；别人 404。
#[tokio::test]
async fn a06_patch_fields_and_rejections() {
    let base = spawn_named_db(&format!("chat_t_a06p_{}", uniq())).await;
    let a = register(&base, &format!("a06a_{}", uniq())).await;
    let b = register(&base, &format!("a06b_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, one) = create_agent(&base, &a, &host_id, "cfgA").await;
    let (_, two) = create_agent(&base, &a, &host_id, "cfgB").await;
    let id = one["id"].as_str().unwrap();

    let body = json!({
        "name": "cfgRenamed",
        "runtime": {"command": "node", "args": ["--one", "--two"]},
        "cwd": "C:/work",
        "extra_dirs": ["C:/extra", "D:/other"],
        "model": "smart",
        "mode": "code",
        "instructions": "先看 README",
        "env": {"FAKE_ECHO": "sekrit", "OTHER": "1"},
        "output_mode": "status",
        "queue_timeout_secs": 600,
    });
    let patched: Value = patch_agent(&base, &a, id, body).await.json().await.unwrap();
    assert_eq!(patched["name"], "cfgRenamed");
    assert_eq!(patched["runtime"]["args"][1], "--two");
    assert_eq!(patched["cwd"], "C:/work");
    assert_eq!(patched["extra_dirs"][0], "C:/extra");
    assert_eq!(patched["model"], "smart");
    assert_eq!(patched["mode"], "code");
    assert_eq!(patched["instructions"], "先看 README");
    assert_eq!(patched["env"]["FAKE_ECHO"], "sekrit");
    assert_eq!(patched["output_mode"], "status");
    assert_eq!(patched["deleted"], false);
    let list = list_my_agents(&base, &a).await;
    let listed = agent_named(&list, "cfgRenamed");
    assert_eq!(listed["env"]["OTHER"], "1");
    assert_eq!(listed["instructions"], "先看 README");

    // 缺省字段不动：只改 model 不应清掉 instructions
    let only: Value = patch_agent(&base, &a, id, json!({"model": "fast"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(only["model"], "fast");
    assert_eq!(only["instructions"], "先看 README");
    assert_eq!(only["mode"], "code");

    let mut too_many = serde_json::Map::new();
    for i in 0..51 {
        too_many.insert(format!("K{i}"), json!("v"));
    }
    for (body, code) in [
        (json!({"name": "bad name"}), "bad_name"),
        (
            json!({"instructions": "字".repeat(8001)}),
            "bad_instructions",
        ),
        (json!({"env": too_many}), "bad_env"),
        (json!({"env": {"A": 1}}), "bad_env"),
        (json!({"env": {"": "x"}}), "bad_env"),
        (
            json!({"extra_dirs": (0..11).map(|i| format!("d{i}")).collect::<Vec<_>>()}),
            "bad_extra_dirs",
        ),
        (json!({"extra_dirs": ["x".repeat(501)]}), "bad_extra_dirs"),
    ] {
        let r = patch_agent(&base, &a, id, body).await;
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap();
        assert_eq!(status, 400, "{code} → {v}");
        assert_eq!(v["error"], code);
    }
    let r = patch_agent(&base, &a, id, json!({"host_id": host_id})).await;
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "host_immutable");

    let r = patch_agent(&base, &a, id, json!({"name": "CFGB"})).await;
    assert_eq!(r.status().as_u16(), 409);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "name_taken");

    let other = two["id"].as_str().unwrap();
    let r = patch_agent(&base, &b, other, json!({"model": "smart"})).await;
    assert_eq!(r.status().as_u16(), 404);

    h.shutdown().await;
}

/// 改完配置再发 whoami：回复里是新的 mode / model / 环境变量 / 第一个文本块。
#[tokio::test]
async fn a06_whoami_uses_new_config() {
    let base = spawn_named_db(&format!("chat_t_a06w_{}", uniq())).await;
    let a = register(&base, &format!("a06w_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "who").await;
    let id = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let r = patch_agent(
        &base,
        &a,
        id,
        json!({
            "mode": "code",
            "model": "smart",
            "instructions": "INST-a06",
            "env": {"FAKE_ECHO": "sekrit"},
            "extra_dirs": ["C:/a06-extra"],
        }),
    )
    .await;
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert_eq!(status, 200, "{text}");

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "w1", "whoami").await;
    let mut frames = Vec::new();
    let mut reply = None;
    for _ in 0..6 {
        frames.extend(until_frame(&mut sa, "message", A01Dur::from_secs(20)).await);
        if let Some(m) = frames
            .iter()
            .rev()
            .find(|f| f["message"]["agent"].is_object())
        {
            reply = Some(m["message"].clone());
            break;
        }
    }
    let msg = reply.expect("应收到 Agent 回复");
    let text = msg["content"].as_str().unwrap();
    assert!(text.contains("mode=code"), "{text}");
    assert!(text.contains("model=smart"), "{text}");
    assert!(text.contains("FAKE_ECHO=sekrit"), "{text}");
    assert!(text.contains("first=INST-a06"), "{text}");
    assert!(text.contains("dirs=C:/a06-extra"), "{text}");
    assert_eq!(msg["agent"]["avatar_version"], 0);
    h.shutdown().await;
}

/// probe：在线时等到结果返回，reported_options 有两个 mode；宿主离线 409。
#[tokio::test]
async fn a06_probe_online_then_offline() {
    let base = spawn_named_db(&format!("chat_t_a06r_{}", uniq())).await;
    let a = register(&base, &format!("a06r_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "prober").await;
    let id = agent["id"].as_str().unwrap();

    let r = http()
        .post(format!("{base}/v1/agents/{id}/probe"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["modes"].as_array().map(|a| a.len()), Some(2), "{body}");
    assert!(body["error"].is_null(), "{body}");

    let mut modes = 0;
    for _ in 0..50 {
        tokio::time::sleep(A01Dur::from_millis(100)).await;
        let list = list_my_agents(&base, &a).await;
        modes = agent_named(&list, "prober")["reported_options"]["modes"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if modes == 2 {
            break;
        }
    }
    assert_eq!(modes, 2, "probe 后应有两个 mode");

    h.shutdown().await;
    let mut offline = false;
    for _ in 0..30 {
        tokio::time::sleep(A01Dur::from_millis(100)).await;
        let hosts: Value = http()
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if hosts[0]["online"] == false {
            offline = true;
            break;
        }
    }
    assert!(offline, "宿主应已离线");
    let r = http()
        .post(format!("{base}/v1/agents/{id}/probe"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "host_offline");
}

/// 创建时可直接带上模型和模式（添加页在本机读好可选项后一起提交）。
#[tokio::test]
async fn a06_create_with_model_and_mode() {
    let base = spawn_named_db(&format!("chat_t_a06p_{}", uniq())).await;
    let a = register(&base, &format!("a06p_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": "picked",
            "host_id": host_id,
            "runtime": {"command": "node", "args": fake_acp_args()},
            "model": "smart",
            "mode": "code",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let agent: Value = r.json().await.unwrap();
    assert_eq!(agent["model"], "smart");
    assert_eq!(agent["mode"], "code");
    h.shutdown().await;
}

/// 删除：发消息被拒且不产生调用；列表消失；会话 deleted；同名可重建；运行中的调用失败。
#[tokio::test]
async fn a06_delete_blocks_send_and_frees_name() {
    let base = spawn_named_db(&format!("chat_t_a06d_{}", uniq())).await;
    let a = register(&base, &format!("a06d_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "doomed").await;
    let id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "d1", "sleep:8000").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    assert_eq!(
        frames
            .iter()
            .rev()
            .find(|f| f["type"] == "agent_invocation")
            .unwrap()["invocation"]["status"],
        "running"
    );

    let r = http()
        .delete(format!("{base}/v1/agents/{id}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);

    let invs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invs.as_array().unwrap().len(), 1);
    assert_eq!(invs[0]["status"], "failed");
    assert_eq!(invs[0]["error"], "agent_deleted");

    let list = list_my_agents(&base, &a).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .all(|x| x["name"] != "doomed"));

    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv.as_str())
        .unwrap();
    assert_eq!(c["agent"]["deleted"], true);
    assert_eq!(c["agent"]["name"], "doomed");

    send_ws(&mut sa, &conv, "d2", "echo:nope").await;
    let frames = until_frame(&mut sa, "error", A01Dur::from_secs(5)).await;
    let err = frames.iter().rev().find(|f| f["type"] == "error").unwrap();
    assert_eq!(err["code"], "agent_deleted");
    let invs: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invs.as_array().unwrap().len(), 1, "拒绝的消息不应产生调用");

    let r = patch_agent(&base, &a, &id, json!({"model": "smart"})).await;
    assert_eq!(r.status().as_u16(), 404);

    let (st, again) = create_agent(&base, &a, &host_id, "doomed").await;
    assert_eq!(st, 200, "{again}");
    assert_ne!(again["id"].as_str().unwrap(), id.as_str());
    h.shutdown().await;
}

/// 头像：上传后版本变化且能取回；别人不能上传。
#[tokio::test]
async fn a06_avatar_upload_and_acl() {
    let base = spawn_named_db(&format!("chat_t_a06v_{}", uniq())).await;
    let a = register(&base, &format!("a06v_{}", uniq())).await;
    let b = register(&base, &format!("a06vb_{}", uniq())).await;
    let (h, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (_, agent) = create_agent(&base, &a, &host_id, "faced").await;
    let id = agent["id"].as_str().unwrap();
    assert_eq!(agent["avatar_version"], 0);

    let png = png_bytes();
    let up = http()
        .put(format!("{base}/v1/agents/{id}/avatar"))
        .header("authorization", authz(&a.access))
        .multipart(avatar_part(png.clone(), "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(up.status().as_u16(), 200);
    let view: Value = up.json().await.unwrap();
    assert_eq!(view["avatar_version"], 1);

    let cur = http()
        .get(format!("{base}/v1/agents/{id}/avatar?v=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(cur.status(), 200);
    assert_eq!(
        cur.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        cur.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(cur.bytes().await.unwrap().as_ref(), png.as_slice());

    let denied = http()
        .put(format!("{base}/v1/agents/{id}/avatar"))
        .header("authorization", authz(&b.access))
        .multipart(avatar_part(png, "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 404);
    h.shutdown().await;
}

// ===== Agent A26 =====

/// 添加 Agent 必须走这台电脑当前宿主连接所属的登录会话。
#[tokio::test]
async fn a26_create_agent_requires_local_host_session() {
    let base = spawn_named_db(&format!("chat_it_a26_{}", uniq())).await;
    let name = format!("a26_{}", uniq());
    let a = register(&base, &name).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;

    let (st, agent) = create_agent(&base, &a, &host_id, "localbot").await;
    assert_eq!(st, 200, "本机会话应能添加: {agent}");
    assert_eq!(agent["name"], "localbot");

    let other = login_device(&base, &name, "secret123", "phone").await;
    let (st, body) = create_agent(&base, &other, &host_id, "remote").await;
    assert_eq!(st, 403, "另一登录会话应拒绝: {body}");
    assert_eq!(body["error"], "host_not_local");

    let uid = uuid::Uuid::parse_str(&a.id).unwrap();
    let (tok, _) = chat_server::auth::mint_access(uid, 0, None, "test-secret").unwrap();
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&tok))
        .json(&json!({
            "name": "nosid",
            "host_id": host_id,
            "runtime": {"command": "node", "args": []},
            "cwd": "",
        }))
        .send()
        .await
        .unwrap();
    let no_sid = r.status().as_u16();
    let no_sid_body: Value = r.json().await.unwrap_or(json!(null));
    assert_eq!(no_sid, 403, "没有 sid 的 token 应拒绝: {no_sid_body}");
    assert_eq!(no_sid_body["error"], "host_not_local");

    ha.shutdown().await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(5);
    loop {
        let hosts: Value = http()
            .get(format!("{base}/v1/agent-hosts"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let online = hosts.as_array().unwrap().iter().any(|h| {
            h["id"].as_str() == Some(host_id.as_str()) && h["online"].as_bool() == Some(true)
        });
        if !online {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "电脑没有离线: {hosts}"
        );
        tokio::time::sleep(A01Dur::from_millis(50)).await;
    }
    let (st, body) = create_agent(&base, &a, &host_id, "later").await;
    assert_eq!(st, 403, "电脑离线应拒绝: {body}");
    assert_eq!(body["error"], "host_not_local");
}

/// 命令 / 目录 / 环境变量 / 调高档位只能在本机改；其余字段、原样回传、降档任何客户端都行。
#[tokio::test]
async fn a26_patch_exec_fields_requires_local_host_session() {
    let base = spawn_named_db(&format!("chat_it_a26p_{}", uniq())).await;
    let name = format!("a26p_{}", uniq());
    let a = register(&base, &name).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "guarded").await;
    assert_eq!(st, 200, "{agent}");
    let id = agent["id"].as_str().unwrap();
    let phone = login_device(&base, &name, "secret123", "phone").await;

    for body in [
        json!({"runtime": {"command": "powershell", "args": ["-c", "calc"]}}),
        json!({"env": {"NODE_OPTIONS": "--require evil.js"}}),
        json!({"cwd": "C:/elsewhere"}),
        json!({"extra_dirs": ["C:/"]}),
        json!({"permission_tier": "full"}),
    ] {
        let r = patch_agent(&base, &phone, id, body.clone()).await;
        assert_eq!(r.status().as_u16(), 403, "远程改 {body} 应拒绝");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"], "host_not_local");
    }

    // 手机整表单保存：执行字段原样带回、只改了 model → 放行
    let r = patch_agent(
        &base,
        &phone,
        id,
        json!({
            "runtime": agent["runtime"],
            "cwd": agent["cwd"],
            "extra_dirs": agent["extra_dirs"],
            "env": agent["env"],
            "permission_tier": agent["permission_tier"],
            "model": "fast",
        }),
    )
    .await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["model"], "fast");
    assert_eq!(v["runtime"], agent["runtime"], "被拒的改动不应落库");

    // 本机调高，远程可以降回去
    let r = patch_agent(&base, &a, id, json!({"permission_tier": "workspace"})).await;
    assert_eq!(r.status().as_u16(), 200);
    let r = patch_agent(&base, &phone, id, json!({"permission_tier": "read_only"})).await;
    assert_eq!(r.status().as_u16(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["permission_tier"], "read_only");
    ha.shutdown().await;
}
