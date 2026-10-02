use super::*;

// ===== V2 P2-2 =====

/// 最小合法 JPEG（文件头魔数 + SOI/APP0）。
fn jpeg_bytes() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

pub(crate) fn avatar_part(body: Vec<u8>, mime: &str) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(body)
        .file_name("avatar.bin")
        .mime_str(mime)
        .unwrap();
    reqwest::multipart::Form::new().part("file", part)
}

/// 头像用例用独立 FILES_DIR，避免并行测试互相删文件。
pub(crate) async fn spawn_avatar() -> (String, std::path::PathBuf) {
    ensure_test_db().await;
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let files_dir = std::env::temp_dir().join(format!("chat-it-avatar-{}", uniq()));
    let cfg = Config {
        database_url: test_db_url(),
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
    (format!("http://{addr}"), files_dir)
}

/// 上传 PNG：版本 0→1、has_avatar；当前版本 immutable，旧版本号 no-cache，字节一致。
#[tokio::test]
async fn p22_upload_png_and_cache_headers() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("av_{}", uniq())).await;
    let png = png_bytes();

    let me: Value = put_avatar(&base, &u.access, png.clone(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 1);
    assert_eq!(me["has_avatar"], true);
    assert_eq!(me["id"], u.id);

    let cur = http()
        .get(format!("{base}/v1/users/{}/avatar?v=1", u.id))
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
    assert_eq!(cur.headers().get("content-type").unwrap(), "image/png");
    assert_eq!(cur.bytes().await.unwrap().as_ref(), png.as_slice());

    let stale = http()
        .get(format!("{base}/v1/users/{}/avatar?v=0", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 200);
    assert_eq!(
        stale
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    assert_eq!(stale.bytes().await.unwrap().as_ref(), png.as_slice());
}

/// 再次上传升到版本 2，旧文件最终被删除。
#[tokio::test]
async fn p22_replace_deletes_old_file() {
    let (base, files_dir) = spawn_avatar().await;
    let u = register(&base, &format!("avr_{}", uniq())).await;
    let png = png_bytes();
    let jpeg = jpeg_bytes();

    let first: Value = put_avatar(&base, &u.access, png.clone(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first["avatar_version"], 1);

    let second: Value = put_avatar(&base, &u.access, jpeg.clone(), "image/jpeg")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(second["avatar_version"], 2);
    assert_eq!(second["has_avatar"], true);

    let cur = http()
        .get(format!("{base}/v1/users/{}/avatar?v=2", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(cur.status(), 200);
    assert_eq!(cur.headers().get("content-type").unwrap(), "image/jpeg");
    assert_eq!(cur.bytes().await.unwrap().as_ref(), jpeg.as_slice());

    // 提交后异步删旧文件。旧版本请求仍回当前图，但磁盘上不应再有 v1 文件。
    // 文件名 {uid}-{版本}-{随机段}.{扩展名}
    let dir = files_dir.join("avatars");
    let has = |prefix: String, ext: &str| {
        std::fs::read_dir(&dir).unwrap().flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&prefix) && n.ends_with(ext)
        })
    };
    let mut gone = false;
    for _ in 0..50 {
        if !has(format!("{}-1-", u.id), ".png") {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(gone, "旧头像文件应在更换后被删除");
    assert!(has(format!("{}-2-", u.id), ".jpg"));
}

/// 声称 image/png 但内容是 HTML，以及超过 1MB，都是 400。
#[tokio::test]
async fn p22_rejects_spoofed_type_and_oversize() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("avb_{}", uniq())).await;

    let html = put_avatar(
        &base,
        &u.access,
        b"<html><script>alert(1)</script></html>".to_vec(),
        "image/png",
    )
    .await;
    assert_eq!(html.status(), 400);

    let big = put_avatar(
        &base,
        &u.access,
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
            .into_iter()
            .chain(vec![0u8; 1_048_577 - 8])
            .collect(),
        "image/png",
    )
    .await;
    assert_eq!(big.status(), 400);

    let me: Value = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 0);
    assert_eq!(me["has_avatar"], false);
}

/// 删除后 has_avatar=false，公开 GET 404，版本仍递增。
#[tokio::test]
async fn p22_delete_clears_avatar() {
    let (base, _) = spawn_avatar().await;
    let u = register(&base, &format!("avd_{}", uniq())).await;
    let uploaded: Value = put_avatar(&base, &u.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(uploaded["avatar_version"], 1);

    let me: Value = http()
        .delete(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["has_avatar"], false);
    assert_eq!(me["avatar_version"], 2);

    let r = http()
        .get(format!("{base}/v1/users/{}/avatar?v=2", u.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // 已经没有头像再删：版本不再递增，避免空操作刷失效
    let again: Value = http()
        .delete(format!("{base}/v1/users/me/avatar"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["has_avatar"], false);
    assert_eq!(again["avatar_version"], 2);
}

/// 未登录不能上传；接口只作用于自己。
#[tokio::test]
async fn p22_upload_requires_self() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("ava_{}", uniq())).await;
    let b = register(&base, &format!("avb2_{}", uniq())).await;

    let anon = http()
        .put(format!("{base}/v1/users/me/avatar"))
        .multipart(avatar_part(png_bytes(), "image/png"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["id"], a.id);
    assert_eq!(me["avatar_version"], 1);

    let b_me: Value = http()
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(b_me["has_avatar"], false);
    assert_eq!(b_me["avatar_version"], 0);
}

/// 好友列表、会话 peer、消息 sender_avatar_version 都反映新版本。
#[tokio::test]
async fn p22_avatar_visible_on_friends_peer_and_messages() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("avs_{}", uniq())).await;
    let b = register(&base, &format!("avr2_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["avatar_version"], 1);

    let friends: Value = http()
        .get(format!("{base}/v1/friends"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let friend = friends
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["user"]["id"] == a.id)
        .unwrap();
    assert_eq!(friend["user"]["avatar_version"], 1);
    assert_eq!(friend["user"]["has_avatar"], true);

    let convs: Value = http()
        .get(format!("{base}/v1/conversations"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let peer = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv)
        .unwrap();
    assert_eq!(peer["peer"]["avatar_version"], 1);
    assert_eq!(peer["peer"]["has_avatar"], true);

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let sent = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "av1", "content": "hi"}),
    )
    .await;
    assert_eq!(sent["sender_avatar_version"], 1);
    assert_eq!(sent["sender_has_avatar"], true);

    let page = history(&base, &b.access, &conv).await;
    let listed = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sender_id"] == a.id)
        .unwrap();
    assert_eq!(listed["sender_avatar_version"], 1);
    assert_eq!(listed["sender_has_avatar"], true);
}

/// 头像变更后好友的 WS 收到 invalidate { what: conversations }。
#[tokio::test]
async fn p22_avatar_change_invalidates_watchers() {
    let (base, _) = spawn_avatar().await;
    let a = register(&base, &format!("avw_{}", uniq())).await;
    let b = register(&base, &format!("avw2_{}", uniq())).await;
    befriend(&base, &a, &b).await;

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let me: Value = put_avatar(&base, &a.access, png_bytes(), "image/png")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["has_avatar"], true);

    let frame = next_of(&mut sb, "invalidate").await;
    assert_eq!(frame["what"], "conversations");
}
