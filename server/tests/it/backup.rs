use super::*;

// ===== V2 P4-1 =====
// 管理后台备份/恢复。恢复会清空整库、搬空 FILES_DIR：
// 全部测试用 spawn_backup_env（独立库 + 独立文件目录），不得用共享的 spawn/spawn_named_db。

/// spawn_named_state 的备份版：独立库 + 每次测试独立的 FILES_DIR。
pub(crate) async fn spawn_backup_env(
    tag: &str,
) -> (String, chat_server::state::SharedState, std::path::PathBuf) {
    let db_name = format!("chat_t_p41{tag}_{}", uniq());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_db_url())
        .await
        .unwrap();
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(&db_name)
    .fetch_one(&admin)
    .await
    .unwrap();
    if !exists {
        let _ = sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&admin)
            .await;
    }
    let url = match base_db_url().rfind('/') {
        Some(i) => format!("{}/{db_name}", &base_db_url()[..i]),
        None => format!("postgres://chat:chat@localhost:15432/{db_name}"),
    };
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let files_dir = std::env::temp_dir().join(format!("chat-it-p41-{tag}-{}", uniq()));
    let cfg = Config {
        database_url: url,
        jwt_secret: "test-secret".into(),
        bind: "127.0.0.1".into(),
        port: 0,
        app_dist: None,
        files_dir: files_dir.to_string_lossy().into(),
        upload_max_bytes: 25 * 1024 * 1024,
        trust_proxy: false,
    };
    std::fs::create_dir_all(&cfg.files_dir).unwrap();
    let state = AppState::new(pool, cfg.clone());
    let app = build_router(state.clone(), &cfg);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), state, files_dir)
}

/// 申请下载 ticket 并把备份文件完整拉回来
pub(crate) async fn download_backup(base: &str, token: &str, name: &str) -> Vec<u8> {
    let c = http();
    let r = c
        .post(format!("{base}/v1/admin/backups/{name}/download-ticket"))
        .header("authorization", authz(token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let url = r.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let dl = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl.status(), 200);
    let dispo = dl
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(dispo.starts_with("attachment"), "{dispo}");
    dl.bytes().await.unwrap().to_vec()
}

/// POST /admin/backups 并轮询到成品出现在列表里，返回备份名
pub(crate) async fn create_and_wait_backup(base: &str, token: &str) -> String {
    let c = http();
    let r = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let name = r.json::<Value>().await.unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(name.starts_with("chat-backup-") && name.ends_with(".tar.gz"));
    wait_n_backups(base, token, &name, 1).await;
    name
}

/// 轮询备份列表，直到含 name 的成品数量 >= want 或该 name 出现在列表里
async fn wait_n_backups(base: &str, token: &str, name: &str, want: usize) -> Value {
    let c = http();
    for _ in 0..120 {
        let list: Value = c
            .get(format!("{base}/v1/admin/backups"))
            .header("authorization", authz(token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let items = list["items"].as_array().unwrap();
        if items.len() >= want || items.iter().any(|i| i["name"] == name) {
            return list;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("backup {name} did not finish in time");
}

/// multipart 上传备份文件恢复；confirm=None 不带 confirm 字段
pub(crate) async fn restore_upload(
    base: &str,
    token: &str,
    bytes: Vec<u8>,
    confirm: Option<&str>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("backup.tar.gz".to_string())
        .mime_str("application/gzip")
        .unwrap();
    let mut form = reqwest::multipart::Form::new();
    if let Some(c) = confirm {
        form = form.text("confirm", c.to_string());
    }
    form = form.part("file", part);
    http()
        .post(format!("{base}/v1/admin/restore"))
        .header("authorization", authz(token))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// 解开 tar.gz → {条目名: 内容}（只留普通文件条目）
pub(crate) fn read_backup(bytes: &[u8]) -> std::collections::HashMap<String, Vec<u8>> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz);
    let mut out = std::collections::HashMap::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        if e.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let name = e.path().unwrap().to_string_lossy().replace('\\', "/");
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).unwrap();
        out.insert(name, buf);
    }
    out
}

/// 由 {条目名: 内容} 重打一个 tar.gz（测试篡改用）。
/// 名字直接写进 header 字节走 append：要的就是能塞 ../ 这类恶意条目名。
pub(crate) fn write_backup(entries: &std::collections::HashMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, data) in entries {
            assert!(name.len() < 100, "test entry name too long");
            let mut h = tar::Header::new_gnu();
            h.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }
    out
}

/// 原密码重新登录，返回新凭据
async fn login_again(base: &str, username: &str) -> TestUser {
    let r: Value = http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": username, "password": "secret123"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["access_token"].as_str().is_some(), "{r}");
    TestUser {
        id: r["user"]["id"].as_str().unwrap().into(),
        username: username.into(),
        access: r["access_token"].as_str().unwrap().into(),
        refresh: r["refresh_token"].as_str().unwrap().into(),
    }
}

/// 备份：造数据 → 生成 → 列表 → ticket 下载 → manifest 与文件条目齐全；
/// 生成中再次请求 409。
#[tokio::test]
async fn p41_backup_list_download_manifest() {
    let (base, _st, files_dir) = spawn_backup_env("bk").await;
    let c = http();
    let admin = register(&base, &format!("p41a_{}", uniq())).await; // 首个 = 管理员
    let bob = register(&base, &format!("p41b_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "m1", "content": "backup me"}),
    )
    .await;
    let att_body = b"attachment-bytes-p41".to_vec();
    let (st, att) = upload_att(
        &base,
        &conv,
        &admin.access,
        "doc.bin",
        "application/octet-stream",
        att_body.clone(),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    put_avatar(&base, &admin.access, png_bytes(), "image/png").await;

    // 备份 1：正常完成
    let name = create_and_wait_backup(&base, &admin.access).await;

    // 备份 2：开始后立刻再次请求 → 409
    let r = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let r2 = c
        .post(format!("{base}/v1/admin/backups"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 409, "生成中再次请求应 409");
    wait_n_backups(&base, &admin.access, &name, 2).await;

    // 下载回来解包校验
    let bytes = download_backup(&base, &admin.access, &name).await;
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "必须是 gzip");
    let entries = read_backup(&bytes);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["format"], 1);
    assert!(manifest["migration_version"].as_i64().unwrap() >= 1);
    assert!(manifest["files"].as_i64().unwrap() >= 2);
    assert!(manifest["files_bytes"].as_i64().unwrap() >= att_body.len() as i64);
    let tables = manifest["tables"].as_array().unwrap();
    let users = tables.iter().find(|t| t["name"] == "users").unwrap();
    assert_eq!(users["rows"], 2, "users 行数应为 2");
    assert!(tables.iter().any(|t| t["name"] == "messages"));
    // tar 里有附件文件与头像文件
    let att_key = format!("files/{att_id}");
    assert_eq!(
        entries.get(&att_key).map(|v| v.as_slice()),
        Some(att_body.as_slice()),
        "附件字节必须原样进备份"
    );
    assert!(
        entries.keys().any(|k| k.starts_with("files/avatars/")),
        "头像文件必须进备份"
    );
    assert!(entries.contains_key("db/users.csv"));
    // 物理备份文件存在
    assert!(files_dir.join(".backups").join(&name).exists());
}

/// 恢复：备份后改数据（新消息/删消息/改昵称/删附件文件）→ 上传恢复 →
/// 数据回到备份时刻、附件字节一致（PG sha256 复核）、头像可访问；
/// 恢复前的 access/refresh 全部失效，原密码可重新登录。
#[tokio::test]
async fn p41_restore_rolls_back_data_and_credentials() {
    let (base, st, files_dir) = spawn_backup_env("rs").await;
    let c = http();
    let admin = register(&base, &format!("p41c_{}", uniq())).await;
    let bob = register(&base, &format!("p41d_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m1 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "keep", "content": "keep me"}),
    )
    .await;
    let m1_id = m1["id"].as_str().unwrap().to_string();
    let att_body = b"restore-bytes-12345".to_vec();
    let (st_code, att) = upload_att(
        &base,
        &conv,
        &admin.access,
        "photo.bin",
        "application/octet-stream",
        att_body.clone(),
    )
    .await;
    assert_eq!(st_code, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    put_avatar(&base, &admin.access, png_bytes(), "image/png").await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let backup_bytes = download_backup(&base, &admin.access, &name).await;

    // 备份之后的改动：新消息 / 删旧消息 / 改昵称 / 删附件物理文件
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "after", "content": "after backup"}),
    )
    .await;
    let r = c
        .delete(format!("{base}/v1/messages/{m1_id}"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .patch(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"display_name": "Changed Name"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    std::fs::remove_file(files_dir.join(&att_id)).unwrap();
    assert!(!files_dir.join(&att_id).exists());

    // 恢复
    let r = restore_upload(&base, &admin.access, backup_bytes, Some("RESTORE")).await;
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    assert!(res["tables"].as_array().unwrap().len() >= 10, "{res}");
    assert!(res["files"].as_i64().unwrap() >= 2);

    // 恢复前的凭据全部失效
    for token in [&admin.access, &bob.access] {
        let r = c
            .get(format!("{base}/v1/users/me"))
            .header("authorization", authz(token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "恢复前 access token 应失效");
    }
    for rt in [&admin.refresh, &bob.refresh] {
        let r = c
            .post(format!("{base}/v1/auth/refresh"))
            .json(&json!({"refresh_token": rt}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "恢复前 refresh token 应失效");
    }

    // 原密码能重新登录
    let admin2 = login_again(&base, &admin.username).await;
    let _bob2 = login_again(&base, &bob.username).await;

    // 数据回到备份时刻：旧消息回来了、新消息不见了、昵称回滚
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin2.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let contents: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(contents.contains(&"keep me"), "{contents:?}");
    assert!(!contents.contains(&"after backup"), "{contents:?}");
    let me2 = me(&base, &admin2).await;
    assert_eq!(me2["display_name"], admin.username);

    // 附件：字节一致；PG sha256 复核「恢复文件 == 原始字节」
    let dl = c
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    let got = dl.bytes().await.unwrap().to_vec();
    assert_eq!(got, att_body);
    let (sha_got, sha_orig): (String, String) =
        sqlx::query_as("SELECT encode(sha256($1),'hex'), encode(sha256($2),'hex')")
            .bind(&got)
            .bind(&att_body)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(sha_got, sha_orig);

    // 头像可访问
    let av = c
        .get(format!("{base}/v1/users/{}/avatar", admin.id))
        .send()
        .await
        .unwrap();
    assert_eq!(av.status(), 200);
    assert_eq!(av.bytes().await.unwrap().as_ref(), png_bytes().as_slice());

    // 维护模式已退出：公共接口正常
    let info = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), 200);
}

/// confirm 缺失或不是 RESTORE → 400，数据不变。
#[tokio::test]
async fn p41_restore_requires_confirm() {
    let (base, _st, _files_dir) = spawn_backup_env("cf").await;
    let c = http();
    let admin = register(&base, &format!("p41e_{}", uniq())).await;
    let bob = register(&base, &format!("p41f_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "x", "content": "still here"}),
    )
    .await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    for confirm in [None, Some(""), Some("restore"), Some("RESTORE ")] {
        let r = restore_upload(&base, &admin.access, bytes.clone(), confirm).await;
        assert_eq!(r.status(), 400, "confirm={confirm:?} 应 400");
    }

    // 数据不变：消息仍在、旧 token 仍有效
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": admin.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "旧 refresh 应仍可用（未发生恢复）");
}

/// refresh token 重放：宽限期内重试旧令牌只回 401、会话照常；过了宽限期再用旧令牌 = 被盗，整个会话作废
#[tokio::test]
async fn refresh_token_reuse_revokes_session() {
    let (base, st) = spawn_named_state(&format!("chat_it_reuse_{}", uniq())).await;
    let u = register(&base, &format!("reuse_{}", uniq())).await;
    let refresh = |rt: String| {
        let base = base.clone();
        async move {
            let r = http()
                .post(format!("{base}/v1/auth/refresh"))
                .json(&json!({"refresh_token": rt}))
                .send()
                .await
                .unwrap();
            let code = r.status().as_u16();
            (code, r.json::<Value>().await.unwrap_or(json!(null)))
        }
    };
    let (code, b) = refresh(u.refresh.clone()).await;
    assert_eq!(code, 200, "{b}");
    let rt2 = b["refresh_token"].as_str().unwrap().to_string();

    // 宽限期内：旧令牌 401，但新令牌仍可用
    assert_eq!(refresh(u.refresh.clone()).await.0, 401);
    let (code, b) = refresh(rt2).await;
    assert_eq!(code, 200, "宽限期内的重试不应撤销会话: {b}");
    let rt3 = b["refresh_token"].as_str().unwrap().to_string();
    let access3 = b["access_token"].as_str().unwrap().to_string();

    // 过了宽限期再拿最早的令牌来刷新 → 会话作废，手里最新的令牌也一起失效
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() - interval '2 minutes' WHERE revoked_at IS NOT NULL")
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(refresh(u.refresh.clone()).await.0, 401);
    assert_eq!(refresh(rt3).await.0, 401, "重放后会话内的令牌都应失效");
    let me = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&access3))
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 401);
}

/// 上传读到一半连接出错：不留 .restore-upload-*.part
#[tokio::test]
async fn p41_aborted_restore_upload_leaves_no_part_file() {
    let (base, _st, files_dir) = spawn_backup_env("tr").await;
    let admin = register(&base, &format!("p41t_{}", uniq())).await;
    let head =
        "--XB\r\nContent-Disposition: form-data; name=\"file\"; filename=\"b.tar.gz\"\r\n\r\n";
    // 先发字段头和一段数据，服务端开始写 .part；随后流出错，连接中断
    let chunks = futures_util::stream::unfold(0, move |i| async move {
        match i {
            0 => Some((
                Ok::<_, std::io::Error>(format!("{head}{}", "x".repeat(64 * 1024))),
                1,
            )),
            1 => {
                tokio::time::sleep(A01Dur::from_millis(300)).await;
                Some((Err(std::io::Error::other("client aborted")), 2))
            }
            _ => None,
        }
    });
    let parts = || -> Vec<String> {
        std::fs::read_dir(&files_dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".restore-upload-"))
            .collect()
    };
    let r = http()
        .post(format!("{base}/v1/admin/restore"))
        .header("authorization", authz(&admin.access))
        .header("content-type", "multipart/form-data; boundary=XB")
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await;
    // 连接中断：客户端拿到错误，或服务端来得及回 400
    assert!(r.is_err() || r.unwrap().status() == 400);
    let deadline = std::time::Instant::now() + A01Dur::from_secs(3);
    while !parts().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(A01Dur::from_millis(50)).await;
    }
    assert!(parts().is_empty(), "残留上传文件: {:?}", parts());
}

/// 篡改 manifest 的 migration_version → 400「版本不一致」，数据不变。
#[tokio::test]
async fn p41_restore_rejects_version_mismatch() {
    let (base, _st, _files_dir) = spawn_backup_env("vm").await;
    let c = http();
    let admin = register(&base, &format!("p41g_{}", uniq())).await;
    let bob = register(&base, &format!("p41h_{}", uniq())).await;
    befriend(&base, &admin, &bob).await;
    let conv = dm_conv(&base, &admin).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "x", "content": "still here"}),
    )
    .await;

    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    let mut entries = read_backup(&bytes);
    let mut manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    manifest["migration_version"] = json!(manifest["migration_version"].as_i64().unwrap() + 999);
    entries.insert(
        "manifest.json".into(),
        serde_json::to_vec(&manifest).unwrap(),
    );
    let bad = write_backup(&entries);
    let r = restore_upload(&base, &admin.access, bad, Some("RESTORE")).await;
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("版本") || err.contains("不一致"), "{body}");

    // 数据不变
    let page: Value = c
        .get(format!("{base}/v1/conversations/{conv}/messages"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
}

/// tar 里放 ../evil 或绝对路径条目 → 400，FILES_DIR 外不出现新文件。
#[tokio::test]
async fn p41_restore_rejects_unsafe_paths() {
    let (base, _st, files_dir) = spawn_backup_env("ev").await;
    let c = http();
    let admin = register(&base, &format!("p41i_{}", uniq())).await;
    let _bob = register(&base, &format!("p41j_{}", uniq())).await;
    let name = create_and_wait_backup(&base, &admin.access).await;
    let bytes = download_backup(&base, &admin.access, &name).await;

    for evil in ["../evil-p41.txt", "/abs-p41.txt"] {
        let mut entries = read_backup(&bytes);
        entries.insert(evil.into(), b"pwned".to_vec());
        let bad = write_backup(&entries);
        let r = restore_upload(&base, &admin.access, bad, Some("RESTORE")).await;
        assert_eq!(r.status(), 400, "evil entry {evil} 应 400");
    }
    // FILES_DIR 之外（父目录 = 系统临时目录）没有出现新文件
    assert!(!files_dir.parent().unwrap().join("evil-p41.txt").exists());
    assert!(!files_dir.join("abs-p41.txt").exists());
    // 普通接口不受影响（旧 token 仍有效）
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// 非管理员调用各接口 403；ticket 重复使用 401、过期 401；非法备份名 400。
#[tokio::test]
async fn p41_backup_admin_gate_and_ticket_rules() {
    let (base, st, _files_dir) = spawn_backup_env("gt").await;
    let c = http();
    let admin = register(&base, &format!("p41k_{}", uniq())).await;
    let user = register(&base, &format!("p41l_{}", uniq())).await;
    let name = create_and_wait_backup(&base, &admin.access).await;

    // 非管理员 → 403（restore 得带合法 multipart 才能越过 body 解析到达鉴权）
    for (method, path) in [
        ("GET", "/admin/backups".to_string()),
        ("POST", "/admin/backups".to_string()),
        ("POST", format!("/admin/backups/{name}/download-ticket")),
        ("DELETE", format!("/admin/backups/{name}")),
    ] {
        let req = match method {
            "GET" => c.get(format!("{base}/v1{path}")),
            "DELETE" => c.delete(format!("{base}/v1{path}")),
            _ => c.post(format!("{base}/v1{path}")),
        };
        let r = req
            .header("authorization", authz(&user.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{method} {path}");
    }
    let r = restore_upload(&base, &user.access, b"x".to_vec(), Some("RESTORE")).await;
    assert_eq!(r.status(), 403, "POST /admin/restore");

    // 非法备份名 → 400（防路径穿越）
    for bad in [
        "x",
        "chat-backup-2026.tar.gz",
        "foo",
        "chat-backup-20260101T000000.tar.gz",
        "chat-backup-20260101T000000Z.tar.gx",
    ] {
        let r = c
            .post(format!("{base}/v1/admin/backups/{bad}/download-ticket"))
            .header("authorization", authz(&admin.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "ticket {bad}");
        let r = c
            .delete(format!("{base}/v1/admin/backups/{bad}"))
            .header("authorization", authz(&admin.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "DELETE {bad}");
    }

    // ticket 一次性：第二次复用 → 401
    let r = c
        .post(format!("{base}/v1/admin/backups/{name}/download-ticket"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    let url = r.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let dl = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl.status(), 200);
    let dl2 = c.get(format!("{base}{url}")).send().await.unwrap();
    assert_eq!(dl2.status(), 401, "ticket 复用应 401");

    // 过期 ticket → 401（直接插入一张已超期的授权）
    let admin_uid = uuid::Uuid::parse_str(&admin.id).unwrap();
    st.tickets.insert(
        "deadbeef".into(),
        chat_server::state::TicketGrant {
            uid: admin_uid,
            session_version: 0,
            sid: None,
            issued: std::time::Instant::now() - std::time::Duration::from_secs(120),
        },
    );
    let dl3 = c
        .get(format!(
            "{base}/v1/admin/backups/{name}/download?t=deadbeef"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl3.status(), 401, "过期 ticket 应 401");

    // 普通用户的 WS ticket 不能用来下载备份（消费时仍查管理员身份）
    let t: Value = c
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&user.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ws_ticket = t["ticket"].as_str().unwrap();
    let dl4 = c
        .get(format!(
            "{base}/v1/admin/backups/{name}/download?t={ws_ticket}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl4.status(), 401, "非管理员 ticket 应 401");
}

/// 维护模式：maintenance=true 时所有 /v1 请求 503（含 WS 升级）；复位后恢复。
#[tokio::test]
async fn p41_maintenance_blocks_api() {
    let (base, st, _files_dir) = spawn_backup_env("mt").await;
    let c = http();
    let u = register(&base, &format!("p41m_{}", uniq())).await;

    st.maintenance
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for path in ["/users/me", "/conversations", "/ws/ticket"] {
        let r = c
            .get(format!("{base}/v1{path}"))
            .header("authorization", authz(&u.access))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503, "{path} 应 503");
    }
    // WS 升级同样被拒
    let r = c
        .get(format!("{base}/v1/ws?ticket=x"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);

    st.maintenance
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}
