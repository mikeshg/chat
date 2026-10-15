use super::*;

// ---------- 管理员/账号角色 ----------

#[tokio::test]
async fn admin_rules() {
    let (_guard, base) = spawn_named_db(&format!("chat_t_adm_{}", uniq())).await;
    let c = http();

    // 首个注册用户 = admin；其余为 user
    let a = register(&base, &format!("ad_{}", uniq())).await;
    let b = register(&base, &format!("bu_{}", uniq())).await;
    let c3 = register(&base, &format!("cu_{}", uniq())).await;
    assert_eq!(me(&base, &a).await["role"], "admin");
    assert_eq!(me(&base, &b).await["role"], "user");

    // 普通用户访问 admin API → 403
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // admin overview
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    let st_code = r.status();
    let ov: Value = r.json().await.unwrap();
    eprintln!("overview -> {st_code} body={ov}");
    assert_eq!(st_code, 200);
    assert_eq!(ov["user_count"], 3);
    assert_eq!(ov["admin_count"], 1);

    // 任命 c3 为管理员 → 管理员账号不可禁用
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", c3.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", c3.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 禁用 b → 立即全线拒绝（REST/登录/搜索排除），数据保留
    let r = c
        .post(format!("{base}/v1/admin/users/{}/disable", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // ws ticket 也被拒
    let r = c
        .get(format!("{base}/v1/ws/ticket"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 搜索排除
    let found: Value = c
        .get(format!("{base}/v1/users/search?q={}", b.username))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 0);

    // 启用 → 账号恢复，但旧 token 已在禁用时随 session_version 作废，必须重新登录
    let r = c
        .post(format!("{base}/v1/admin/users/{}/enable", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/users/me"))
        .header("authorization", authz(&b.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 撤销 a 的管理员（还有 c3）→ 成功；a 失去 admin 访问权
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", a.id))
        .header("authorization", authz(&a.access))
        .json(&json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!("{base}/v1/admin/overview"))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // 末位保护：c3 是最后一个管理员，撤销自己 → 400
    let r = c
        .post(format!("{base}/v1/admin/users/{}/role", c3.id))
        .header("authorization", authz(&c3.access))
        .json(&json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 重置密码：旧密码死、新密码活
    let r = c
        .post(format!("{base}/v1/admin/users/{}/password", b.id))
        .header("authorization", authz(&c3.access))
        .json(&json!({"password": "newpass999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "secret123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = c
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": b.username, "password": "newpass999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}
