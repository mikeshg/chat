use super::*;

// ===== V2 P2-3 =====

/// 上传音频附件。`duration` 为 None 时不带字段；Some 时写入 multipart 文本。
async fn upload_audio(
    base: &str,
    conv: &str,
    at: &str,
    name: &str,
    mime: &str,
    body: &[u8],
    duration: Option<&str>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(body.to_vec())
        .file_name(name.to_string())
        .mime_str(mime)
        .unwrap();
    let mut form = reqwest::multipart::Form::new()
        .text("conversation_id", conv.to_string())
        .part("file", part);
    if let Some(d) = duration {
        form = form.text("duration_ms", d.to_string());
    }
    http()
        .post(format!("{base}/v1/files"))
        .header("authorization", authz(at))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

fn assert_voice(view: &Value, duration: i64) {
    assert_eq!(view["kind"], "audio");
    assert_eq!(view["content"], "[语音]");
    assert_eq!(view["attachment"]["duration_ms"].as_i64(), Some(duration));
    assert_eq!(view["attachment"]["mime"], "audio/webm");
}

/// 语音消息：上传带时长 → 发送后 kind/占位/时长在历史、广播、sync 中一致。
#[tokio::test]
async fn p23_voice_upload_send_sync() {
    let base = spawn().await;
    let a = register(&base, &format!("v3a_{}", uniq())).await;
    let b = register(&base, &format!("v3b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "voice.webm",
        "audio/webm",
        b"webm-bytes",
        Some("5300"),
    )
    .await;
    assert_eq!(up.status(), 200);
    let att: Value = up.json().await.unwrap();
    assert_eq!(att["duration_ms"].as_i64(), Some(5300));
    let att_id = att["id"].as_str().unwrap();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let ack = ws_send(
        &mut sa,
        &conv,
        json!({"client_msg_id": "v1", "content": "", "attachment_id": att_id}),
    )
    .await;
    assert_voice(&ack, 5300);

    let broadcast = next_of(&mut sb, "message").await;
    assert_voice(&broadcast["message"], 5300);

    let page = history(&base, &b.access, &conv).await;
    let hist = &page["messages"][0];
    assert_voice(hist, 5300);

    sb.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let synced = next_of(&mut sb, "sync_messages").await;
    assert_voice(&synced["messages"][0], 5300);
}

/// duration_ms 只接受音频，且必须是 1–300000 的整数。
#[tokio::test]
async fn p23_duration_rejected() {
    let base = spawn().await;
    let a = register(&base, &format!("v3c_{}", uniq())).await;
    let b = register(&base, &format!("v3d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47];

    let cases = [
        ("audio/webm", "voice.webm", b"a".as_slice(), Some("0")),
        ("audio/webm", "voice.webm", b"a", Some("300001")),
        ("audio/webm", "voice.webm", b"a", Some("nope")),
        ("image/png", "pic.png", png, Some("5300")),
    ];
    for (mime, name, body, dur) in cases {
        let r = upload_audio(&base, &conv, &a.access, name, mime, body, dur).await;
        assert_eq!(r.status(), 400, "{name} duration={dur:?} 应 400");
    }
}

/// 普通音频文件不带 duration_ms，仍是 file，时长为空。
#[tokio::test]
async fn p23_plain_audio_stays_file() {
    let base = spawn().await;
    let a = register(&base, &format!("v3e_{}", uniq())).await;
    let b = register(&base, &format!("v3f_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "song.mp3",
        "audio/mpeg",
        b"mp3",
        None,
    )
    .await;
    assert_eq!(up.status(), 200);
    let att: Value = up.json().await.unwrap();
    assert!(att.get("duration_ms").unwrap().is_null());

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let ack = ws_send(
        &mut sa,
        &conv,
        json!({
            "client_msg_id": "f1",
            "content": "",
            "attachment_id": att["id"].as_str().unwrap()
        }),
    )
    .await;
    assert_eq!(ack["kind"], "file");
    assert_eq!(ack["content"], "[文件] song.mp3");
    assert!(ack["attachment"]["duration_ms"].is_null());
}

/// 语音不可编辑；删除后墓碑不再带出 duration_ms。
#[tokio::test]
async fn p23_voice_edit_denied_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("v3g_{}", uniq())).await;
    let b = register(&base, &format!("v3h_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = dm_conv(&base, &a).await;
    let c = http();

    let up = upload_audio(
        &base,
        &conv,
        &a.access,
        "voice.webm",
        "audio/webm",
        b"webm",
        Some("1500"),
    )
    .await;
    let att: Value = up.json().await.unwrap();

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let sent = ws_send(
        &mut sa,
        &conv,
        json!({
            "client_msg_id": "v2",
            "content": "",
            "attachment_id": att["id"].as_str().unwrap()
        }),
    )
    .await;
    let mid = sent["id"].as_str().unwrap();

    let edit = c
        .patch(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"content": "改一下", "expected_version": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(edit.status(), 400);

    let del = c
        .delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let tomb: Value = del.json().await.unwrap();
    assert!(tomb["deleted_at"].as_str().is_some());
    assert!(tomb["attachment"].is_null());
    assert!(tomb
        .get("attachment")
        .and_then(|a| a.get("duration_ms"))
        .is_none());

    let page = history(&base, &b.access, &conv).await;
    let hist = &page["messages"][0];
    assert!(hist["attachment"].is_null());
    assert!(hist["content"] != "[语音]");
}
