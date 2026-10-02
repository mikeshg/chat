use super::*;

// ===== V2 P3-1 =====
// 全局消息搜索（GET /v1/search）+ 会话内搜索墓碑修复

/// GET /v1/search：params 交给 reqwest 编码（含中文/百分号/RFC3339），返回 (status, body)
async fn gsearch(base: &str, at: &str, params: &[(&str, String)]) -> (u16, Value) {
    let r = http()
        .get(format!("{base}/v1/search"))
        .header("authorization", authz(at))
        .query(params)
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

fn gids(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

/// 全局搜索：跨会话命中按 created_at 倒序；非成员会话不出现；墓碑不命中；
/// 会话内搜索同步修好（回归）；退群后立即搜不到该会话。
#[tokio::test]
async fn p31_search_global_scope_and_tombstone() {
    let base = spawn().await;
    let a = register(&base, &format!("sa_{}", uniq())).await;
    let b = register(&base, &format!("sb_{}", uniq())).await;
    let cx = register(&base, &format!("sc_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    befriend(&base, &a, &cx).await;
    befriend(&base, &b, &cx).await;
    let dm = dm_conv(&base, &a).await;
    let gid = mk_group(&base, &a, &[b.id.clone(), cx.id.clone()]).await;

    let c = http();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let m1 = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"k1","content":"周报 alpha"}),
    )
    .await;
    let m2 = ws_send(
        &mut sb,
        &gid,
        json!({"client_msg_id":"k2","content":"周报 beta"}),
    )
    .await;
    // 将被删除的命中消息；另一个 a 非成员的群里含同关键词的消息
    let gone = ws_send(
        &mut sa,
        &gid,
        json!({"client_msg_id":"k3","content":"周报 gone"}),
    )
    .await;
    let g2 = mk_group(&base, &b, &[cx.id.clone()]).await;
    ws_send(
        &mut sb,
        &g2,
        json!({"client_msg_id":"k4","content":"周报 secret"}),
    )
    .await;

    let r = c
        .delete(format!(
            "{base}/v1/messages/{}",
            gone["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 1) 两个会话都命中，按 created_at DESC（beta 后发 → 在前）；墓碑与外人会话不进结果
    let (st, page) = gsearch(&base, &a.access, &[("q", "周报".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            m2["id"].as_str().unwrap().to_string(),
            m1["id"].as_str().unwrap().to_string()
        ]
    );
    assert_eq!(page["has_more"], false);
    // 客户端按 conversation_id 对照本地会话目录显示会话名
    assert_eq!(
        page["messages"][0]["conversation_id"].as_str().unwrap(),
        gid
    );

    // 2) 搜墓碑占位文字不命中已删除消息
    let (st, page) = gsearch(&base, &a.access, &[("q", "消息已删除".into())]).await;
    assert_eq!(st, 200);
    assert!(gids(&page).is_empty(), "tombstone must not match");

    // 3) 会话内搜索回归：同样不返回墓碑、占位文字不命中
    let found: Value = c
        .get(format!(
            "{base}/v1/conversations/{gid}/messages/search?q=周报"
        ))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = found.as_array().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"].as_str().unwrap(), m2["id"].as_str().unwrap());
    let found: Value = c
        .get(format!(
            "{base}/v1/conversations/{gid}/messages/search?q=已删除"
        ))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(found.as_array().unwrap().is_empty());

    // 4) 非成员会话不可定向搜索；退群后历史立即搜不到
    let (st, _) = gsearch(&base, &a.access, &[("conversation_id", g2.clone())]).await;
    assert_eq!(st, 403);
    let r = c
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let (st, page) = gsearch(&base, &a.access, &[("q", "周报".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![m1["id"].as_str().unwrap().to_string()]);
    let (st, _) = gsearch(&base, &a.access, &[("conversation_id", gid.clone())]).await;
    assert_eq!(st, 403);
}

/// 筛选：conversation_id+kind、sender_id、from/to（含 from、不含 to）
#[tokio::test]
async fn p31_search_filters() {
    let base = spawn().await;
    let a = register(&base, &format!("fa_{}", uniq())).await;
    let b = register(&base, &format!("fb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;

    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;
    let (mut sb, _) = tokio_tungstenite::connect_async(ws_url(&base, &b).await)
        .await
        .unwrap();
    next_of(&mut sb, "presence_init").await;

    let t1 = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"f1","content":"找找这段文字"}),
    )
    .await;
    let (st, att) = upload_att(
        &base,
        &dm,
        &a.access,
        "p.png",
        "image/png",
        png_fixture(60, 40),
    )
    .await;
    assert_eq!(st, 200);
    let img = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"f2","content":"","attachment_id":att["id"].as_str().unwrap()}),
    )
    .await;
    let t2 = ws_send(
        &mut sb,
        &dm,
        json!({"client_msg_id":"f3","content":"对方的话"}),
    )
    .await;

    // conversation_id + kind=image → 只有该会话的图片消息
    let (st, page) = gsearch(
        &base,
        &a.access,
        &[("conversation_id", dm.clone()), ("kind", "image".into())],
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![img["id"].as_str().unwrap().to_string()]);
    assert_eq!(page["messages"][0]["kind"], "image");

    // sender_id → 只看 b 发的
    let (st, page) = gsearch(&base, &a.access, &[("sender_id", b.id.clone())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![t2["id"].as_str().unwrap().to_string()]);

    // kind=text → 两条文本（图片不算）
    let (st, page) = gsearch(&base, &a.access, &[("kind", "text".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            t2["id"].as_str().unwrap().to_string(),
            t1["id"].as_str().unwrap().to_string()
        ]
    );

    // from/to：以图片消息的 created_at 为界——含 from、不含 to
    let pivot = img["created_at"].as_str().unwrap().to_string();
    let (st, page) = gsearch(&base, &a.access, &[("from", pivot.clone())]).await;
    assert_eq!(st, 200);
    assert_eq!(
        gids(&page),
        vec![
            t2["id"].as_str().unwrap().to_string(),
            img["id"].as_str().unwrap().to_string()
        ]
    );
    let (st, page) = gsearch(&base, &a.access, &[("to", pivot)]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![t1["id"].as_str().unwrap().to_string()]);
}

/// 翻页：5 条命中、limit=2 分三页取完，不重不漏，has_more 正确
#[tokio::test]
async fn p31_search_pagination() {
    let base = spawn().await;
    let a = register(&base, &format!("pa_{}", uniq())).await;
    let b = register(&base, &format!("pb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let mut sent = Vec::new();
    for i in 0..5 {
        let m = ws_send(
            &mut sa,
            &dm,
            json!({"client_msg_id":format!("pg{i}"),"content":format!("分页 关键词 第{i}条")}),
        )
        .await;
        sent.push(m["id"].as_str().unwrap().to_string());
    }

    let mut got: Vec<String> = Vec::new();
    let mut cursor: Option<(String, String)> = None;
    for page_no in 0..3 {
        let mut params = vec![("q", "关键词".into()), ("limit", "2".into())];
        if let Some((before, before_id)) = &cursor {
            params.push(("before", before.clone()));
            params.push(("before_id", before_id.clone()));
        }
        let (st, page) = gsearch(&base, &a.access, &params).await;
        assert_eq!(st, 200);
        let msgs = page["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        cursor = Some((
            last["created_at"].as_str().unwrap().to_string(),
            last["id"].as_str().unwrap().to_string(),
        ));
        assert_eq!(
            page["has_more"].as_bool().unwrap(),
            page_no < 2,
            "page {page_no} has_more"
        );
        got.extend(gids(&page));
    }
    // 全部取到、无重复、按 created_at DESC = 发送顺序倒序
    let mut expect = sent.clone();
    expect.reverse();
    assert_eq!(got, expect);
}

/// 参数校验与通配符转义：全空 400、q 超长 400、非法 kind 400、非成员 403、%/_ 按字面量
#[tokio::test]
async fn p31_search_validation_and_escape() {
    let base = spawn().await;
    let a = register(&base, &format!("va_{}", uniq())).await;
    let b = register(&base, &format!("vb_{}", uniq())).await;
    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    next_of(&mut sa, "presence_init").await;

    let pct = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e1","content":"进度 100% 完成"}),
    )
    .await;
    let us = ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e2","content":"变量 a_b 命名"}),
    )
    .await;
    ws_send(
        &mut sa,
        &dm,
        json!({"client_msg_id":"e3","content":"变量 axb 不命中"}),
    )
    .await;

    // % 与 _ 按字面量，不当通配符
    let (st, page) = gsearch(&base, &a.access, &[("q", "100%".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![pct["id"].as_str().unwrap().to_string()]);
    let (st, page) = gsearch(&base, &a.access, &[("q", "a_b".into())]).await;
    assert_eq!(st, 200);
    assert_eq!(gids(&page), vec![us["id"].as_str().unwrap().to_string()]);

    // 参数全空 → 400；q 全空白且无筛选 → 400；q 101 字 → 400；非法 kind → 400
    let (st, _) = gsearch(&base, &a.access, &[]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "   ".into())]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "x".repeat(101))]).await;
    assert_eq!(st, 400);
    let (st, _) = gsearch(&base, &a.access, &[("q", "x".repeat(100))]).await;
    assert_eq!(st, 200);
    let (st, _) = gsearch(&base, &a.access, &[("kind", "bogus".into())]).await;
    assert_eq!(st, 400);
    // 不存在的会话 / 非成员会话 → 403
    let (st, _) = gsearch(
        &base,
        &a.access,
        &[("conversation_id", uuid::Uuid::new_v4().to_string())],
    )
    .await;
    assert_eq!(st, 403);
}
