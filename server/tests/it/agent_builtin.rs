use super::*;

// ===== Agent A28 =====
// 内置 Agent：管理员在 server_settings 配 builtin_endpoints（api_key 只写不读）；
// runtime 第三种形状 {"builtin": true}；快照按契约带 builtin.endpoints（含 key）；
// GET /v1/agent-builtin/models 给有使用权的用户列「端点id/模型」选项。

/// POST /v1/agents 的便捷变体：runtime 用任意 JSON（内置形状或非法形状）。
async fn a28_create(
    base: &str,
    u: &TestUser,
    host_id: &str,
    name: &str,
    runtime: Value,
) -> (u16, Value) {
    let r = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&u.access))
        .json(&json!({"name": name, "host_id": host_id, "runtime": runtime, "cwd": ""}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

/// 往专属会话发一条消息，收回该次调用的 invocation_start 帧。
async fn a28_invocation_start(sh: &mut RawHostWs, su: &mut RawHostWs, conv: &str) -> Value {
    send_ws(su, conv, &format!("a28-{}", uniq()), "hi").await;
    until_frame(sh, "invocation_start", A01Dur::from_secs(10))
        .await
        .into_iter()
        .find(|f| f["type"] == "invocation_start")
        .expect("应收到 invocation_start")
}

/// 1. 管理员 PUT 两个端点 → GET 不带 api_key、api_key_set 正确，地址末尾 / 去掉；
///    再 PUT 不带 api_key → 原 key 保留（从下发给宿主的调用快照断言）；带空串 → 清空。
#[tokio::test]
async fn a28_builtin_endpoints_roundtrip() {
    let base = spawn_named_db(&format!("chat_it_a28rt_{}", uniq())).await;
    let admin = register(&base, &format!("a28ra_{}", uniq())).await;
    let a = register(&base, &format!("a28ru_{}", uniq())).await;

    let ep = |name: &str, protocol: &str, url: &str, key: &str, models: Vec<&str>| {
        json!({"name": name, "protocol": protocol, "base_url": url,
               "api_key": key, "models": models})
    };
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"builtin_endpoints": [
            ep("DeepSeek", "chat", "https://api.deepseek.com/v1/", "sk-deep",
               vec!["deepseek-chat", "deepseek-reasoner"]),
            ep("Local", "responses", "http://127.0.0.1:9000", "", vec!["m1"]),
        ]}),
    )
    .await;
    assert_eq!(r.status(), 200);

    let s = admin_get_settings(&base, &admin).await;
    let eps = s["builtin_endpoints"]
        .as_array()
        .expect("应有 builtin_endpoints");
    assert_eq!(eps.len(), 2, "{s}");
    assert_eq!(eps[0]["name"], "DeepSeek");
    assert_eq!(eps[0]["protocol"], "chat");
    assert_eq!(eps[0]["base_url"], "https://api.deepseek.com/v1");
    assert_eq!(eps[0]["api_key_set"], true);
    assert!(
        eps[0].get("api_key").is_none(),
        "GET 不得回显 api_key: {eps:?}"
    );
    assert_eq!(
        eps[0]["models"],
        json!(["deepseek-chat", "deepseek-reasoner"])
    );
    assert_eq!(eps[1]["api_key_set"], false);
    let ep_id = eps[0]["id"].as_str().unwrap().to_string();
    assert!(
        uuid::Uuid::parse_str(&ep_id).is_ok(),
        "id 应是 uuid: {ep_id}"
    );

    // 再 PUT：同一个 id 不带 api_key → 保留原 key；顺带改名 / 改地址，id 不变。
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"builtin_endpoints": [
            {"id": ep_id, "name": "DeepSeek2", "protocol": "chat",
             "base_url": "https://api.deepseek.com/v2", "models": ["deepseek-chat"]},
        ]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let s = admin_get_settings(&base, &admin).await;
    let eps = s["builtin_endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0]["id"], ep_id);
    assert_eq!(eps[0]["name"], "DeepSeek2");
    assert_eq!(eps[0]["api_key_set"], true);

    // key 是否真的保住：看下发给宿主的调用快照
    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = a28_create(&base, &a, &host_id, "b28k", json!({"builtin": true})).await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let start = a28_invocation_start(&mut sh, &mut su, &conv).await;
    let snap_eps = start["agent"]["builtin"]["endpoints"]
        .as_array()
        .expect("快照应带 builtin.endpoints");
    assert_eq!(snap_eps.len(), 1);
    assert_eq!(snap_eps[0]["name"], "DeepSeek2");
    assert_eq!(
        snap_eps[0]["api_key"], "sk-deep",
        "没带 api_key 的 PUT 应保留原 key"
    );

    // 带空串 → 清空
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"builtin_endpoints": [
            {"id": ep_id, "name": "DeepSeek2", "protocol": "chat",
             "base_url": "https://api.deepseek.com/v2", "api_key": "",
             "models": ["deepseek-chat"]},
        ]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let s = admin_get_settings(&base, &admin).await;
    assert_eq!(s["builtin_endpoints"][0]["api_key_set"], false);
    let start = a28_invocation_start(&mut sh, &mut su, &conv).await;
    assert_eq!(
        start["agent"]["builtin"]["endpoints"][0]["api_key"], "",
        "清空后快照里的 api_key 应为空串"
    );
}

/// 2. 非法配置 400 bad_builtin_endpoints：协议非法、地址非 http(s)、模型为空、
///    名称为空、超过 20 个端点。普通用户 PUT 403。
#[tokio::test]
async fn a28_builtin_endpoints_validation() {
    let base = spawn_named_db(&format!("chat_it_a28val_{}", uniq())).await;
    let admin = register(&base, &format!("a28va_{}", uniq())).await;
    let u = register(&base, &format!("a28vu_{}", uniq())).await;

    // 普通用户 → 403
    let r = admin_put_settings(&base, &u, json!({"builtin_endpoints": []})).await;
    assert_eq!(r.status(), 403);

    let ok_ep = json!({"name":"E","protocol":"chat","base_url":"https://e.com","models":["m"]});
    let cases = vec![
        json!({"builtin_endpoints": [{"name":"E","protocol":"grpc","base_url":"https://e.com","models":["m"]}]}),
        json!({"builtin_endpoints": [{"name":"E","protocol":"chat","base_url":"ftp://e.com","models":["m"]}]}),
        json!({"builtin_endpoints": [{"name":"E","protocol":"chat","base_url":"https://e.com","models":[]}]}),
        json!({"builtin_endpoints": [{"name":"","protocol":"chat","base_url":"https://e.com","models":["m"]}]}),
        json!({"builtin_endpoints": [{"name":"E","protocol":"chat","base_url":"https://e.com","models":["m"],"api_key": "x".repeat(501)}]}),
        json!({"builtin_endpoints": vec![ok_ep.clone(); 21]}),
        // 不是数组
        json!({"builtin_endpoints": {"name": "E"}}),
    ];
    for (i, body) in cases.iter().enumerate() {
        let r = admin_put_settings(&base, &admin, body.clone()).await;
        let v: Value = r.json().await.unwrap_or(json!(null));
        assert_eq!(
            v["error"], "bad_builtin_endpoints",
            "case {i} 应 400 bad_builtin_endpoints: {v}"
        );
    }
    // 上一个断言里合法的空数组也应能清掉配置
    let r = admin_put_settings(&base, &admin, json!({"builtin_endpoints": []})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        admin_get_settings(&base, &admin).await["builtin_endpoints"],
        json!([])
    );
}

/// 3. runtime {"builtin": true} 创建成功；{"builtin": false} → 400 bad_runtime。
#[tokio::test]
async fn a28_builtin_agent_runtime() {
    let base = spawn_named_db(&format!("chat_it_a28ag_{}", uniq())).await;
    let a = register(&base, &format!("a28ga_{}", uniq())).await;
    let (sh, host_id) = spawn_raw_host(&base, &a).await;

    let (st, agent) = a28_create(&base, &a, &host_id, "b28a", json!({"builtin": true})).await;
    assert_eq!(st, 200, "{agent}");
    assert_eq!(agent["runtime"], json!({"builtin": true}));

    let (st, body) = a28_create(&base, &a, &host_id, "b28b", json!({"builtin": false})).await;
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["error"], "bad_runtime");

    // 内置 Agent 的 PATCH runtime 也接受同一形状
    let agent_id = agent["id"].as_str().unwrap();
    let r = http()
        .patch(format!("{base}/v1/agents/{agent_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"runtime": {"builtin": false}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    drop(sh);
}

/// 4. invocation_start / 快照：内置 Agent 的 agent 带 builtin.endpoints（含 api_key）；
///    命令形状的 Agent 快照没有 builtin 字段。
#[tokio::test]
async fn a28_snapshot_builtin_field() {
    let base = spawn_named_db(&format!("chat_it_a28sn_{}", uniq())).await;
    let admin = register(&base, &format!("a28sa_{}", uniq())).await;
    let a = register(&base, &format!("a28su_{}", uniq())).await;
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"builtin_endpoints": [
            {"name":"DeepSeek","protocol":"chat","base_url":"https://api.deepseek.com/v1",
             "api_key":"sk-x","models":["deepseek-chat"]},
        ]}),
    )
    .await;
    assert_eq!(r.status(), 200);

    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, builtin) = a28_create(&base, &a, &host_id, "b28s", json!({"builtin": true})).await;
    assert_eq!(st, 200, "{builtin}");
    let (_, cmd) = create_agent(&base, &a, &host_id, "c28s").await;
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let start = a28_invocation_start(
        &mut sh,
        &mut su,
        builtin["conversation_id"].as_str().unwrap(),
    )
    .await;
    let eps = start["agent"]["builtin"]["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0]["api_key"], "sk-x");
    assert_eq!(eps[0]["models"], json!(["deepseek-chat"]));
    assert_eq!(start["agent"]["runtime"], json!({"builtin": true}));

    let start =
        a28_invocation_start(&mut sh, &mut su, cmd["conversation_id"].as_str().unwrap()).await;
    assert!(
        start["agent"].get("builtin").is_none(),
        "命令形状 Agent 的快照不应有 builtin: {}",
        start["agent"]
    );
}

/// 5. GET /v1/agent-builtin/models：端点×模型的扁平选项（id = 端点id/模型），
///    不带 base_url / api_key；无 Agent 使用权的用户 403。
#[tokio::test]
async fn a28_builtin_models_endpoint() {
    let base = spawn_named_db(&format!("chat_it_a28md_{}", uniq())).await;
    let admin = register(&base, &format!("a28ma_{}", uniq())).await;
    let u = register(&base, &format!("a28mu_{}", uniq())).await;
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"builtin_endpoints": [
            {"name":"DeepSeek","protocol":"chat","base_url":"https://api.deepseek.com/v1",
             "api_key":"sk-x","models":["deepseek-chat","deepseek-reasoner"]},
            {"name":"Local","protocol":"responses","base_url":"http://127.0.0.1:9000",
             "models":["org/m1"]},
        ]}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let s = admin_get_settings(&base, &admin).await;
    let id0 = s["builtin_endpoints"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id1 = s["builtin_endpoints"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let models = |u: &TestUser| {
        let base = base.clone();
        let access = u.access.clone();
        async move {
            http()
                .get(format!("{base}/v1/agent-builtin/models"))
                .header("authorization", authz(&access))
                .send()
                .await
                .unwrap()
        }
    };
    let r = models(&u).await;
    assert_eq!(r.status(), 200);
    let list = r.json::<Value>().await.unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 3, "{list}");
    assert_eq!(arr[0]["id"], format!("{id0}/deepseek-chat"));
    assert_eq!(arr[0]["name"], "DeepSeek · deepseek-chat");
    // 模型名里还可以再有 /
    assert_eq!(arr[2]["id"], format!("{id1}/org/m1"));
    assert_eq!(arr[2]["name"], "Local · org/m1");
    for item in arr {
        assert!(item.get("base_url").is_none() && item.get("api_key").is_none());
    }

    // 收回默认使用权 → 普通用户 403；管理员不受影响
    let r = admin_put_settings(&base, &admin, json!({"agent_default_allowed": false})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(models(&u).await.status(), 403);
    assert_eq!(models(&admin).await.status(), 200);
}

/// 假 Gitea：一个 Release（tag v9.9.9），带 Windows 安装包、签名和 APK
async fn fake_gitea() -> String {
    use axum::routing::get;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let b = base.clone();
    let app = axum::Router::new()
        .route(
            "/api/v1/repos/o/r/releases/latest",
            get(move || {
                let b = b.clone();
                async move {
                    axum::Json(json!({
                        "tag_name": "v9.9.9",
                        "body": "修了一些问题",
                        "published_at": "2026-09-30T00:00:00Z",
                        "assets": [
                            {"name": "Chat_9.9.9_x64-setup.exe", "size": 5, "browser_download_url": format!("{b}/dl/exe")},
                            {"name": "Chat_9.9.9_x64-setup.exe.sig", "size": 3, "browser_download_url": format!("{b}/dl/sig")},
                            {"name": "Chat_9.9.9.apk", "size": 3, "browser_download_url": format!("{b}/dl/apk")},
                            {"name": "Chat_9.9.9_amd64.deb", "size": 3, "browser_download_url": format!("{b}/dl/apk")},
                            {"name": "Chat_9.9.9_amd64.deb.sig", "size": 3, "browser_download_url": format!("{b}/dl/sig")},
                        ]
                    }))
                }
            }),
        )
        .route("/dl/exe", get(|| async { "EXE!!" }))
        .route("/dl/sig", get(|| async { "SIG\n" }))
        .route("/dl/apk", get(|| async { "APK" }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    base
}

#[tokio::test]
async fn client_update_from_gitea_release() {
    let (base, _st) = spawn_named_state(&format!("chat_it_upd_{}", uniq())).await;
    let admin = register(&base, &format!("updadm_{}", uniq())).await;
    let c = http();

    // 没配置仓库 → 404；Windows 更新检查同样 404（插件当作检查失败，不会误装）
    let r = c
        .get(format!("{base}/v1/client/latest"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // 不是仓库地址 → 400
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"update_repo_url": "http://g.example.com/x"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    let gitea = fake_gitea().await;
    let r = admin_put_settings(
        &base,
        &admin,
        json!({"update_repo_url": format!("{gitea}/o/r/")}),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        admin_get_settings(&base, &admin).await["update_repo_url"],
        format!("{gitea}/o/r")
    );

    let v: Value = c
        .get(format!("{base}/v1/client/latest"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["version"], "9.9.9");
    assert_eq!(v["notes"], "修了一些问题");
    assert_eq!(v["assets"].as_array().unwrap().len(), 5);

    // Tauri 动态更新格式：下载地址指回本服务，签名去掉末尾换行
    let u: Value = c
        .get(format!("{base}/v1/client/update/windows/x86_64/0.1.0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(u["version"], "9.9.9");
    assert_eq!(u["signature"], "SIG");
    assert_eq!(
        u["url"],
        format!("{base}/v1/client/download/Chat_9.9.9_x64-setup.exe")
    );

    // Linux 按安装方式挑：deb 有包；AppImage 这个 Release 里没有 → 204；mac 不支持 → 204
    let u2: Value = c
        .get(format!(
            "{base}/v1/client/update/linux/x86_64/0.1.0?bundle=deb"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        u2["url"],
        format!("{base}/v1/client/download/Chat_9.9.9_amd64.deb")
    );
    for q in [
        "linux/x86_64/0.1.0?bundle=appimage",
        "darwin/aarch64/0.1.0?bundle=app",
    ] {
        let r = c
            .get(format!("{base}/v1/client/update/{q}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204, "{q}");
    }

    // 下载经服务端中转
    let r = c.get(u["url"].as_str().unwrap()).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "EXE!!");
    let r = c
        .get(format!("{base}/v1/client/download/nope.exe"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // 清空 = 关闭
    let r = admin_put_settings(&base, &admin, json!({"update_repo_url": ""})).await;
    assert_eq!(r.status(), 200);
    let r = c
        .get(format!("{base}/v1/client/latest"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
