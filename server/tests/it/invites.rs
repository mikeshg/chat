use super::*;

// ===== V2 P1-2 =====
// 注册开关 + 邀请码。全部用 spawn_named_db 独立库：改的是全局设置，不能干扰并发测试。

/// 带可选邀请码的注册请求（返回原始响应；不像 register() 断言 200）
async fn register_invite(base: &str, name: &str, invite: Option<&str>) -> reqwest::Response {
    let mut body = json!({"username": name, "password": "secret123", "display_name": name});
    if let Some(c) = invite {
        body["invite_code"] = json!(c);
    }
    http()
        .post(format!("{base}/v1/auth/register"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// 直接 SQL 写 server_settings（覆盖「空库 + closed」等 API 无法到达的时序场景）
async fn set_mode_sql(pool: &sqlx::PgPool, mode: &str) {
    sqlx::query(
        "INSERT INTO server_settings (key, value) VALUES ('registration_mode', to_jsonb($1::text))
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(mode)
    .execute(pool)
    .await
    .unwrap();
}

async fn put_mode(base: &str, admin: &TestUser, mode: &str) -> reqwest::Response {
    http()
        .put(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .json(&json!({"registration_mode": mode}))
        .send()
        .await
        .unwrap()
}

async fn create_invite(base: &str, admin: &TestUser, body: Value) -> Value {
    let r = http()
        .post(format!("{base}/v1/admin/invites"))
        .header("authorization", authz(&admin.access))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "create invite failed");
    r.json().await.unwrap()
}

/// server-info 无需登录可访问，默认 open
#[tokio::test]
async fn p12_server_info_public() {
    let base = spawn_named_db(&format!("chat_t_p12a_{}", uniq())).await;
    let c = http();
    let r: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["registration_mode"], "open");

    // 管理员改模式后公共端点立即反映
    let admin = register(&base, &format!("p12a_{}", uniq())).await;
    let r = put_mode(&base, &admin, "invite").await;
    assert_eq!(r.status(), 200);
    let r: Value = c
        .get(format!("{base}/v1/server-info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["registration_mode"], "invite");
}

/// 非管理员访问 settings / invites → 403；未登录 → 401
#[tokio::test]
async fn p12_admin_endpoints_require_admin() {
    let base = spawn_named_db(&format!("chat_t_p12b_{}", uniq())).await;
    let c = http();
    let _admin = register(&base, &format!("p12b_{}", uniq())).await;
    let u = register(&base, &format!("p12c_{}", uniq())).await;
    let inv_id = uuid::Uuid::new_v4();

    for (method, path) in [
        ("GET", "/v1/admin/settings".to_string()),
        ("PUT", "/v1/admin/settings".to_string()),
        ("GET", "/v1/admin/invites".to_string()),
        ("POST", "/v1/admin/invites".to_string()),
        ("DELETE", format!("/v1/admin/invites/{inv_id}")),
    ] {
        let req = c
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .header("authorization", authz(&u.access))
            .json(&json!({}));
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 403, "{method} {path} 应拒绝普通用户");
        let r = c
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{method} {path} 应拒绝未登录");
    }
}

/// closed：空库时首个用户仍放行（引导例外），之后一律 403
#[tokio::test]
async fn p12_closed_mode_bootstrap() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12d_{}", uniq())).await;
    // 空库直接 SQL 置 closed——等价于部署后没注册过人就关了注册
    set_mode_sql(&st.pool, "closed").await;

    let r = register_invite(&base, &format!("p12d_{}", uniq()), None).await;
    assert_eq!(
        r.status(),
        200,
        "空库引导例外：closed 模式下首个用户仍应注册成功"
    );

    let r = register_invite(&base, &format!("p12e_{}", uniq()), None).await;
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "管理员已关闭注册");
}

/// invite 模式：无码/错码 403；有效码 200 且 uses+1、users.invite_id 写入；撤销/过期 403
#[tokio::test]
async fn p12_invite_mode() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12f_{}", uniq())).await;
    let c = http();
    let admin = register(&base, &format!("p12f_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);

    // 无码 → 403
    let r = register_invite(&base, &format!("p12g_{}", uniq()), None).await;
    assert_eq!(r.status(), 403);
    // 错码 → 403
    let r = register_invite(&base, &format!("p12g_{}", uniq()), Some("NOTEXIST99")).await;
    assert_eq!(r.status(), 403);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "邀请码无效或已失效");

    // 有效码（故意传小写+首尾空白，服务端规范化）→ 200
    let inv = create_invite(&base, &admin, json!({"note": "test"})).await;
    let code = inv["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 10);
    let fuzzy = format!("  {} ", code.to_lowercase());
    let invited_name = format!("p12g_{}", uniq());
    let r = register_invite(&base, &invited_name, Some(&fuzzy)).await;
    assert_eq!(r.status(), 200, "有效码（大小写/空白规范化后）应注册成功");

    // uses +1、users.invite_id 写入
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    let invite_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT invite_id FROM users WHERE username = $1")
            .bind(&invited_name)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(
        invite_id,
        Some(uuid::Uuid::parse_str(inv["id"].as_str().unwrap()).unwrap())
    );

    // 列表视图字段完整：usable=true
    let list: Value = c
        .get(format!("{base}/v1/admin/invites"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &list.as_array().unwrap()[0];
    assert_eq!(row["code"], code);
    assert_eq!(row["uses"], 1);
    assert_eq!(row["usable"], true);
    assert!(!row["created_by_name"].as_str().unwrap().is_empty());

    // 撤销 → 幂等 204，之后码失效
    let r = c
        .delete(format!(
            "{base}/v1/admin/invites/{}",
            inv["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .delete(format!(
            "{base}/v1/admin/invites/{}",
            inv["id"].as_str().unwrap()
        ))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "撤销应幂等");
    let r = register_invite(&base, &format!("p12h_{}", uniq()), Some(&code)).await;
    assert_eq!(r.status(), 403);

    // 过期码 → 403（SQL 直接把 expires_at 拨到过去）
    let inv2 = create_invite(&base, &admin, json!({"expires_in_hours": 24})).await;
    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 hour' WHERE id = $1")
        .bind(uuid::Uuid::parse_str(inv2["id"].as_str().unwrap()).unwrap())
        .execute(&st.pool)
        .await
        .unwrap();
    let r = register_invite(
        &base,
        &format!("p12i_{}", uniq()),
        Some(inv2["code"].as_str().unwrap()),
    )
    .await;
    assert_eq!(r.status(), 403);
}

/// max_uses=1：两个请求并发用同一码注册，恰一个 200、一个 403
#[tokio::test]
async fn p12_invite_max_uses_race() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12j_{}", uniq())).await;
    let admin = register(&base, &format!("p12j_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);
    let inv = create_invite(&base, &admin, json!({"max_uses": 1})).await;
    let code = inv["code"].as_str().unwrap().to_string();

    let (n1, n2) = (format!("p12k_{}", uniq()), format!("p12l_{}", uniq()));
    let (r1, r2) = tokio::join!(
        register_invite(&base, &n1, Some(&code)),
        register_invite(&base, &n2, Some(&code)),
    );
    let mut codes = [r1.status().as_u16(), r2.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 403], "恰好一个成功一个拒绝");

    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
}

/// 注册因用户名冲突失败时事务回滚，邀请次数不消耗
#[tokio::test]
async fn p12_invite_rollback_on_conflict() {
    let (base, st) = spawn_named_state(&format!("chat_t_p12m_{}", uniq())).await;
    let admin = register(&base, &format!("p12m_{}", uniq())).await;
    assert_eq!(put_mode(&base, &admin, "invite").await.status(), 200);
    let inv = create_invite(&base, &admin, json!({})).await;
    let code = inv["code"].as_str().unwrap().to_string();

    let name = format!("p12n_{}", uniq());
    assert_eq!(
        register_invite(&base, &name, Some(&code)).await.status(),
        200
    );
    // 同名再注册 → 409；uses 必须保持 1
    assert_eq!(
        register_invite(&base, &name, Some(&code)).await.status(),
        409
    );
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1, "冲突回滚后邀请次数不应增加");
}

/// open（默认）忽略 invite_code：带错误码也能注册
#[tokio::test]
async fn p12_open_ignores_invite() {
    let base = spawn_named_db(&format!("chat_t_p12o_{}", uniq())).await;
    let _admin = register(&base, &format!("p12o_{}", uniq())).await;
    let r = register_invite(&base, &format!("p12p_{}", uniq()), Some("BADCODE123")).await;
    assert_eq!(r.status(), 200);
}

/// PUT /admin/settings 未知模式 → 400；合法值返回完整设置
#[tokio::test]
async fn p12_settings_validation() {
    let base = spawn_named_db(&format!("chat_t_p12q_{}", uniq())).await;
    let admin = register(&base, &format!("p12q_{}", uniq())).await;

    let r = put_mode(&base, &admin, "bogus").await;
    assert_eq!(r.status(), 400);

    let r = put_mode(&base, &admin, "invite").await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["registration_mode"], "invite");

    let body: Value = http()
        .get(format!("{base}/v1/admin/settings"))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["registration_mode"], "invite");

    // 创建参数边界：max_uses / expires_in_hours 越界 → 400
    for body in [
        json!({"max_uses": 0}),
        json!({"max_uses": 1001}),
        json!({"expires_in_hours": 0}),
        json!({"expires_in_hours": 8761}),
    ] {
        let r = http()
            .post(format!("{base}/v1/admin/invites"))
            .header("authorization", authz(&admin.access))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{body} 应 400");
    }
}
