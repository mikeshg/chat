use super::*;

// ===== Agent A05 =====
// 附件进出 Agent 专属会话：主人的附件消息是「待交付文件」，不触发调用；
// 下一条文字指令把它们带走（invocation_start.attachments + InvocationView.attachment_ids，
// 上限 10 个）。Agent 产出文件经 /v1/agent-tools/files（Invocation 凭据）上传，
// invocation_finish.files 回传后服务端按序发 Agent 的附件消息。

/// 带附件的 send_message（attachment_id 字段；content 为空即纯附件消息）
async fn send_ws_att<S>(s: &mut S, conv: &str, cmid: &str, att: &str, content: &str)
where
    S: SinkExt<WsMsg> + Unpin,
    <S as futures_util::Sink<WsMsg>>::Error: std::fmt::Debug,
{
    s.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":conv,"client_msg_id":cmid,
               "content":content,"attachment_id":att})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
}

/// POST /v1/agent-tools/files：返回裸 Response（要断言 200/401/413）
async fn tool_upload(
    base: &str,
    auth: &str,
    name: &str,
    mime: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(body)
        .file_name(name.to_string())
        .mime_str(mime)
        .unwrap();
    http()
        .post(format!("{base}/v1/agent-tools/files"))
        .header("authorization", auth)
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

/// 收集用户侧帧直到指定调用到终态；inv_id 为空时只看第一个终态调用
async fn until_inv_terminal(s: &mut RawHostWs, inv_id: &str) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        frames.extend(until_frame(s, "agent_invocation", A01Dur::from_secs(20)).await);
        if frames.iter().any(|f| {
            f["type"] == "agent_invocation"
                && (inv_id.is_empty() || f["invocation"]["id"] == inv_id)
                && matches!(
                    f["invocation"]["status"].as_str(),
                    Some("succeeded") | Some("failed") | Some("cancelled") | Some("timed_out")
                )
        }) {
            return frames;
        }
    }
}

/// 主路径：附件消息不触发调用；下一条文字指令把待交付附件交给 Agent。
/// 假 agent inspect 回报 image:/link: 两行 → 证明 host 收到了 attachments 元数据、
/// 下载了文件并按 image 能力构造了提示块。
#[tokio::test]
async fn a05_attachments_ride_next_text() {
    let base = spawn_named_db(&format!("chat_t_a05a_{}", uniq())).await;
    let a = register(&base, &format!("a05a_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "filesaw").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let png = png_fixture(64, 48);
    let png_len = png.len();
    let (st, p) = upload_att(&base, &conv, &a.access, "shot.png", "image/png", png).await;
    assert_eq!(st, 200, "{p}");
    let png_id = p["id"].as_str().unwrap().to_string();
    let (st, t) = upload_att(
        &base,
        &conv,
        &a.access,
        "notes.txt",
        "text/plain",
        b"note-body".to_vec(),
    )
    .await;
    assert_eq!(st, 200, "{t}");
    let txt_id = t["id"].as_str().unwrap().to_string();

    // 附件消息（含一条带文字附言的）不产生调用
    send_ws_att(&mut sa, &conv, "a05-p1", &png_id, "").await;
    send_ws_att(&mut sa, &conv, "a05-p2", &txt_id, "先看这两个文件").await;
    let quiet = frames_for(&mut sa, A01Dur::from_millis(800)).await;
    assert!(
        !quiet.iter().any(|f| f["type"] == "agent_invocation"),
        "附件消息不应触发调用: {quiet:?}"
    );
    assert!(
        quiet
            .iter()
            .filter(|f| f["type"] == "message" && f["message"]["attachment"].is_object())
            .count()
            >= 2,
        "附件消息本身仍应广播: {quiet:?}"
    );

    // 下一条文字 → 调用带走两个附件（按消息 seq 顺序）
    send_ws(&mut sa, &conv, "a05-c1", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(inv["invocation"]["status"], "running");
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();
    let ids: Vec<String> = inv["invocation"]["attachment_ids"]
        .as_array()
        .expect("agent_invocation 帧应带 attachment_ids")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![png_id.clone(), txt_id.clone()]);

    let frames = until_inv_terminal(&mut sa, &inv_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    // PNG 走了 image 块（假 agent 声明了 promptCapabilities.image）
    assert!(
        content.contains(&format!("image:image/png:{png_len}")),
        "{content}"
    );
    // 文本文件走 resource_link，且文件确实下载到了 in/
    assert!(content.contains("link:notes.txt:true"), "{content}");

    // REST 读模型同样带 attachment_ids
    let rows: Value = http()
        .get(format!("{base}/v1/conversations/{conv}/agent-invocations"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<String> = rows.as_array().unwrap()[0]["attachment_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![png_id, txt_id]);

    ha.shutdown().await;
}

/// --no-image：agent 未声明 image 能力时图片也走 resource_link
#[tokio::test]
async fn a05_no_image_capability_uses_link() {
    let base = spawn_named_db(&format!("chat_t_a05b_{}", uniq())).await;
    let a = register(&base, &format!("a05b_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--no-image".into());
    let (st, agent) = create_agent_with(&base, &a, &host_id, "nopic", args).await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, p) = upload_att(
        &base,
        &conv,
        &a.access,
        "pic.png",
        "image/png",
        png_fixture(20, 20),
    )
    .await;
    assert_eq!(st, 200, "{p}");
    let png_id = p["id"].as_str().unwrap().to_string();
    send_ws_att(&mut sa, &conv, "a05-n1", &png_id, "").await;
    let _ = next_of(&mut sa, "message").await;

    send_ws(&mut sa, &conv, "a05-n2", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv_id = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap()["invocation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    assert!(content.contains("link:pic.png:true"), "{content}");
    assert!(!content.contains("image:"), "{content}");

    ha.shutdown().await;
}

/// 附件只被带走一次：第二次 inspect 不再收到上次的附件
#[tokio::test]
async fn a05_attachments_consumed_once() {
    let base = spawn_named_db(&format!("chat_t_a05c_{}", uniq())).await;
    let a = register(&base, &format!("a05c_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "once").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, p) = upload_att(
        &base,
        &conv,
        &a.access,
        "a.txt",
        "text/plain",
        b"1".to_vec(),
    )
    .await;
    assert_eq!(st, 200, "{p}");
    let att_id = p["id"].as_str().unwrap().to_string();
    send_ws_att(&mut sa, &conv, "a05-o1", &att_id, "").await;
    let _ = next_of(&mut sa, "message").await;

    send_ws(&mut sa, &conv, "a05-o2", "inspect").await;
    let frames = until_inv_terminal(&mut sa, "").await;
    let inv1 = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "succeeded")
        .unwrap();
    assert_eq!(
        inv1["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 第二次 inspect：不带附件
    send_ws(&mut sa, &conv, "a05-o3", "inspect").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv2 = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    let inv2_id = inv2["invocation"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        inv2["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "已交付的附件不应再次携带"
    );
    let frames = until_inv_terminal(&mut sa, &inv2_id).await;
    let reply = frames
        .iter()
        .find(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .expect("应有 Agent 回复");
    let content = reply["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("(no attachments)"),
        "第二次 inspect 不该看到上次附件: {content}"
    );

    ha.shutdown().await;
}

/// 边界：待交付附件上限 10；超出部分继续待交付，被再下一条指令带走。
/// 用裸宿主直接看 invocation_start.attachments 的形状（id/name/mime/size）。
#[tokio::test]
async fn a05_pending_attachments_capped_at_10() {
    let base = spawn_named_db(&format!("chat_t_a05d_{}", uniq())).await;
    let a = register(&base, &format!("a05d_{}", uniq())).await;
    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "capper").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..12 {
        let (st, p) = upload_att(
            &base,
            &conv,
            &a.access,
            &format!("f{i:02}.txt"),
            "text/plain",
            format!("b{i}").into_bytes(),
        )
        .await;
        assert_eq!(st, 200, "{p}");
        ids.push(p["id"].as_str().unwrap().to_string());
        send_ws_att(&mut su, &conv, &format!("a05-k{i}"), &ids[i], "").await;
    }
    let quiet = frames_for(&mut su, A01Dur::from_millis(800)).await;
    assert!(
        !quiet.iter().any(|f| f["type"] == "agent_invocation"),
        "12 条附件消息都不该触发: {quiet:?}"
    );

    // 第 13 条消息是文字 → 触发调用，只带前 10 个
    send_ws(&mut su, &conv, "a05-cmd", "go").await;
    let host_frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start = host_frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let atts = start["attachments"]
        .as_array()
        .expect("invocation_start 应带 attachments");
    assert_eq!(atts.len(), 10, "附件上限 10: {atts:?}");
    for (i, att) in atts.iter().enumerate() {
        assert_eq!(att["id"].as_str().unwrap(), ids[i]);
        assert_eq!(att["name"].as_str().unwrap(), format!("f{i:02}.txt"));
        assert_eq!(att["mime"].as_str().unwrap(), "text/plain");
        assert_eq!(att["size"].as_i64().unwrap(), 2);
    }
    let inv_id = start["invocation_id"].as_str().unwrap().to_string();

    // 用户侧帧同样携带 10 个 attachment_ids
    let frames = until_frame(&mut su, "agent_invocation", A01Dur::from_secs(10)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    assert_eq!(
        inv["invocation"]["attachment_ids"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    host_finish(&mut sh, &inv_id).await;
    let _ = until_inv_terminal(&mut su, &inv_id).await;

    // 剩余 2 个仍处于待交付：下一条文字带走
    send_ws(&mut su, &conv, "a05-cmd2", "go2").await;
    let host_frames = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let start2 = host_frames
        .iter()
        .find(|f| f["type"] == "invocation_start")
        .unwrap();
    let atts2 = start2["attachments"].as_array().unwrap();
    assert_eq!(atts2.len(), 2, "剩余附件应继续待交付: {atts2:?}");
    assert_eq!(atts2[0]["id"].as_str().unwrap(), ids[10]);
    assert_eq!(atts2[1]["id"].as_str().unwrap(), ids[11]);
    let inv2 = start2["invocation_id"].as_str().unwrap().to_string();
    host_finish(&mut sh, &inv2).await;
    let _ = until_inv_terminal(&mut su, &inv2).await;
}

/// Agent 产出文件：先正文后附件消息；附件可下载；占用主人的存储配额
#[tokio::test]
async fn a05_agent_output_file_becomes_message() {
    let base = spawn_named_db(&format!("chat_t_a05e_{}", uniq())).await;
    let a = register(&base, &format!("a05e_{}", uniq())).await;
    let (ha, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "writer").await;
    assert_eq!(st, 200, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let st0: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let used0 = st0["used_bytes"].as_i64().unwrap();

    send_ws(&mut sa, &conv, "a05-m1", "makefile:report.txt:hello").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv_id = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap()["invocation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;

    // 两条 Agent 消息：先文本 "ok"，再文件消息
    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    assert_eq!(agent_msgs.len(), 2, "应是一条正文+一条附件消息: {frames:?}");
    assert_eq!(agent_msgs[0]["message"]["content"], "ok");
    assert!(agent_msgs[0]["message"]["attachment"].is_null());
    let fm = &agent_msgs[1]["message"];
    assert_eq!(fm["kind"], "file");
    assert_eq!(fm["agent"]["id"], agent_id.as_str());
    assert_eq!(fm["attachment"]["file_name"], "report.txt");
    assert_eq!(fm["attachment"]["mime"], "text/plain");
    let att_id = fm["attachment"]["id"].as_str().unwrap().to_string();
    assert!(
        fm["seq"].as_i64().unwrap() > agent_msgs[0]["message"]["seq"].as_i64().unwrap(),
        "附件消息应在正文之后"
    );

    // 能力 URL 下载到写入的内容
    let data = http()
        .get(format!("{base}/v1/files/{att_id}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&data[..], b"hello");

    // 上传者=主人：存储用量增加 5 字节
    let st1: Value = http()
        .get(format!("{base}/v1/users/me/storage"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st1["used_bytes"].as_i64().unwrap(), used0 + 5);

    ha.shutdown().await;
}

/// 产出文件超配额：调用仍成功，正文追加「未发送」提示，不发附件消息
#[tokio::test]
async fn a05_output_over_quota_skipped() {
    let base = spawn_named_db(&format!("chat_t_a05f_{}", uniq())).await;
    let admin = register(&base, &format!("a05f_{}", uniq())).await;
    let r = admin_put_settings(&base, &admin, json!({"storage_quota_mb": 1})).await;
    assert_eq!(r.status(), 200);
    let (ha, host_id) = spawn_host(&base, &admin, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &admin, &host_id, "fat").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &admin).await)
        .await
        .unwrap();

    // 1.5MB 产出 > 1MB 配额 → 服务端 413 → files_skipped=1
    send_ws(&mut sa, &conv, "a05-q1", "makebig:big.bin:1572864").await;
    let frames = until_frame(&mut sa, "agent_invocation", A01Dur::from_secs(15)).await;
    let inv = frames
        .iter()
        .find(|f| f["type"] == "agent_invocation")
        .unwrap();
    let inv_id = inv["invocation"]["id"].as_str().unwrap().to_string();
    let frames = until_inv_terminal(&mut sa, &inv_id).await;

    let fin = frames
        .iter()
        .rev()
        .find(|f| f["type"] == "agent_invocation" && f["invocation"]["id"] == inv_id.as_str())
        .unwrap();
    assert_eq!(fin["invocation"]["status"], "succeeded");

    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    // 只有一条正文，追加配额提示；无附件消息
    assert_eq!(agent_msgs.len(), 1, "{frames:?}");
    let content = agent_msgs[0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("ok"), "{content}");
    assert!(content.contains("1 个文件"), "{content}");
    assert!(content.contains("未发送"), "{content}");

    ha.shutdown().await;
}

/// 凭据与归属：tools_token 只在调用存活期有效；finish 里别调用的附件 id 被忽略；files_failed 进正文
#[tokio::test]
async fn a05_tool_upload_scoped_to_invocation() {
    let base = spawn_named_db(&format!("chat_t_a05g_{}", uniq())).await;
    let a = register(&base, &format!("a05g_{}", uniq())).await;
    let (mut sh, host_id) = spawn_raw_host(&base, &a).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "tooler2").await;
    assert_eq!(st, 200, "{agent}");
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut su, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    // 无凭据 / 普通 Bearer 都被拒
    let r = tool_upload(&base, "", "a.txt", "text/plain", b"x".to_vec()).await;
    assert_eq!(r.status(), 401);
    let r = tool_upload(
        &base,
        &format!("Bearer {}", a.access),
        "a.txt",
        "text/plain",
        b"x".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 401);

    // inv1 running：上传成功，归属=主人+专属会话+inv1
    send_ws(&mut su, &conv, "a05-s1", "t1").await;
    let f = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s1 = f.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv1 = s1["invocation_id"].as_str().unwrap().to_string();
    let tok1 = s1["tools_token"].as_str().unwrap().to_string();
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "a.txt",
        "text/plain",
        b"hi".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let att_a = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();

    // inv2 running：att_a 属于 inv1，写进 inv2 的 finish.files 被忽略
    send_ws(&mut su, &conv, "a05-s2", "t2").await;
    let f = until_frame(&mut sh, "invocation_start", A01Dur::from_secs(10)).await;
    let s2 = f.iter().find(|x| x["type"] == "invocation_start").unwrap();
    let inv2 = s2["invocation_id"].as_str().unwrap().to_string();
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv2,"ok":true,"text":"done",
               "files":[att_a], "files_failed": 2})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let frames = until_inv_terminal(&mut su, &inv2).await;
    let agent_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["agent"].is_object())
        .collect();
    assert_eq!(agent_msgs.len(), 1, "越权附件 id 不应产生消息: {frames:?}");
    // 宿主上传失败的产出要在正文里说出来
    assert_eq!(
        agent_msgs[0]["message"]["content"],
        "done
（2 个文件上传失败未发送）"
    );

    // inv1 还活着：凭据仍可用；finish 里写自己的两个文件 → 两条附件消息按序到达
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "b.txt",
        "text/plain",
        b"yo".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 200);
    let att_b = r.json::<Value>().await.unwrap()["attachment_id"]
        .as_str()
        .unwrap()
        .to_string();
    sh.send(WsMsg::Text(
        json!({"type":"invocation_finish","invocation_id":inv1,"ok":true,"text":"done1",
               "files":[att_a, att_b]})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let frames = until_inv_terminal(&mut su, &inv1).await;
    let file_msgs: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "message" && f["message"]["attachment"].is_object())
        .collect();
    assert_eq!(file_msgs.len(), 2, "{frames:?}");
    assert_eq!(file_msgs[0]["message"]["attachment"]["file_name"], "a.txt");
    assert_eq!(file_msgs[1]["message"]["attachment"]["file_name"], "b.txt");

    // inv1 结束后同一凭据再上传 → 401
    let r = tool_upload(
        &base,
        &format!("Invocation {tok1}"),
        "c.txt",
        "text/plain",
        b"late".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 401);
}
