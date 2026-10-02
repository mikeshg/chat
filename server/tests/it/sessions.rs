use super::*;

// ===== V2 P1-5：自助改密码 + 登录设备管理 =====

pub(crate) async fn rest_status(base: &str, method: &str, path: &str, access: &str) -> u16 {
    let c = http();
    let req = match method {
        "GET" => c.get(format!("{base}/v1{path}")),
        "POST" => c.post(format!("{base}/v1{path}")),
        "DELETE" => c.delete(format!("{base}/v1{path}")),
        _ => unreachable!(),
    };
    req.header("authorization", authz(access))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn login_status(base: &str, username: &str, password: &str) -> u16 {
    http()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// 读帧直到见到 want；中途收到 force_logout 直接判失败（未被踢的连接不该收到踢人帧）
async fn next_of_no_kick<S>(s: &mut S, want: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..10 {
        if let WsMsg::Text(t) = s.next().await.unwrap().unwrap() {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_ne!(v["type"], "force_logout", "unexpected force_logout: {v}");
            if v["type"] == want {
                return v;
            }
        }
    }
    panic!("did not receive frame type={want}");
}

/// 设备列表能看到多次登录，current 只标发起请求的 session，online 反映 WS 连接
#[tokio::test]
async fn sessions_list_marks_current_and_online() {
    let base = spawn().await;
    let u = register(&base, &format!("sa_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "设备甲").await;
    let _dev_b = login_device(&base, &u.username, "secret123", "设备乙").await;

    // 注册 + 两次登录 = 3 个 session
    let list = sessions_of(&base, &dev_a.access).await;
    assert_eq!(list.len(), 3);
    let a = list.iter().find(|s| s["device_label"] == "设备甲").unwrap();
    let b = list.iter().find(|s| s["device_label"] == "设备乙").unwrap();
    assert_eq!(a["current"], true);
    assert_eq!(b["current"], false);
    assert_eq!(a["online"], false);
    assert_eq!(b["online"], false);
    assert!(list.iter().all(|s| s["user_agent"].is_string()));

    // A 建立 WS 后仅 A 的 session 在线
    let (mut wa, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_a).await)
        .await
        .unwrap();
    next_of(&mut wa, "presence_init").await;
    let list = sessions_of(&base, &dev_a.access).await;
    let a = list.iter().find(|s| s["device_label"] == "设备甲").unwrap();
    let b = list.iter().find(|s| s["device_label"] == "设备乙").unwrap();
    assert_eq!(a["online"], true);
    assert_eq!(b["online"], false);
}

/// refresh 轮换沿用同一 session，且推进 last_used_at
#[tokio::test]
async fn refresh_rotates_token_within_same_session() {
    let base = spawn().await;
    let u = register(&base, &format!("sb_{}", uniq())).await;
    let dev = login_device(&base, &u.username, "secret123", "本机").await;
    let before = sessions_of(&base, &dev.access).await;
    let s0 = before.iter().find(|s| s["device_label"] == "本机").unwrap();
    let sid = s0["id"].as_str().unwrap().to_string();
    let used0 = s0["last_used_at"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    let new_access = res["access_token"].as_str().unwrap();

    let after = sessions_of(&base, new_access).await;
    let s1 = after
        .iter()
        .find(|s| s["id"] == sid)
        .expect("session id changed");
    assert!(
        s1["last_used_at"].as_str().unwrap() > used0.as_str(),
        "last_used_at 应前进"
    );
    // refresh 不产生新 session
    assert_eq!(after.len(), before.len());
}

/// A 移出设备 B：B 的 WS 收 force_logout(session_revoked) 并被关闭、凭据全失效；
/// A 的连接不受影响；U 仍在线 → 好友收不到离线广播
#[tokio::test]
async fn revoke_session_kicks_only_that_device() {
    let base = spawn().await;
    let u = register(&base, &format!("sc_{}", uniq())).await;
    let f = register(&base, &format!("sf_{}", uniq())).await;
    befriend(&base, &u, &f).await;
    let conv = dm_conv(&base, &u).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let (mut wa, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_a).await)
        .await
        .unwrap();
    let (mut wb, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_b).await)
        .await
        .unwrap();
    let (mut wf, _) = tokio_tungstenite::connect_async(ws_url(&base, &f).await)
        .await
        .unwrap();
    next_of(&mut wa, "presence_init").await;
    next_of(&mut wb, "presence_init").await;
    next_of(&mut wf, "presence_init").await;

    // A 移出 B 的 session
    let list = sessions_of(&base, &dev_a.access).await;
    let b_sid = list.iter().find(|s| s["device_label"] == "B").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );

    // B：先收 force_logout(session_revoked)，随后服务端关闭 socket
    let kicked = next_of(&mut wb, "force_logout").await;
    assert_eq!(kicked["reason"], "session_revoked");
    closed_without_application_frames(&mut wb).await;

    // B 的 access / refresh 全部失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // A 的连接仍可受理帧，且全程没收到 force_logout
    wa.send(WsMsg::Text(
        json!({"type":"sync","conversation_id":conv,"last_seq":0})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    next_of_no_kick(&mut wa, "sync_messages").await;

    // U 仍有 A 在线 → F（好友观察者）不得收到 U 的离线广播
    let stray = tokio::time::timeout(std::time::Duration::from_millis(800), wf.next()).await;
    if let Ok(Some(Ok(WsMsg::Text(t)))) = stray {
        let v: Value = serde_json::from_str(&t).unwrap();
        assert!(
            !(v["type"] == "presence" && v["user_id"] == u.id && v["online"] == false),
            "仍在在线的用户不该被广播离线: {v}"
        );
    }
}

/// 撤销当前 session → 400；撤销他人/不存在的 session → 404；重复撤销幂等 204
#[tokio::test]
async fn revoke_session_guards_and_idempotent() {
    let base = spawn().await;
    let u = register(&base, &format!("sd_{}", uniq())).await;
    let other = register(&base, &format!("se_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let _dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let list = sessions_of(&base, &dev_a.access).await;
    let cur = list.iter().find(|s| s["current"] == true).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_sid = list.iter().find(|s| s["device_label"] == "B").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 当前设备 → 400
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{cur}"),
            &dev_a.access
        )
        .await,
        400
    );
    // 别人的 session → 404；不存在的 id → 404
    let other_sid = sessions_of(&base, &other.access).await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{other_sid}"),
            &dev_a.access
        )
        .await,
        404
    );
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{}", uuid::Uuid::new_v4()),
            &dev_a.access
        )
        .await,
        404
    );
    // 正常撤销 → 204；再撤一次仍 204（幂等）
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );
    assert_eq!(
        rest_status(
            &base,
            "DELETE",
            &format!("/users/me/sessions/{b_sid}"),
            &dev_a.access
        )
        .await,
        204
    );
}

/// 自助改密：旧密码错 403；成功后签发新凭据、当前 session 不变；
/// 其他 session 的 token/refresh/WS 全失效；新密码可登录旧密码不能
#[tokio::test]
async fn change_password_kicks_other_sessions() {
    let base = spawn().await;
    let u = register(&base, &format!("sg_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;
    let (mut wb, _) = tokio_tungstenite::connect_async(ws_url(&base, &dev_b).await)
        .await
        .unwrap();
    next_of(&mut wb, "presence_init").await;
    let c = http();
    let sid_before = sessions_of(&base, &dev_a.access)
        .await
        .iter()
        .find(|s| s["current"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 旧密码错误 → 403（不能是 401，否则客户端会走刷新流程）
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"nope-nope","new_password":"newpass456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 新密码长度不合规 → 400（与注册一致 6-72）
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 成功 → 返回为当前 session 新签发的凭据
    let r = c
        .post(format!("{base}/v1/users/me/password"))
        .header("authorization", authz(&dev_a.access))
        .json(&json!({"old_password":"secret123","new_password":"newpass456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    let new_access = res["access_token"].as_str().unwrap().to_string();
    let new_refresh = res["refresh_token"].as_str().unwrap().to_string();

    // 新凭据可用；session 列表只剩当前一条且 id 不变
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &new_access).await,
        200
    );
    let list = sessions_of(&base, &new_access).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"].as_str().unwrap(), sid_before);
    assert_eq!(list[0]["current"], true);

    // A 的旧 access → 401（session_version 已 +1）；B 的 access/refresh → 401
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_a.access).await,
        401
    );
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 同一 session 的新 refresh 仍有效（改密不撤销本设备的登录）
    let r = c
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": new_refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // B 的 WS：force_logout(password_changed) → 服务端关闭
    let kicked = next_of(&mut wb, "force_logout").await;
    assert_eq!(kicked["reason"], "password_changed");
    closed_without_application_frames(&mut wb).await;

    // 新密码可登录，旧密码不能
    assert_eq!(login_status(&base, &u.username, "secret123").await, 401);
    assert_eq!(login_status(&base, &u.username, "newpass456").await, 200);
}

/// 一键退出其他所有设备：只剩当前 session
#[tokio::test]
async fn revoke_other_sessions_keeps_current() {
    let base = spawn().await;
    let u = register(&base, &format!("sh_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;
    let dev_c = login_device(&base, &u.username, "secret123", "C").await;

    assert_eq!(
        rest_status(
            &base,
            "POST",
            "/users/me/sessions/revoke-others",
            &dev_a.access
        )
        .await,
        204
    );
    let list = sessions_of(&base, &dev_a.access).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["device_label"], "A");
    assert_eq!(list[0]["current"], true);

    // 其他设备的凭据全部失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_b.access).await,
        401
    );
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_c.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_b.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // 幂等：再调一次仍 204
    assert_eq!(
        rest_status(
            &base,
            "POST",
            "/users/me/sessions/revoke-others",
            &dev_a.access
        )
        .await,
        204
    );
}

/// 老 access token（无 sid claim）仍可通过 require_user：session_version 对就行
#[tokio::test]
async fn legacy_access_token_without_sid_still_accepted() {
    let base = spawn().await;
    let u = register(&base, &format!("si_{}", uniq())).await;
    let uid = uuid::Uuid::parse_str(&u.id).unwrap();
    // 模拟升级前签发的 token：session_version=0、不带 sid
    let (at, _) = chat_server::auth::mint_access(uid, 0, None, "test-secret").unwrap();
    assert_eq!(rest_status(&base, "GET", "/users/me", &at).await, 200);
    assert_eq!(rest_status(&base, "GET", "/ws/ticket", &at).await, 200);
}

/// 登出撤销所属 session：设备列表里消失、该设备的 access/refresh 全失效
#[tokio::test]
async fn logout_revokes_whole_session() {
    let base = spawn().await;
    let u = register(&base, &format!("sj_{}", uniq())).await;
    let dev_a = login_device(&base, &u.username, "secret123", "A").await;
    let dev_b = login_device(&base, &u.username, "secret123", "B").await;

    let r = http()
        .post(format!("{base}/v1/auth/logout"))
        .json(&json!({"refresh_token": dev_a.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // B 视角：A 的 session 从列表消失（剩注册时的 session + B）
    let list = sessions_of(&base, &dev_b.access).await;
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s["device_label"] != "A"));

    // A 的凭据随 session 一起失效
    assert_eq!(
        rest_status(&base, "GET", "/users/me", &dev_a.access).await,
        401
    );
    let r = http()
        .post(format!("{base}/v1/auth/refresh"))
        .json(&json!({"refresh_token": dev_a.refresh}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
