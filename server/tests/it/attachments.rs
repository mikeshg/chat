use super::*;

// ===== V2 P2-1 =====
// 图片尺寸/缩略图 + 每用户存储配额

/// spawn() 共用的 files_dir（与 spawn() 中的路径一致）
fn it_files_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("chat-it-files")
}

/// 复制一行 attachments（新 id，共享 storage_path/thumb_path），模拟 P2-4 转发副本
async fn copy_attachment(pool: &sqlx::PgPool, src_id: &str) -> String {
    let copy_id = uuid::Uuid::new_v4();
    let n = sqlx::query(
        "INSERT INTO attachments
             (id, uploader_id, conv_id, file_name, mime, size, storage_path, width, height, thumb_path)
         SELECT $1, uploader_id, conv_id, file_name, mime, size, storage_path, width, height, thumb_path
           FROM attachments WHERE id = $2",
    )
    .bind(copy_id)
    .bind(uuid::Uuid::parse_str(src_id).unwrap())
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1);
    copy_id.to_string()
}

/// WS 入站限速：短时间刷几百帧的连接被断开
#[tokio::test]
async fn ws_inbound_flood_disconnects() {
    let base = spawn().await;
    let a = register(&base, &format!("flood_{}", uniq())).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    for _ in 0..310 {
        if ws.send(WsMsg::Text("{}".into())).await.is_err() {
            break;
        }
    }
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(m) = ws.next().await {
            if matches!(m, Ok(WsMsg::Close(_)) | Err(_)) {
                return;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "刷帧的连接应被断开");
}

/// 下载走流式：Range 回 206 片段（音视频拖动要用），类型和下载方式仍按库里记的
#[tokio::test]
async fn download_supports_range_and_keeps_headers() {
    let base = spawn().await;
    let a = register(&base, &format!("rng_{}", uniq())).await;
    let b = register(&base, &format!("rngb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let body: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    let (st, att) = upload_att(
        &base,
        &conv,
        &a.access,
        "clip.mp4",
        "video/mp4",
        body.clone(),
    )
    .await;
    assert_eq!(st, 200, "{att}");
    let url = format!("{base}/v1/files/{}", att["id"].as_str().unwrap());

    let full = http().get(&url).send().await.unwrap();
    assert_eq!(full.status(), 200);
    assert_eq!(full.headers()["content-type"], "video/mp4");
    assert!(full.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("inline;"));
    assert_eq!(full.headers()["x-content-type-options"], "nosniff");
    assert_eq!(full.bytes().await.unwrap().to_vec(), body);

    let part = http()
        .get(&url)
        .header("range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 100-199/10000");
    assert_eq!(part.headers()["content-type"], "video/mp4");
    assert_eq!(
        part.bytes().await.unwrap().to_vec(),
        body[100..200].to_vec()
    );

    let dl = http().get(format!("{url}?dl=1")).send().await.unwrap();
    assert!(dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment;"));
    assert_eq!(
        http()
            .get(format!("{base}/v1/files/{}", uuid::Uuid::new_v4()))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

/// 大图：上传记录宽高 + 生成缩略图；?thumb=1 拿 JPEG；消息视图字段一致
#[tokio::test]
async fn p2_1_image_dims_thumb() {
    let base = spawn().await;
    let a = register(&base, &format!("p2a_{}", uniq())).await;
    let b = register(&base, &format!("p2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let (st, att) = upload_att(
        &base,
        &conv,
        &a.access,
        "big.png",
        "image/png",
        png_fixture(1200, 800),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap();
    assert_eq!(att["width"], 1200);
    assert_eq!(att["height"], 800);
    assert_eq!(att["has_thumb"], true);

    // ?thumb=1 → JPEG，最长边 480
    let dl = c
        .get(format!("{base}/v1/files/{att_id}?thumb=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.headers()["content-type"], "image/jpeg");
    let t = image::load_from_memory(&dl.bytes().await.unwrap()).unwrap();
    assert_eq!((t.width(), t.height()), (480, 320));

    // dl=1 忽略 thumb：返回原图
    let dl = c
        .get(format!("{base}/v1/files/{att_id}?thumb=1&dl=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.headers()["content-type"], "image/png");

    // 消息视图字段与上传响应一致
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "i1", "content": "", "attachment_id": att_id}),
    )
    .await;
    assert_eq!(m["kind"], "image");
    assert_eq!(m["attachment"]["width"], 1200);
    assert_eq!(m["attachment"]["height"], 800);
    assert_eq!(m["attachment"]["has_thumb"], true);
}

/// 小图有宽高无缩略图（?thumb=1 回退原图）；坏图照常入库、字段为 null
#[tokio::test]
async fn p2_1_small_and_broken_image() {
    let base = spawn().await;
    let a = register(&base, &format!("s2a_{}", uniq())).await;
    let b = register(&base, &format!("s2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let png = png_fixture(300, 200);
    let (st, att) = upload_att(&base, &conv, &a.access, "s.png", "image/png", png.clone()).await;
    assert_eq!(st, 200);
    assert_eq!(att["width"], 300);
    assert_eq!(att["height"], 200);
    assert_eq!(att["has_thumb"], false);
    let dl = c
        .get(format!(
            "{base}/v1/files/{}?thumb=1",
            att["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 200);
    assert_eq!(dl.headers()["content-type"], "image/png");
    assert_eq!(dl.bytes().await.unwrap().as_ref(), png.as_slice());

    // 坏图：mime 声明 image/png 但内容不可解码 → 照常入库，尺寸/缩略图缺省
    let junk: Vec<u8> = (0..8192).map(|i| (i * 31 % 256) as u8).collect();
    let (st, att) = upload_att(&base, &conv, &a.access, "b.png", "image/png", junk).await;
    assert_eq!(st, 200);
    assert!(att["width"].is_null());
    assert!(att["height"].is_null());
    assert_eq!(att["has_thumb"], false);
}

/// 撤回消息：附件行保留并隐藏（公开下载 404），物理文件由过期清扫按引用计数删——
/// 仍有副本共享 storage_path 时原图与缩略图都保留
#[tokio::test]
async fn p2_1_delete_cleans_thumb() {
    let base = spawn().await;
    let a = register(&base, &format!("d2a_{}", uniq())).await;
    let b = register(&base, &format!("d2b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let (_, att) = upload_att(
        &base,
        &conv,
        &a.access,
        "d.png",
        "image/png",
        png_fixture(900, 700),
    )
    .await;
    let id = att["id"].as_str().unwrap();
    let file = it_files_dir().join(id);
    let thumb = it_files_dir().join(format!("{id}.thumb.jpg"));
    assert!(file.exists());
    assert!(thumb.exists());

    let m = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d1", "content": "", "attachment_id": id}),
    )
    .await;
    let r = c
        .delete(format!("{base}/v1/messages/{}", m["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 撤回不再删文件：公开下载立即 404，物理文件与缩略图保留（待过期清扫）
    let dl = c
        .get(format!("{base}/v1/files/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), 404);
    assert!(file.exists());
    assert!(thumb.exists());

    // 拨过保留期后清扫：独立 storage_path 的原图与缩略图一起删
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let att_uuid = uuid::Uuid::parse_str(id).unwrap();
    assert!(sweep_recalled(&pool, it_files_dir().to_str().unwrap(), &[att_uuid]).await >= 1);
    assert!(!file.exists());
    assert!(!thumb.exists());

    // 共享 storage_path 的副本（模拟转发）存在时，撤回一条不清理物理文件
    let (_, att2) = upload_att(
        &base,
        &conv,
        &a.access,
        "d2.png",
        "image/png",
        png_fixture(900, 700),
    )
    .await;
    let id2 = att2["id"].as_str().unwrap();
    let file2 = it_files_dir().join(id2);
    let thumb2 = it_files_dir().join(format!("{id2}.thumb.jpg"));
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    let copy = copy_attachment(&pool, id2).await;
    let m2 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d2", "content": "", "attachment_id": id2}),
    )
    .await;
    let m3 = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "d3", "content": "", "attachment_id": copy}),
    )
    .await;

    let r = c
        .delete(format!("{base}/v1/messages/{}", m2["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 撤回行过期清扫：副本行仍引用同一 storage_path，物理文件保留
    let id2_uuid = uuid::Uuid::parse_str(id2).unwrap();
    assert!(
        sweep_recalled(&pool, it_files_dir().to_str().unwrap(), &[id2_uuid]).await >= 1
    );
    assert!(file2.exists());
    assert!(thumb2.exists());

    let r = c
        .delete(format!("{base}/v1/messages/{}", m3["id"].as_str().unwrap()))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // 最后一个引用过期：原图与缩略图一起删
    let copy_uuid = uuid::Uuid::parse_str(&copy).unwrap();
    assert!(
        sweep_recalled(&pool, it_files_dir().to_str().unwrap(), &[copy_uuid]).await >= 1
    );
    assert!(!file2.exists());
    assert!(!thumb2.exists());
}

/// 配额：1MB 上限 → 600KB 成功、再 600KB 拒绝；共享行不重复计；0 恢复不限
#[tokio::test]
async fn p2_1_storage_quota() {
    let (_guard, base, state) = spawn_named_state(&format!("chat_t_p21q_{}", uniq())).await;
    let admin = register(&base, &format!("qad_{}", uniq())).await;
    let u = register(&base, &format!("qu_{}", uniq())).await;
    befriend(&base, &admin, &u).await;
    let conv = dm_conv(&base, &u).await;
    let c = http();

    // 缺省不限额
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 0);
    assert!(s["quota_bytes"].is_null());

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["storage_quota_mb"], 1);

    // 600KB 成功；再 600KB → 413 带中文提示
    let blob = vec![7u8; 600 * 1024];
    let (st, att) = upload_att(
        &base,
        &conv,
        &u.access,
        "a.bin",
        "application/octet-stream",
        blob.clone(),
    )
    .await;
    assert_eq!(st, 200);
    let att_id = att["id"].as_str().unwrap().to_string();
    let (st, err) = upload_att(
        &base,
        &conv,
        &u.access,
        "b.bin",
        "application/octet-stream",
        blob,
    )
    .await;
    assert_eq!(st, 413);
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("存储空间已满"),
        "unexpected error body: {err}"
    );

    // me/storage：用量与上限
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 614400);
    assert_eq!(s["quota_bytes"], 1048576);

    // 共享 storage_path 的副本不重复计
    copy_attachment(&state.pool, &att_id).await;
    let s: Value = c
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["used_bytes"], 614400);

    // admin/users 行带 storage_bytes
    let users: Value = c
        .get(format!("{base}/v1/admin/users"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == u.id)
        .unwrap();
    assert_eq!(row["storage_bytes"], 614400);

    // 调 0 → 恢复不限
    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (st, _) = upload_att(
        &base,
        &conv,
        &u.access,
        "c.bin",
        "application/octet-stream",
        vec![9u8; 600 * 1024],
    )
    .await;
    assert_eq!(st, 200);
}

/// 非管理员设配额 403；非法值 400；server-info 不暴露配额
#[tokio::test]
async fn p2_1_quota_admin_and_validation() {
    let (_guard, base) = spawn_named_db(&format!("chat_t_p21v_{}", uniq())).await;
    let admin = register(&base, &format!("vad_{}", uniq())).await;
    let u = register(&base, &format!("vu_{}", uniq())).await;
    let c = http();

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&u.access))
        .json(&json!({"storage_quota_mb": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    for v in [json!(-1), json!(1048577), json!("5"), json!(1.5)] {
        let r = c
            .put(format!("{base}/v1/admin/settings"))
            .header("authorization", authz(&admin.access))
            .json(&json!({"storage_quota_mb": v}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{v} 应 400");
    }

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"storage_quota_mb": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.get("storage_quota_mb").is_none());
}
