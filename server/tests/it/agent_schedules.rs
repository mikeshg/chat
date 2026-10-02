use super::*;

// ===== Agent A17 =====
// 定时调用。到点用把 next_run_at 拨到过去来模拟，不改服务器时钟。

pub(crate) async fn a17_send(
    method: &str,
    url: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let c = http();
    let req = match method {
        "GET" => c.get(url),
        "POST" => c.post(url),
        "PATCH" => c.patch(url),
        "DELETE" => c.delete(url),
        other => panic!("method {other}"),
    };
    let req = req.header("authorization", authz(token));
    let req = if let Some(body) = body {
        req.json(&body)
    } else {
        req
    };
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(Value::Null);
    (status, body)
}

pub(crate) fn a17_daily(name: &str, prompt: &str, time: &str) -> Value {
    json!({
        "name": name,
        "prompt": prompt,
        "spec": {"type": "daily", "time": time},
        "timezone": "Asia/Shanghai",
    })
}

pub(crate) async fn a17_due(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "UPDATE agent_schedules SET next_run_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

async fn a17_due_hours(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "UPDATE agent_schedules SET next_run_at = now() - interval '3 hours' WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

async fn a17_wait_text(base: &str, u: &TestUser, conv: &str, exact: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    loop {
        let texts = agent_texts(&conv_messages(base, u, conv).await);
        if texts.iter().any(|t| t == exact) {
            return texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等定时回复 {exact:?} 超时，当前 {texts:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    }
}

fn a17_sched_reply(name: &str, body: &str) -> String {
    format!("⏰ 定时任务「{name}」\n{body}")
}

/// 1. 创建 / 列表 / 修改 / 删除；别人 404；不是成员 400；只读私聊 400；超过 50 条 400。
#[tokio::test]
async fn agent_a17_schedule_crud() {
    let base = spawn_named_db(&format!("chat_it_a17c_{}", uniq())).await;
    let a = register(&base, &format!("a17a_{}", uniq())).await;
    let b = register(&base, &format!("a17b_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (st, agent) = create_agent(&base, &a, &host_id, "sched").await;
    assert_eq!(st, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, _) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &b.access,
        None,
    )
    .await;
    assert_eq!(code, 404, "别人列定时任务应 404");

    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "x",
            "prompt": "echo:x",
            "target_conversation_id": uuid::Uuid::new_v4().to_string(),
            "spec": {"type":"daily","time":"09:00"},
            "timezone": "Asia/Shanghai",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "not_member");

    befriend(&base, &a, &b).await;
    let dm = dm_conv(&base, &a).await;
    let un = http()
        .delete(format!("{base}/v1/friends/{}", b.id))
        .header("authorization", authz(&a.access))
        .send()
        .await
        .unwrap();
    assert_eq!(un.status(), 204);
    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "只读",
            "prompt": "echo:x",
            "target_conversation_id": dm,
            "spec": {"type":"interval","minutes":30},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "readonly_dm");

    for (label, payload) in [
        (
            "空名",
            json!({"name":"","prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "超长名",
            json!({"name":"n".repeat(41),"prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "空指令",
            json!({"name":"ok","prompt":"","spec":{"type":"daily","time":"09:00"},"timezone":"UTC"}),
        ),
        (
            "间隔太短",
            json!({"name":"ok","prompt":"echo:x","spec":{"type":"interval","minutes":4},"timezone":"UTC"}),
        ),
        (
            "坏时区",
            json!({"name":"ok","prompt":"echo:x","spec":{"type":"daily","time":"09:00"},"timezone":"Mars/Base"}),
        ),
    ] {
        let (code, body) = a17_send(
            "POST",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            Some(payload),
        )
        .await;
        assert_eq!(code, 400, "{label}: {body}");
    }

    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨报", "echo:ping", "09:00")),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    assert_eq!(created["created_by"], "user");
    assert_eq!(created["target_conversation_id"], conv);
    assert_eq!(created["enabled"], true);
    assert!(created["next_run_at"].is_string());
    let sid = created["id"].as_str().unwrap().to_string();

    let (code, _) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &b.access,
        Some(a17_daily("偷", "echo:x", "09:00")),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &b.access,
        Some(json!({"name":"no"})),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &b.access,
        None,
    )
    .await;
    assert_eq!(code, 404);

    let (code, patched) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        Some(json!({"name": "晨报改", "enabled": false})),
    )
    .await;
    assert_eq!(code, 200, "{patched}");
    assert_eq!(patched["name"], "晨报改");
    assert_eq!(patched["enabled"], false);

    let before = chrono::Utc::now();
    let (code, patched) = a17_send(
        "PATCH",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        Some(json!({
            "enabled": true,
            "spec": {"type":"interval","minutes":60},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{patched}");
    assert_eq!(patched["enabled"], true);
    assert_eq!(patched["spec"]["type"], "interval");
    let next = chrono::DateTime::parse_from_rfc3339(patched["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mins = (next - before).num_minutes();
    assert!(
        (59..=61).contains(&mins),
        "间隔 60 分钟的下次运行应在约 1 小时后，实际差 {mins} 分钟 ({next})"
    );

    let (code, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "晨报改");

    for i in 0..49 {
        let (code, body) = a17_send(
            "POST",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            Some(json!({
                "name": format!("n{i}"),
                "prompt": "echo:x",
                "spec": {"type":"interval","minutes":60},
                "timezone": "UTC",
            })),
        )
        .await;
        assert_eq!(code, 200, "第 {i} 条填充失败 {body}");
    }
    let (code, body) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "overflow",
            "prompt": "echo:x",
            "spec": {"type":"interval","minutes":60},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["error"], "too_many");

    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{sid}"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 204);
    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 49);
    assert!(list.as_array().unwrap().iter().all(|s| s["id"] != sid));
}

/// 建调用失败时整轮回滚：一次性计划仍启用、仍到点，修好后下一轮照常触发（不会「显示已触发却没调用」）
#[tokio::test]
async fn agent_a17_insert_failure_keeps_schedule_due() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17x_{}", uniq())).await;
    let a = register(&base, &format!("a17x_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "atomic").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let at = chrono::Utc::now() + chrono::Duration::hours(2);
    let (code, once) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({"name": "一次", "prompt": "echo:x",
                    "spec": {"type":"once","at": at.to_rfc3339()}, "timezone": "UTC"})),
    )
    .await;
    assert_eq!(code, 200, "{once}");
    let sid = uuid::Uuid::parse_str(once["id"].as_str().unwrap()).unwrap();
    a17_due(&st.pool, &sid.to_string()).await;

    // 让定时调用的插入失败（这个测试库独占）
    sqlx::query("ALTER TABLE agent_invocations ADD CONSTRAINT t_no_sched CHECK (schedule_id IS NULL) NOT VALID")
        .execute(&st.pool)
        .await
        .unwrap();
    assert!(chat_server::schedules::sweep_due_schedules(&st)
        .await
        .is_err());
    let (enabled, due): (bool, bool) =
        sqlx::query_as("SELECT enabled, next_run_at <= now() FROM agent_schedules WHERE id = $1")
            .bind(sid)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert!(enabled && due, "插入失败不应推进计划");

    sqlx::query("ALTER TABLE agent_invocations DROP CONSTRAINT t_no_sched")
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(
        chat_server::schedules::sweep_due_schedules(&st)
            .await
            .unwrap(),
        1
    );
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE schedule_id = $1")
            .bind(sid)
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
    host.shutdown().await;
}

/// 2. 到点触发：回复带前缀；once 停用；daily 的下次运行是明天同一钟点。
#[tokio::test]
async fn agent_a17_due_fires_once_and_daily() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17f_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17f_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "duebot").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap();

    let (code, daily) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("晨报", "echo:ping", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{daily}");
    let daily_id = daily["id"].as_str().unwrap().to_string();

    let soon = chrono::Utc::now() + chrono::Duration::hours(2);
    let (code, once) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "一次",
            "prompt": "echo:oncebody",
            "spec": {"type":"once","at": soon.to_rfc3339()},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{once}");
    let once_id = once["id"].as_str().unwrap().to_string();

    a17_due(&st.pool, &daily_id).await;
    a17_due(&st.pool, &once_id).await;

    let daily_reply = a17_sched_reply("晨报", "ping");
    let once_reply = a17_sched_reply("一次", "oncebody");
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let texts = loop {
        let texts = agent_texts(&conv_messages(&base, &a, conv).await);
        if texts.iter().any(|t| t == &daily_reply) && texts.iter().any(|t| t == &once_reply) {
            break texts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等两条定时回复超时 {texts:?}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert!(texts.contains(&daily_reply));
    assert!(texts.contains(&once_reply));

    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    let once_row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == once_id)
        .unwrap();
    assert_eq!(once_row["enabled"], false);
    assert!(once_row["next_run_at"].is_null(), "{once_row}");
    let daily_row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == daily_id)
        .unwrap();
    let got = chrono::DateTime::parse_from_rfc3339(daily_row["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    use chrono::{Datelike, TimeZone};
    let shanghai = chrono_tz::Asia::Shanghai;
    let now_local = chrono::Utc::now().with_timezone(&shanghai);
    let tomorrow = now_local.date_naive().succ_opt().unwrap();
    let expected = shanghai
        .with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 0, 0)
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let delta_ms = (got.timestamp_millis() - expected.timestamp_millis()).abs();
    assert!(
        delta_ms <= 1000,
        "daily 下次应是明天 00:00 上海，got {got} expected {expected}"
    );

    let invs = list_inv(&base, &a, conv).await;
    let sched_invs: Vec<&Value> = invs
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["trigger_message_id"].is_null())
        .collect();
    assert_eq!(sched_invs.len(), 2, "{invs}");
    assert!(sched_invs.iter().all(|r| r["schedule_id"].is_string()));

    let keys: Vec<String> =
        sqlx::query_scalar("SELECT context_key FROM agent_contexts WHERE agent_id = $1")
            .bind(uuid::Uuid::parse_str(aid).unwrap())
            .fetch_all(&st.pool)
            .await
            .unwrap();
    assert!(keys.contains(&format!("schedule:{daily_id}")), "{keys:?}");
    assert!(keys.contains(&format!("schedule:{once_id}")), "{keys:?}");

    let once_inv = sched_invs
        .iter()
        .find(|r| r["schedule_id"] == once_id)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, _) = a17_send(
        "DELETE",
        &format!("{base}/v1/agent-schedules/{once_id}"),
        &a.access,
        None,
    )
    .await;
    assert_eq!(code, 204, "跑完的 once 应能删掉，历史调用保留");
    let invs = list_inv(&base, &a, conv).await;
    let kept = invs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == once_inv)
        .unwrap();
    assert!(
        kept["schedule_id"].is_null() || kept.get("schedule_id").is_none(),
        "{kept}"
    );
    assert!(kept["trigger_message_id"].is_null());
    host.shutdown().await;
}

/// 3. 宿主离线时到点 → queued，上线后执行并带前缀。
#[tokio::test]
async fn agent_a17_offline_queues_then_runs() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17q_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17q_{}", uniq())).await;
    let (conv, aid, install) = offline_agent(&base, &a, "offbot").await;
    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(a17_daily("离线", "echo:offbody", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    a17_due(&st.pool, created["id"].as_str().unwrap()).await;

    let deadline = std::time::Instant::now() + A01Dur::from_secs(15);
    let queued = loop {
        let rows = list_inv(&base, &a, &conv).await;
        if let Some(row) = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["status"] == "queued")
        {
            break row.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "离线到点应排队 {rows}"
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert!(queued["trigger_message_id"].is_null());
    assert!(queued["schedule_id"].is_string());

    let (host, _) = spawn_host(&base, &a, install).await;
    let texts = a17_wait_text(&base, &a, &conv, &a17_sched_reply("离线", "offbody")).await;
    assert_eq!(texts, vec![a17_sched_reply("离线", "offbody")]);
    host.shutdown().await;
}

/// 4. Agent 删除后不再触发；主人离开目标群后停用；主人被禁用后停用。
#[tokio::test]
async fn agent_a17_guards_stop_the_schedule() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17g_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(3_600_000, std::sync::atomic::Ordering::Relaxed);
    let admin = register(&base, &format!("a17ad_{}", uniq())).await;
    let owner = register(&base, &format!("a17ow_{}", uniq())).await;
    let friend = register(&base, &format!("a17fr_{}", uniq())).await;

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap().to_string();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, doomed) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(a17_daily("将删", "echo:gone", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{doomed}");
    assert_eq!(delete_agent_req(&base, &owner, &aid).await, 204);
    a17_due(&st.pool, doomed["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT enabled, disabled_reason FROM agent_schedules WHERE id = $1")
            .bind(uuid::Uuid::parse_str(doomed["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(row.0, false);
    assert_eq!(row.1.as_deref(), Some("agent_deleted"));
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE schedule_id = $1")
            .bind(uuid::Uuid::parse_str(doomed["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "已删除的 Agent 不应再被触发");
    assert!(agent_texts(&conv_messages(&base, &owner, &conv).await).is_empty());

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard2").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    host.shutdown().await;
    let gid = a11_group(&base, &owner, &[&friend]).await;
    let (code, group_sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(json!({
            "name": "群报",
            "prompt": "echo:nope",
            "target_conversation_id": gid,
            "spec": {"type":"daily","time":"00:00"},
            "timezone": "Asia/Shanghai",
        })),
    )
    .await;
    assert_eq!(code, 200, "{group_sched}");
    let leave = http()
        .delete(format!("{base}/v1/conversations/{gid}/members/me"))
        .header("authorization", authz(&owner.access))
        .send()
        .await
        .unwrap();
    assert_eq!(leave.status(), 204);
    a17_due(&st.pool, group_sched["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let (code, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        None,
    )
    .await;
    assert_eq!(code, 200, "{list}");
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == group_sched["id"])
        .unwrap();
    assert_eq!(row["enabled"], false);
    assert_eq!(row["disabled_reason"], "not_member");
    assert!(agent_texts(&conv_messages(&base, &friend, &gid).await).is_empty());

    let (host, host_id) = spawn_host(&base, &owner, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &owner, &host_id, "guard3").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    host.shutdown().await;
    let (code, sched) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &owner.access,
        Some(a17_daily("禁用", "echo:no", "00:00")),
    )
    .await;
    assert_eq!(code, 200, "{sched}");
    let dis = http()
        .post(format!("{base}/v1/admin/users/{}/disable", owner.id))
        .header("authorization", authz(&admin.access))
        .send()
        .await
        .unwrap();
    assert_eq!(dis.status(), 204);
    a17_due(&st.pool, sched["id"].as_str().unwrap()).await;
    chat_server::schedules::sweep_due_schedules(&st)
        .await
        .unwrap();
    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT enabled, disabled_reason FROM agent_schedules WHERE id = $1")
            .bind(uuid::Uuid::parse_str(sched["id"].as_str().unwrap()).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(row.0, false);
    assert_eq!(row.1.as_deref(), Some("owner_disabled"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE agent_id = $1")
        .bind(uuid::Uuid::parse_str(aid).unwrap())
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// 5. 错过多次只补一次；并发两次扫描只建一个调用。
#[tokio::test]
async fn agent_a17_one_fire_per_schedule() {
    let (base, st) = spawn_named_state(&format!("chat_it_a17o_{}", uniq())).await;
    st.schedule_sweep_interval_ms
        .store(3_600_000, std::sync::atomic::Ordering::Relaxed);
    let a = register(&base, &format!("a17o_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let (code, agent) = create_agent(&base, &a, &host_id, "oncefire").await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    host.shutdown().await;

    let (code, created) = a17_send(
        "POST",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        Some(json!({
            "name": "间隔",
            "prompt": "echo:gap",
            "spec": {"type":"interval","minutes":30},
            "timezone": "UTC",
        })),
    )
    .await;
    assert_eq!(code, 200, "{created}");
    let sid = created["id"].as_str().unwrap();
    a17_due_hours(&st.pool, sid).await;
    let before = chrono::Utc::now();
    let (left, right) = tokio::join!(
        chat_server::schedules::sweep_due_schedules(&st),
        chat_server::schedules::sweep_due_schedules(&st),
    );
    left.unwrap();
    right.unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_invocations WHERE schedule_id = $1")
            .bind(uuid::Uuid::parse_str(sid).unwrap())
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "并发扫描只能建一个调用");
    let rows = list_inv(&base, &a, &conv).await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["status"], "queued");

    let (_, list) = a17_send(
        "GET",
        &format!("{base}/v1/agents/{aid}/schedules"),
        &a.access,
        None,
    )
    .await;
    let next = chrono::DateTime::parse_from_rfc3339(list[0]["next_run_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let after = chrono::Utc::now();
    assert!(
        next > before + chrono::Duration::minutes(29),
        "错过的间隔不应回填到过去，next={next} before={before}"
    );
    assert!(
        next < after + chrono::Duration::minutes(31),
        "下一次应约为现在加 30 分钟，next={next} after={after}"
    );
}

/// 6. 假 agent 经 MCP 调 create_schedule，列表里出现 created_by=agent。
#[tokio::test]
async fn agent_a17_tool_create_schedule() {
    let base = spawn_named_db(&format!("chat_it_a17t_{}", uniq())).await;
    let a = register(&base, &format!("a17t_{}", uniq())).await;
    let (host, host_id) = spawn_host(&base, &a, uuid::Uuid::new_v4()).await;
    let mut args = fake_acp_args();
    args.push("--mcp=http".into());
    let (code, agent) = create_agent_with(&base, &a, &host_id, "toolbot", args).await;
    assert_eq!(code, 200, "{agent}");
    let aid = agent["id"].as_str().unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();
    send_ws(&mut sa, &conv, "t1", "tool:create_schedule").await;

    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    let row = loop {
        let (code, list) = a17_send(
            "GET",
            &format!("{base}/v1/agents/{aid}/schedules"),
            &a.access,
            None,
        )
        .await;
        assert_eq!(code, 200, "{list}");
        if let Some(row) = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["created_by"] == "agent")
        {
            break row.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "工具创建的定时任务没出现。列表 {list} 会话 {:?}",
            agent_texts(&conv_messages(&base, &a, &conv).await)
        );
        tokio::time::sleep(A01Dur::from_millis(100)).await;
    };
    assert_eq!(row["name"], "from-agent");
    assert_eq!(row["target_conversation_id"], conv);
    assert_eq!(row["created_by"], "agent");
    host.shutdown().await;
}
