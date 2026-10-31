use super::*;

// ==================== 轻量投票（Chat V1 / Devin C，§8.4） ====================

async fn post_poll(base: &str, u: &TestUser, conv: &str, body: Value) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/conversations/{conv}/polls"))
        .header("authorization", authz(&u.access))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn vote(base: &str, u: &TestUser, mid: &str, ids: &[String]) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/messages/{mid}/poll/votes"))
        .header("authorization", authz(&u.access))
        .json(&json!({"option_ids": ids}))
        .send()
        .await
        .unwrap()
}

async fn close_poll(base: &str, u: &TestUser, mid: &str) -> reqwest::Response {
    http()
        .post(format!("{base}/v1/messages/{mid}/poll/close"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap()
}

fn opt_ids(m: &Value) -> Vec<String> {
    m["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

fn opt_by_label<'a>(m: &'a Value, label: &str) -> &'a Value {
    m["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["label"] == label)
        .unwrap()
}

/// 创建：仅群聊成员、形状校验、kind/content 占位、client_msg_id 幂等、不可伪造
#[tokio::test]
async fn poll_create_validate() {
    let base = spawn().await;
    let a = register(&base, &format!("qa_{}", uniq())).await;
    let b = register(&base, &format!("qb_{}", uniq())).await;
    let out = register(&base, &format!("qx_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    // 正常创建：kind='poll'，content='[投票] question'，seq 照常分配
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":" 午饭去哪 ","options":[" A ","B","C"],"client_msg_id":"pv1",
               "expires_at":"2999-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(m["kind"], "poll");
    assert_eq!(m["content"], "[投票] 午饭去哪"); // 问题 trim 后入占位文本
    // UI3：建群已占 seq 1（group_created），本条 poll 为 seq 2
    assert_eq!(m["seq"].as_i64().unwrap(), 2);
    let p = &m["poll"];
    assert_eq!(p["question"], "午饭去哪");
    assert_eq!(p["allow_multiple"], false);
    assert_eq!(p["anonymous"], false);
    assert_eq!(p["expired"], false);
    assert_eq!(p["version"].as_i64().unwrap(), 0);
    assert_eq!(p["total_voters"].as_i64().unwrap(), 0);
    assert_eq!(p["options"].as_array().unwrap().len(), 3);
    assert_eq!(p["options"][0]["index"].as_i64().unwrap(), 0);
    assert_eq!(p["options"][0]["label"], "A"); // 选项 trim 落库
    assert!(p["expires_at"].is_string());
    assert_eq!(p["my_option_ids"].as_array().unwrap().len(), 0);

    // client_msg_id 幂等：同键重试返回同一条消息，不重复落库
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"午饭去哪","options":["A","B","C"],"client_msg_id":"pv1"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m2: Value = r.json().await.unwrap();
    assert_eq!(m2["id"], m["id"]);
    let page = history(&base, &a.access, &gid).await;
    let msgs = page["messages"].as_array().unwrap();
    // system group_created + poll（history 按 seq DESC，[0] 为最新 poll）
    assert_eq!(msgs.len(), 2);
    assert!(
        msgs.iter().any(|x| x["kind"] == "system")
            && msgs.iter().any(|x| x["kind"] == "poll" && x["poll"].is_object()),
        "history must contain one system and one poll: {msgs:?}"
    );

    // DM 拒绝（poll 仅群聊）
    let dm = dm_conv(&base, &a).await;
    let r = post_poll(
        &base,
        &a,
        &dm,
        json!({"question":"q","options":["x","y"],"client_msg_id":"pv-dm"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 非成员拒绝
    let r = post_poll(
        &base,
        &out,
        &gid,
        json!({"question":"q","options":["x","y"],"client_msg_id":"pv-out"}),
    )
    .await;
    assert_eq!(r.status(), 403);

    // 形状校验：问题空/超长、选项数、选项空/超长/去重后不足
    for (i, body) in [
        json!({"question":"","options":["x","y"]}),
        json!({"question":"q","options":["x"]}),
        json!({"question":"q","options":["x","x"]}), // 去重后不足 2
        json!({"question":"q","options":["x","  "]}), // trim 后空
        json!({"question":"q","options":vec!["x"; 11]}),
        json!({"question":"q","options":["x","y"],"expires_at":"2000-01-01T00:00:00Z"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut b = body;
        b["client_msg_id"] = json!(format!("bad{i}"));
        let r = post_poll(&base, &a, &gid, b).await;
        assert_eq!(r.status(), 400, "case {i} should be 400");
    }
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"x".repeat(201),"options":["a","b"],"client_msg_id":"badq"}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":["a","x".repeat(101)],"client_msg_id":"bado"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // send_message 不能伪造 kind='poll'（kind 由服务端推导，poll 负载不经 WS 进入）
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let forged = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"forge","content":"[投票] fake","kind":"poll"}),
    )
    .await;
    assert_eq!(forged["kind"], "text");
    assert!(forged["poll"].is_null());
}

/// 平面字符边界：按 Unicode 字符数（非字节）校验——𠀀 为 BMP 外 4 字节字符，
/// 问题 200/201、选项 100/101、消息 4000/4001 恰好跨线各一对
#[tokio::test]
async fn boundary_char_counts() {
    let base = spawn().await;
    let a = register(&base, &format!("ba_{}", uniq())).await;
    let b = register(&base, &format!("bb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;
    let dm = dm_conv(&base, &a).await;
    let wide = "𠀀";

    // 投票问题：200 字符通过、201 拒绝（𠀀 UTF-8 为 4 字节，按字节算会误判）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":wide.repeat(200),"options":["x","y"],"client_msg_id":"bnd-q200"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(m["poll"]["question"].as_str().unwrap().chars().count(), 200);
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":wide.repeat(201),"options":["x","y"],"client_msg_id":"bnd-q201"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 投票选项：100 字符通过、101 拒绝
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":[wide.repeat(100),"y"],"client_msg_id":"bnd-o100"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let m: Value = r.json().await.unwrap();
    assert_eq!(
        m["poll"]["options"][0]["label"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        100
    );
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"q","options":[wide.repeat(101),"y"],"client_msg_id":"bnd-o101"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // 消息正文：4000 字符通过、4001 拒绝（WS 发送路径；拒绝走 error 帧）
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let m = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"bnd-m4000","content":wide.repeat(4000)}),
    )
    .await;
    assert_eq!(m["content"].as_str().unwrap().chars().count(), 4000);
    sa.send(WsMsg::Text(
        json!({"type":"send_message","conversation_id":dm,
               "client_msg_id":"bnd-m4001","content":wide.repeat(4001)})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let err = next_of(&mut sa, "error").await;
    assert_eq!(err["code"], "bad_message");
}

/// 投票：替换语义、相同集合幂等、单/多选约束、跨 poll 选项归属、撤票、seq 不变
#[tokio::test]
async fn poll_vote_replace() {
    let base = spawn().await;
    let a = register(&base, &format!("va_{}", uniq())).await;
    let b = register(&base, &format!("vb_{}", uniq())).await;
    let c = register(&base, &format!("vc_{}", uniq())).await;
    let out = register(&base, &format!("vx_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &c).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), c.id.clone()]).await;

    // 多选投票
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"团建","options":["爬山","桌游","烧烤"],"allow_multiple":true,
               "client_msg_id":"v1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);
    let seq = m["seq"].as_i64().unwrap();

    // b 投 [A] → count/total/my/version 全部正确
    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);
    assert_eq!(opt_by_label(&v, "爬山")["count"].as_i64().unwrap(), 1);
    assert_eq!(v["poll"]["my_option_ids"].as_array().unwrap()[0], ids[0]);
    assert_eq!(v["seq"].as_i64().unwrap(), seq); // 消息 seq 不动

    // 相同集合幂等：不升版本
    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);

    // 替换 [A]→[A,B]：多选累加本人集合
    let r = vote(&base, &b, mid, &[ids[0].clone(), ids[1].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 2);
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);
    assert_eq!(opt_by_label(&v, "桌游")["count"].as_i64().unwrap(), 1);

    // 替换 [A,B]→[C]：旧选项计数回落（替换而非累加）
    let r = vote(&base, &b, mid, &[ids[2].clone()]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(opt_by_label(&v, "爬山")["count"].as_i64().unwrap(), 0);
    assert_eq!(opt_by_label(&v, "桌游")["count"].as_i64().unwrap(), 0);
    assert_eq!(opt_by_label(&v, "烧烤")["count"].as_i64().unwrap(), 1);

    // 撤票：空集合
    let r = vote(&base, &b, mid, &[]).await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 0);
    assert_eq!(v["poll"]["my_option_ids"].as_array().unwrap().len(), 0);

    // c 全选 3 项（多选上限=选项数）
    let r = vote(&base, &c, mid, &ids).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["poll"]["total_voters"].as_i64().unwrap(), 1);

    // 非成员投票 → 403
    let r = vote(&base, &out, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 403);

    // 他 poll 的选项 id → 400（应用层校验 + 复合外键兜底）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"p2","options":["x","y"],"client_msg_id":"v2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let foreign = opt_ids(&m2)[0].clone();
    let r = vote(&base, &b, mid, &[foreign]).await;
    assert_eq!(r.status(), 400);
    let r = vote(&base, &b, mid, &[uuid::Uuid::new_v4().to_string()]).await;
    assert_eq!(r.status(), 400);

    // 单选约束
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"单选","options":["是","否"],"client_msg_id":"v3"}),
    )
    .await;
    let sm: Value = r.json().await.unwrap();
    let smid = sm["id"].as_str().unwrap();
    let sids = opt_ids(&sm);
    let r = vote(&base, &b, smid, &sids).await;
    assert_eq!(r.status(), 400); // 单选至多 1 项
    let r = vote(&base, &b, smid, &[sids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    // 单选换票：直接替换
    let r = vote(&base, &b, smid, &[sids[1].clone()]).await;
    let v2: Value = r.json().await.unwrap();
    assert_eq!(v2["poll"]["my_option_ids"].as_array().unwrap()[0], sids[1]);
    assert_eq!(
        v2["poll"]["version"].as_i64().unwrap(),
        v["poll"]["version"].as_i64().unwrap() + 1
    );

    // 非 poll 消息投票 → 404
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let t = ws_send(&mut sa, &gid, json!({"client_msg_id":"t","content":"hi"})).await;
    let tid = t["id"].as_str().unwrap();
    let r = vote(&base, &b, tid, &[]).await;
    assert_eq!(r.status(), 404);

    // 会话 last_seq 不被投票推进（不产生新消息）；未读不受 mutation 影响
    let list = conv_list(&base, &c).await;
    let item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == gid)
        .unwrap();
    assert_eq!(item["last_seq"].as_i64().unwrap(), 5); // 1 system + 3 poll + 1 text
}

/// 关闭：创建者/群主权限、幂等、关后拒投；投票行与消息版本同步
#[tokio::test]
async fn poll_close_perm() {
    let base = spawn().await;
    let a = register(&base, &format!("ca_{}", uniq())).await;
    let b = register(&base, &format!("cb_{}", uniq())).await;
    let c = register(&base, &format!("cc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &c).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), c.id.clone()]).await;

    // b（普通成员）发起；c（非创建者非群主）关闭 → 403
    let r = post_poll(
        &base,
        &b,
        &gid,
        json!({"question":"q1","options":["x","y"],"client_msg_id":"c1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let r = close_poll(&base, &c, mid).await;
    assert_eq!(r.status(), 403);

    // 创建者 b 关闭 → 200，closed_at 落定、版本 +1
    let r = close_poll(&base, &b, mid).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["poll"]["closed_at"].is_string());
    assert_eq!(v["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v["edit_version"].as_i64().unwrap(), 1);

    // 重复关闭幂等：版本不再升
    let r = close_poll(&base, &b, mid).await;
    assert_eq!(r.status(), 200);
    let v2: Value = r.json().await.unwrap();
    assert_eq!(v2["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(v2["edit_version"].as_i64().unwrap(), 1);

    // 关闭后投票 → 409，结果仍可读
    let ids = opt_ids(&m);
    let r = vote(&base, &a, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &a.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert!(pm["poll"]["closed_at"].is_string());

    // 群主（非创建者）也可关闭
    let r = post_poll(
        &base,
        &b,
        &gid,
        json!({"question":"q2","options":["x","y"],"client_msg_id":"c2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let r = close_poll(&base, &a, m2["id"].as_str().unwrap()).await;
    assert_eq!(r.status(), 200);

    // 非 poll 消息关闭 → 404
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let t = ws_send(&mut sa, &gid, json!({"client_msg_id":"t2","content":"hi"})).await;
    let r = close_poll(&base, &a, t["id"].as_str().unwrap()).await;
    assert_eq!(r.status(), 404);
}

/// 过期按 DB now() 判定；软删除墓碑掩去 poll 且拒绝后续投票/关闭
#[tokio::test]
async fn poll_expire_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("xa_{}", uniq())).await;
    let b = register(&base, &format!("xb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"限时","options":["x","y"],"client_msg_id":"e1",
               "expires_at":"2999-01-01T00:00:00Z"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);

    // 回拨 expires_at 到过去 → DB now() 判过期
    let pool = PgPoolOptions::new().connect(&test_db_url()).await.unwrap();
    sqlx::query("UPDATE polls SET expires_at = now() - interval '1 second' WHERE message_id = $1")
        .bind(uuid::Uuid::parse_str(mid).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let r = vote(&base, &b, mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["expired"], true);
    // 过期后创建者仍可关闭（展示终态），关闭后依旧拒投
    let r = close_poll(&base, &a, mid).await;
    assert_eq!(r.status(), 200);

    // 软删除：墓碑掩去 poll；投票/关闭一律拒绝
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"tomb","options":["x","y"],"client_msg_id":"e2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let mid2 = m2["id"].as_str().unwrap();
    let ids2 = opt_ids(&m2);
    let r = http()
        .delete(format!("{base}/v1/messages/{mid2}"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = vote(&base, &b, mid2, &[ids2[0].clone()]).await;
    assert_eq!(r.status(), 409);
    let r = close_poll(&base, &a, mid2).await;
    assert_eq!(r.status(), 409);
    let page = history(&base, &a.access, &gid).await;
    let tm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid2)
        .unwrap();
    assert_eq!(tm["content"], "消息已撤回");
    assert!(tm["poll"].is_null()); // 墓碑掩去 poll（与正文/附件同口径）
}

/// 匿名隐私：voters 永不出库（REST/WS/广播全路径掩蔽）；my_option_ids 逐人个性化；
/// WS 同步复用 message / message_updated 帧
#[tokio::test]
async fn poll_anonymous_and_ws() {
    let base = spawn().await;
    let a = register(&base, &format!("aa_{}", uniq())).await;
    let b = register(&base, &format!("ab_{}", uniq())).await;
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

    // REST 创建匿名投票 → 双端收 message 帧（kind='poll'，视图含 poll）
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"匿","options":["x","y"],"anonymous":true,"client_msg_id":"an1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap().to_string();
    let ids = opt_ids(&m);
    let fa = next_of(&mut sa, "message").await;
    let fb = next_of(&mut sb, "message").await;
    assert_eq!(fa["message"]["kind"], "poll");
    assert_eq!(fb["message"]["poll"]["anonymous"], true);

    // b 投票 → 双端 message_updated：匿名掩蔽 voters，my_option_ids 个性化
    let r = vote(&base, &b, &mid, &[ids[0].clone()]).await;
    assert_eq!(r.status(), 200);
    let resp: Value = r.json().await.unwrap();
    for o in resp["poll"]["options"].as_array().unwrap() {
        assert_eq!(o["voters"].as_array().unwrap().len(), 0, "anonymous voters");
    }
    assert_eq!(resp["poll"]["my_option_ids"].as_array().unwrap()[0], ids[0]);
    assert_eq!(resp["poll"]["total_voters"].as_i64().unwrap(), 1);

    let ua = next_of(&mut sa, "message_updated").await;
    let ub = next_of(&mut sb, "message_updated").await;
    // a（非投票者）视角：无 voter 泄漏、my 为空
    assert_eq!(
        ua["message"]["poll"]["options"][0]["voters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ua["message"]["poll"]["my_option_ids"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ua["message"]["poll"]["options"][0]["count"]
            .as_i64()
            .unwrap(),
        1
    );
    // b 视角：本人选择可见，voters 仍掩蔽
    assert_eq!(
        ub["message"]["poll"]["my_option_ids"].as_array().unwrap()[0],
        ids[0]
    );
    assert_eq!(
        ub["message"]["poll"]["options"][0]["voters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // 非匿名投票：voters 按读模型可见
    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"公开","options":["x","y"],"client_msg_id":"an2"}),
    )
    .await;
    let m2: Value = r.json().await.unwrap();
    let mid2 = m2["id"].as_str().unwrap().to_string();
    let ids2 = opt_ids(&m2);
    next_of(&mut sa, "message").await;
    next_of(&mut sb, "message").await;
    let r = vote(&base, &b, &mid2, &[ids2[0].clone()]).await;
    let v: Value = r.json().await.unwrap();
    let voters = opt_by_label(&v, "x")["voters"].as_array().unwrap();
    assert_eq!(voters[0], b.id);
    let ua2 = next_of(&mut sa, "message_updated").await;
    assert_eq!(ua2["message"]["poll"]["options"][0]["voters"][0], b.id);

    // 版本合并契约：message_updated 携带完整视图（poll + content + reactions 同帧）
    assert_eq!(ua2["message"]["poll"]["version"].as_i64().unwrap(), 1);
    assert_eq!(ua2["message"]["content"], "[投票] 公开");
}

#[tokio::test]
async fn poll_creation_replay_is_private_over_rest_and_ws() {
    let base = spawn().await;
    let a = register(&base, &format!("pra_{}", uniq())).await;
    let b = register(&base, &format!("prb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let conv = mk_group(&base, &a, &[b.id.clone()]).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    next_of(&mut sb, "presence_init").await;
    let body = json!({"question":"private choices","options":["x","y"],
        "anonymous":true,"client_msg_id":"private-replay"});
    let r = post_poll(&base, &a, &conv, body.clone()).await;
    assert_eq!(r.status(), 200);
    let original: Value = r.json().await.unwrap();
    let mid = original["id"].as_str().unwrap();
    let ids = opt_ids(&original);
    assert_eq!(next_of(&mut sa, "message").await["message"]["id"], mid);
    assert_eq!(next_of(&mut sb, "message").await["message"]["id"], mid);
    for (user, choice) in [(&a, &ids[0]), (&b, &ids[1])] {
        assert_eq!(
            vote(&base, user, mid, &[choice.clone()]).await.status(),
            200
        );
        next_of(&mut sa, "message_updated").await;
        next_of(&mut sb, "message_updated").await;
    }
    let r = http()
        .put(format!(
            "{base}/v1/messages/{mid}/reactions/{}",
            urlencoded("\u{1f44d}")
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let a_view = next_of(&mut sa, "reaction_update").await["message"].clone();
    let b_view = next_of(&mut sb, "reaction_update").await["message"].clone();
    assert_eq!(a_view["poll"]["my_option_ids"], json!([ids[0]]));
    assert_eq!(b_view["poll"]["my_option_ids"], json!([ids[1]]));
    assert_eq!(a_view["reactions"][0]["reacted_by_me"], true);
    assert_eq!(b_view["reactions"][0]["reacted_by_me"], false);

    for websocket in [false, true] {
        let replay = if websocket {
            ws_send(
                &mut sa,
                &conv,
                json!({"client_msg_id":"private-replay","content":"retry"}),
            )
            .await
        } else {
            let r = post_poll(&base, &a, &conv, body.clone()).await;
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        };
        assert_eq!(replay["id"], original["id"]);
        assert_eq!(replay["seq"], original["seq"]);
        assert_eq!(replay, a_view);
        // Sync is a queue fence: any replay broadcast would precede this response.
        sb.send(WsMsg::Text(
            json!({"type":"sync","conversation_id":conv,"last_seq":0})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let sync = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let WsMsg::Text(t) = sb.next().await.unwrap().unwrap() {
                    let f: Value = serde_json::from_str(&t).unwrap();
                    assert_ne!(
                        f["type"], "message",
                        "replay must not broadcast a creation view"
                    );
                    assert_ne!(f["type"], "preference_updated", "replay must not re-notify");
                    if f["type"] == "sync_messages" {
                        break f;
                    }
                }
            }
        })
        .await
        .expect("sync fence timed out");
        // sync 从 last_seq=0 会带上 UI3 group_created；只断言其中的 poll 视图
        let sync_msgs = sync["messages"].as_array().unwrap();
        let poll_msg = sync_msgs
            .iter()
            .find(|x| x["id"] == mid)
            .unwrap_or_else(|| panic!("sync missing poll {mid}: {sync_msgs:?}"));
        assert_eq!(poll_msg, &b_view);
        for option in poll_msg["poll"]["options"].as_array().unwrap() {
            assert_eq!(option["voters"], json!([]));
        }
    }
}

/// 并发替换：消息行锁串行化，后提交者覆盖先提交者，终态收敛且版本按真实变更计数
#[tokio::test]
async fn poll_concurrent_votes() {
    let base = spawn().await;
    let a = register(&base, &format!("za_{}", uniq())).await;
    let b = register(&base, &format!("zb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let gid = mk_group(&base, &a, &[b.id.clone()]).await;

    let r = post_poll(
        &base,
        &a,
        &gid,
        json!({"question":"cc","options":["x","y","z"],"client_msg_id":"cc1"}),
    )
    .await;
    let m: Value = r.json().await.unwrap();
    let mid = m["id"].as_str().unwrap();
    let ids = opt_ids(&m);

    // 同一用户并发两个不同选择 → 串行化，两次都是真实变更
    let pick0 = [ids[0].clone()];
    let pick1 = [ids[1].clone()];
    let (r1, r2) = tokio::join!(vote(&base, &b, mid, &pick0), vote(&base, &b, mid, &pick1),);
    assert_eq!(r1.status(), 200);
    assert_eq!(r2.status(), 200);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["version"].as_i64().unwrap(), 2);
    assert_eq!(pm["poll"]["total_voters"].as_i64().unwrap(), 1);
    let counts: i64 = pm["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["count"].as_i64().unwrap())
        .sum();
    assert_eq!(counts, 1, "最终只剩一个选项被投（替换语义收敛）");
    assert_eq!(pm["poll"]["my_option_ids"].as_array().unwrap().len(), 1);

    // 并发相同选择 → 只有一次真实变更
    let pick2 = [ids[2].clone()];
    let (r3, r4) = tokio::join!(vote(&base, &b, mid, &pick2), vote(&base, &b, mid, &pick2),);
    assert_eq!(r3.status(), 200);
    assert_eq!(r4.status(), 200);
    let page = history(&base, &b.access, &gid).await;
    let pm = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == mid)
        .unwrap();
    assert_eq!(pm["poll"]["version"].as_i64().unwrap(), 3); // 2 + 1 次真实变更
    assert_eq!(opt_by_label(pm, "z")["count"].as_i64().unwrap(), 1);
}
