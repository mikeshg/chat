use super::*;

// ==================== 会话偏好（Chat V1 / Devin B，§8.3） ====================

pub(crate) async fn put_pref(
    base: &str,
    u: &TestUser,
    conv: &str,
    body: Value,
) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/conversations/{conv}/preferences"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// 在指定会话的列表项中取 preference 字段
fn pref_of(list: &Value, conv: &str) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == conv)
        .unwrap_or_else(|| panic!("conv {conv} missing"))["preference"]
        .clone()
}

/// 偏好读写：缺省行=默认值；部分更新不动其他字段；静音形状校验；置顶排序
#[tokio::test]
async fn prefs_crud_and_order() {
    let base = spawn().await;
    let a = register(&base, &format!("pa_{}", uniq())).await;
    let b = register(&base, &format!("pb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    // 默认：无偏好行 → 全默认字段
    let list = conv_list(&base, &a).await;
    let p = pref_of(&list, &dm);
    assert!(p["pinned_at"].is_null());
    assert!(p["archived_at"].is_null());
    assert!(p["muted_until"].is_null());
    assert_eq!(p["mute_forever"], false);

    // 群比 DM 新 → 初始群在前
    assert_eq!(list[0]["id"], gid);
    assert_eq!(list[1]["id"], dm);

    // 置顶 DM → DM 排前
    let r = put_pref(&base, &a, &dm, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_string());
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], dm, "pinned dm first");

    // 置顶群（pinned_at 更新）→ 群在置顶块内排前
    let r = put_pref(&base, &a, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], gid, "pinned block ordered by pinned_at desc");

    // 取消群置顶 → DM（仍置顶）在前
    let r = put_pref(&base, &a, &gid, json!({"pinned": false})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_null());
    let list = conv_list(&base, &a).await;
    assert_eq!(list[0]["id"], dm);

    // 静音：永久 / 定时互斥形状校验
    let r = put_pref(&base, &a, &dm, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert_eq!(p["mute_forever"], true);
    assert!(p["muted_until"].is_null());

    let until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let r = put_pref(
        &base,
        &a,
        &dm,
        json!({"mute": {"mode": "until", "until": until}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    // 切到定时静音会清掉永久静音
    assert_eq!(p["mute_forever"], false);
    assert!(p["muted_until"].is_string());

    // 非法静音形状全部被拒（serde 数据错 422 / 域校验 400，均属 4xx）
    for body in [
        json!({"mute": {"mode": "until"}}), // 缺 until
        json!({"mute": {"mode": "until", "until": "2000-01-01T00:00:00Z"}}), // 过去时刻
        json!({"mute": {"mode": "until", "until": "not-a-time"}}), // 非法时间
        json!({"mute": {"mode": "forever", "until": until}}), // 冗余 until
        json!({"mute": {"mode": "none", "until": until}}), // none 带 until
        json!({"mute": {"mode": "bogus"}}), // 未知 mode
    ] {
        let r = put_pref(&base, &a, &dm, body).await;
        assert!(r.status().is_client_error(), "body must be rejected: {r:?}");
    }

    // mode none 清空两个静音字段
    let r = put_pref(&base, &a, &dm, json!({"mute": {"mode": "none"}})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert_eq!(p["mute_forever"], false);
    assert!(p["muted_until"].is_null());

    // 部分更新：归档后只清 pinned，archived_at 保持
    let r = put_pref(&base, &a, &dm, json!({"archived": true})).await;
    assert_eq!(r.status(), 200);
    let r = put_pref(&base, &a, &dm, json!({"pinned": false})).await;
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_null());
    assert!(
        p["archived_at"].is_string(),
        "partial update must not clear archive"
    );

    // 幂等：同状态重复 PUT 返回一致字段（状态覆盖，非事件）
    let r1: Value = put_pref(&base, &a, &dm, json!({"pinned": true}))
        .await
        .json()
        .await
        .unwrap();
    let r2: Value = put_pref(&base, &a, &dm, json!({"pinned": true}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r1["pinned_at"].is_string(), true);
    assert_eq!(
        r2["pinned_at"], r1["pinned_at"],
        "repeated pin preserves ordering"
    );
    assert_eq!(r2["archived_at"], r1["archived_at"]);

    // 空 PUT：200，不改任何字段
    let r = put_pref(&base, &a, &dm, json!({})).await;
    assert_eq!(r.status(), 200);
    let p: Value = r.json().await.unwrap();
    assert!(p["pinned_at"].is_string());
    assert!(p["archived_at"].is_string());

    // GET /conversations/preferences：返回本人的偏好行
    let prefs: Value = http()
        .get(format!("{base}/v1/conversations/preferences"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = prefs
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["conversation_id"] == dm)
        .expect("pref entry for dm");
    assert!(entry["preference"]["pinned_at"].is_string());
    assert!(entry["preference"]["archived_at"].is_string());
}

#[tokio::test]
async fn preferences_advance_after_waited_conversation_lock() {
    let (base, st) = spawn_named_state(&format!("chat_t_prefclock_{}", uniq())).await;
    let a = register(&base, &format!("pca_{}", uniq())).await;
    let b = register(&base, &format!("pcb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = mk_group(&base, &a, &[b.id.clone()]).await;
    let cid = uuid::Uuid::parse_str(&conv).unwrap();
    let uid = uuid::Uuid::parse_str(&b.id).unwrap();
    for mention in [false, true] {
        let initial: Value = put_pref(&base, &b, &conv, json!({"pinned":true,"archived":true}))
            .await
            .json()
            .await
            .unwrap();
        let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
            .await
            .unwrap();
        next_of(&mut sa, "presence_init").await;
        let mut barrier = st.pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR UPDATE")
            .bind(cid)
            .execute(&mut *barrier)
            .await
            .unwrap();
        let task = if mention {
            let conv = conv.clone();
            let text = format!("@{} check this", b.username);
            tokio::spawn(async move { ws_send(&mut sa, &conv, json!({"content":text})).await })
        } else {
            let request = http()
                .put(format!("{base}/v1/conversations/{conv}/preferences"))
                .header("authorization", authz(&b.access))
                .json(&json!({"pinned":true,"archived":false}));
            tokio::spawn(async move {
                let r = request.send().await.unwrap();
                assert_eq!(r.status(), 200);
                r.json::<Value>().await.unwrap()
            })
        };
        wait_for_blocker(&st.pool, pid, &task).await;
        assert!(!task.is_finished());
        let previous: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "UPDATE conversation_preferences SET updated_at = clock_timestamp()
             WHERE conversation_id = $1 AND user_id = $2 RETURNING updated_at",
        )
        .bind(cid)
        .bind(uid)
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
        barrier.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let current = pref_of(&conv_list(&base, &b).await, &conv);
        let updated =
            chrono::DateTime::parse_from_rfc3339(current["updated_at"].as_str().unwrap()).unwrap();
        assert!(
            updated > previous,
            "a waited write must advance freshness, mention={mention}"
        );
        assert!(current["archived_at"].is_null());
        assert_eq!(current["pinned_at"], initial["pinned_at"]);
    }
}

/// 权限与同步：非成员/解散/被移出 403；preference_updated 只发本人全部连接
#[tokio::test]
async fn prefs_permissions_and_fanout() {
    let base = spawn().await;
    let a = register(&base, &format!("qa_{}", uniq())).await;
    let b = register(&base, &format!("qb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let c = http();

    // b 开两条连接（模拟多设备）
    let (mut sb1, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb1, "presence_init").await;
    let (mut sb2, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb2, "presence_init").await;
    // a 也在线，确认不会收到别人的 preference_updated
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let r = put_pref(&base, &b, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 200);
    // b 的两条连接都收到
    for s in [&mut sb1, &mut sb2] {
        let f = next_of(s, "preference_updated").await;
        assert_eq!(f["conversation_id"], gid);
        assert!(f["preference"]["pinned_at"].is_string());
    }
    // a 只能收到消息类帧，不能收到 b 的偏好帧：a 发一条消息，读到 message 为止，
    // 途中不得出现 preference_updated
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":gid,"client_msg_id":"x1","content":"hi"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    for _ in 0..8 {
        let m = sa.next().await.unwrap().unwrap();
        if let WsMsg::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_ne!(v["type"], "preference_updated", "pref leaked to a");
            if v["type"] == "message" {
                break;
            }
        }
    }

    // 非成员 → 403；且 GET 列表不暴露他人偏好
    let outsider = register(&base, &format!("qc_{}", uniq())).await;
    let r = put_pref(&base, &outsider, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 403);
    let prefs: Value = c
        .get(format!("{base}/v1/conversations/preferences"))
        .header("authorization", authz(&outsider.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(prefs
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["conversation_id"] != gid));

    // 被移出群 → 403
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}/members/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = put_pref(&base, &b, &gid, json!({"pinned": false})).await;
    assert_eq!(r.status(), 403, "kicked member must not write prefs");

    // 群主解散 → 任何人写偏好 403
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = put_pref(&base, &a, &gid, json!({"pinned": true})).await;
    assert_eq!(r.status(), 403, "dissolved conv must not accept prefs");
}

/// 归档规则：普通消息不解除归档；@本人消息解除归档并向被提及者推 preference_updated；
/// 静音不抑制未读数/mention 标记；重连后列表仍携带偏好（目录刷新即可恢复）
#[tokio::test]
async fn prefs_archive_and_mute_semantics() {
    let base = spawn().await;
    let a = register(&base, &format!("ma_{}", uniq())).await;
    let b = register(&base, &format!("mb_{}", uniq())).await;
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

    // b 归档群
    let r = put_pref(&base, &b, &gid, json!({"archived": true})).await;
    assert_eq!(r.status(), 200);
    next_of(&mut sb, "preference_updated").await;

    // 普通消息不解除归档
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n1","content":"plain"}),
    )
    .await;
    next_of(&mut sb, "message").await;
    let list = conv_list(&base, &b).await;
    assert!(
        pref_of(&list, &gid)["archived_at"].is_string(),
        "plain message must not unarchive"
    );
    // 未读数仍照常累计
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == gid)
            .unwrap()["unread_count"]
            .as_i64()
            .unwrap(),
        1
    );

    // @本人 → 解除归档，b 收到 preference_updated（archived_at=null）
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n2","content":format!("@{} ping", b.username)}),
    )
    .await;
    let f = next_of(&mut sb, "preference_updated").await;
    assert_eq!(f["conversation_id"], gid);
    assert!(f["preference"]["archived_at"].is_null());
    let list = conv_list(&base, &b).await;
    assert!(
        pref_of(&list, &gid)["archived_at"].is_null(),
        "mention must unarchive"
    );

    // 静音：未读数与 mention 标记照常更新（静音只影响应用内提示，属客户端）
    let r = put_pref(&base, &b, &gid, json!({"mute": {"mode": "forever"}})).await;
    assert_eq!(r.status(), 200);
    next_of(&mut sb, "preference_updated").await;
    let _ = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"n3","content":format!("@{} again", b.username)}),
    )
    .await;
    next_of(&mut sb, "message").await;
    let list = conv_list(&base, &b).await;
    let item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == gid)
        .unwrap();
    assert!(item["unread_count"].as_i64().unwrap() >= 1);
    assert_eq!(
        item["has_mention"], true,
        "mute must not suppress mention flag"
    );

    // 重连/目录刷新：偏好仍在
    drop(sb);
    let (mut sb2, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb2, "presence_init").await;
    let list = conv_list(&base, &b).await;
    assert_eq!(pref_of(&list, &gid)["mute_forever"], true);
}
