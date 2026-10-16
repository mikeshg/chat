use super::*;

// ===== V2 P3-2 =====
// 群公告、置顶消息、会议链接。改全局设置的用例用独立库。

fn conv_of(list: &Value, id: &str) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("conv {id} missing"))
        .clone()
}

async fn put_announcement(
    base: &str,
    u: &TestUser,
    conv: &str,
    text: Option<&str>,
) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/announcement"))
        .header("authorization", authz(&u.access))
        .json(&json!({"text": text}))
        .send()
        .await
        .unwrap()
}

async fn list_pins(base: &str, u: &TestUser, conv: &str) -> reqwest::Response {
    http()
        .get(format!("{base}/v1/conversations/{conv}/pins"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

async fn put_pin(base: &str, u: &TestUser, conv: &str, mid: &str) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/pins/{mid}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

async fn del_pin(base: &str, u: &TestUser, conv: &str, mid: &str) -> reqwest::Response {
    http()
        .delete(format!("{base}/v1/conversations/{conv}/pins/{mid}"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

/// 群主设公告 → 成员目录可见且收到 invalidate；成员 403；私聊 400；清空为 null；超长 400。
#[tokio::test]
async fn p32_announcement_owner_only_and_invalidate() {
    let base = spawn().await;
    let a = register(&base, &format!("p32a_{}", uniq())).await;
    let b = register(&base, &format!("p32b_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let r = put_announcement(&base, &a, &gid, Some("值班表\n周一 A")).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["text"], "值班表\n周一 A");
    assert_eq!(body["updated_by_name"], a.username);
    assert!(body["updated_at"].is_string());

    let frame = next_of(&mut sb, "invalidate").await;
    assert_eq!(frame["what"], "conversations");

    let listed = conv_of(&conv_list(&base, &b).await, &gid);
    assert_eq!(listed["announcement"]["text"], "值班表\n周一 A");
    assert_eq!(listed["announcement"]["updated_by_name"], a.username);

    let r = put_announcement(&base, &b, &gid, Some("成员改公告")).await;
    assert_eq!(r.status(), 403);

    let r = put_announcement(&base, &a, &dm, Some("私聊没有公告")).await;
    assert_eq!(r.status(), 400);

    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = put_announcement(&base, &a, &selfc, Some("助手没有公告")).await;
    assert_eq!(r.status(), 400);

    let r = put_announcement(&base, &a, &gid, None).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["text"].is_null());
    assert!(body["updated_by_name"].is_null());
    assert!(body["updated_at"].is_null());
    let listed = conv_of(&conv_list(&base, &b).await, &gid);
    assert!(listed["announcement"].is_null());

    let too_long: String = "字".repeat(2001);
    let r = put_announcement(&base, &a, &gid, Some(&too_long)).await;
    assert_eq!(r.status(), 400);
}

/// 成员置顶 → 其他成员收到 pins_updated 且列表按时间倒序；重复置顶幂等；取消后消失。
#[tokio::test]
async fn p32_pin_notifies_members_and_is_idempotent() {
    let base = spawn().await;
    let a = register(&base, &format!("p32c_{}", uniq())).await;
    let b = register(&base, &format!("p32d_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let first = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "p1", "content": "第一条"}),
    )
    .await;
    let _ = next_of(&mut sb, "message").await;
    let second = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id": "p2", "content": "第二条"}),
    )
    .await;
    let _ = next_of(&mut sb, "message").await;
    let mid1 = first["id"].as_str().unwrap();
    let mid2 = second["id"].as_str().unwrap();

    let r = put_pin(&base, &b, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let frame = next_of(&mut sa, "pins_updated").await;
    assert_eq!(frame["conversation_id"], gid);
    let _ = next_of(&mut sb, "pins_updated").await;

    let r = put_pin(&base, &a, &gid, mid2).await;
    assert_eq!(r.status(), 204);
    let _ = next_of(&mut sb, "pins_updated").await;

    let r = list_pins(&base, &a, &gid).await;
    assert_eq!(r.status(), 200);
    let pins: Value = r.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 2);
    assert_eq!(pins[0]["message"]["id"], mid2);
    assert_eq!(pins[0]["pinned_by_name"], a.username);
    assert_eq!(pins[1]["message"]["id"], mid1);
    assert_eq!(pins[1]["pinned_by_name"], b.username);

    let r = put_pin(&base, &b, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let pins: Value = list_pins(&base, &a, &gid).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 2, "重复置顶不新增行");

    let r = del_pin(&base, &a, &gid, mid1).await;
    assert_eq!(r.status(), 204);
    let pins: Value = list_pins(&base, &b, &gid).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 1);
    assert_eq!(pins[0]["message"]["id"], mid2);

    let r = del_pin(&base, &a, &gid, mid1).await;
    assert_eq!(r.status(), 204, "取消不存在的置顶也是 204");
}

/// 不能置顶别的会话的消息；非成员不能读置顶列表。
#[tokio::test]
async fn p32_pin_rejects_foreign_message_and_non_member() {
    let base = spawn().await;
    let a = register(&base, &format!("p32e_{}", uniq())).await;
    let b = register(&base, &format!("p32f_{}", uniq())).await;
    let cuser = register(&base, &format!("p32g_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let foreign = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id": "fx", "content": "私聊内容"}),
    )
    .await;
    let mid = foreign["id"].as_str().unwrap();

    let r = put_pin(&base, &a, &gid, mid).await;
    assert!(
        r.status() == 400 || r.status() == 404,
        "跨会话置顶应 400 或 404"
    );

    let r = list_pins(&base, &cuser, &gid).await;
    assert_eq!(r.status(), 403);
    let r = put_pin(&base, &cuser, &gid, mid).await;
    assert_eq!(r.status(), 403);
}

/// 第 21 条置顶被拒绝。
#[tokio::test]
async fn p32_pin_cap_is_twenty() {
    let base = spawn().await;
    let a = register(&base, &format!("p32h_{}", uniq())).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let mut ids = Vec::new();
    for i in 0..21 {
        let m = ws_send(
            &mut sa,
            &selfc,
            json!({"client_msg_id": format!("cap{i}"), "content": format!("m{i}")}),
        )
        .await;
        ids.push(m["id"].as_str().unwrap().to_string());
    }
    for id in &ids[..20] {
        assert_eq!(put_pin(&base, &a, &selfc, id).await.status(), 204);
    }
    let r = put_pin(&base, &a, &selfc, &ids[20]).await;
    assert_eq!(r.status(), 400);
    let err = r.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(err.contains("20"), "{err}");
    let pins: Value = list_pins(&base, &a, &selfc).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 20);
}

/// 置顶消息被软删除后，列表不再返回它。
#[tokio::test]
async fn p32_deleted_message_drops_from_pins() {
    let base = spawn().await;
    let a = register(&base, &format!("p32i_{}", uniq())).await;
    let selfc = self_conv(&base, &a).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &selfc,
        json!({"client_msg_id": "gone", "content": "将被删除"}),
    )
    .await;
    let mid = m["id"].as_str().unwrap();
    assert_eq!(put_pin(&base, &a, &selfc, mid).await.status(), 204);
    let pinned = next_of(&mut sa, "pins_updated").await;
    assert_eq!(pinned["conversation_id"], selfc);

    let r = http()
        .delete(format!("{base}/v1/messages/{mid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let dropped = next_of(&mut sa, "pins_updated").await;
    assert_eq!(dropped["conversation_id"], selfc);
    let pins: Value = list_pins(&base, &a, &selfc).await.json().await.unwrap();
    assert_eq!(pins.as_array().unwrap().len(), 0);
}

/// 解除好友后的私聊只读，置顶与取消置顶都被拒绝。
#[tokio::test]
async fn p32_unfriended_dm_rejects_pin() {
    let base = spawn().await;
    let a = register(&base, &format!("p32j_{}", uniq())).await;
    let b = register(&base, &format!("p32k_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id": "dm1", "content": "还是好友时"}),
    )
    .await;
    let mid = m["id"].as_str().unwrap();

    let r = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let r = put_pin(&base, &a, &dm, mid).await;
    assert_eq!(r.status(), 403);
    let r = del_pin(&base, &b, &dm, mid).await;
    assert_eq!(r.status(), 403);
}

/// meeting_base_url：默认出现在 server-info；管理员可改；非法值 400；非管理员 403；空串关闭。
#[tokio::test]
async fn p32_meeting_base_url_settings() {
    let (_guard, base) = spawn_named_db(&format!("chat_t_p32_{}", uniq())).await;
    let c = http();
    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["meeting_base_url"], "https://meet.jit.si");

    let admin = register(&base, &format!("p32m_{}", uniq())).await;
    let user = register(&base, &format!("p32n_{}", uniq())).await;

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&user.access))
        .json(&json!({"meeting_base_url": "https://meet.example"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    for bad in [
        "ftp://x",
        "https://meet.example/room id",
        &format!("https://{}", "a".repeat(200)),
    ] {
        let r = c
            .put(format!("{base}/v1/admin/settings"))
            .header("authorization", authz(&admin.access))
            .json(&json!({"meeting_base_url": bad}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{bad}");
    }

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": "https://meet.example/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let saved: Value = r.json().await.unwrap();
    assert_eq!(saved["meeting_base_url"], "https://meet.example");
    let info: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["meeting_base_url"], "https://meet.example");

    // scheme 本身的 // 不能被当成路径尾斜杠剥掉
    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": "https://"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let saved: Value = r.json().await.unwrap();
    assert_eq!(saved["meeting_base_url"], "https://");

    let r = c
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"meeting_base_url": ""}))
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
    assert_eq!(info["meeting_base_url"], "");
}
