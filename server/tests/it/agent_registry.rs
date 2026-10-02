use super::*;

// ===== Agent A07 =====
// 假登记表：本地 HTTP 提供 registry.json 和一个 zip。宿主的登记表地址指向它。

fn a07_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// 单个文件、不压缩的 zip。登记表测试不依赖额外的打包库。
fn a07_store_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let name_b = name.as_bytes();
    let crc = a07_crc32(data);
    let mut out = Vec::new();
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name_b);
    out.extend_from_slice(data);
    let central_at = out.len();
    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name_b);
    let central_len = out.len() - central_at;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(central_len as u32).to_le_bytes());
    out.extend_from_slice(&(central_at as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn a07_bin(archive: &str, sha: &str, cmd: &str) -> Value {
    json!({
        "archive": archive,
        "sha256": sha,
        "cmd": cmd,
        "args": []
    })
}

fn a07_agent(id: &str, name: &str, desc: &str, icon: &str, key: &str, bin: Value) -> Value {
    let mut binary = serde_json::Map::new();
    binary.insert(key.to_string(), bin);
    json!({
        "id": id,
        "name": name,
        "version": "1.0.0",
        "description": desc,
        "icon": icon,
        "distribution": {"binary": binary}
    })
}

async fn a07_spawn_host(
    base: &str,
    u: &TestUser,
    registry_url: &str,
    data_dir: std::path::PathBuf,
) -> (agent_host::HostHandle, String) {
    let h = agent_host::spawn(
        agent_host::HostConfig {
            server_url: base.into(),
            install_id: uuid::Uuid::new_v4(),
            label: "a07-host".into(),
            platform: "windows".into(),
            mcp_bridge: None,
            builtin_agent: None,
            registry_url: registry_url.into(),
            data_dir,
        },
        std::sync::Arc::new(AgentTickets {
            base: base.into(),
            access: u.access.clone(),
        }),
    );
    let id = tokio::time::timeout(A01Dur::from_secs(10), h.wait_host_id())
        .await
        .expect("host_ready 超时")
        .expect("未能拿到 host_id");
    (h, id.to_string())
}

async fn a07_registry(base: &str, u: &TestUser, host_id: &str) -> (u16, Value) {
    let r = http()
        .get(format!("{base}/v1/agent-hosts/{host_id}/registry"))
        .header("authorization", authz(&u.access))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

async fn a07_install(base: &str, u: &TestUser, host_id: &str, entry: &str, version: &str) -> u16 {
    http()
        .post(format!(
            "{base}/v1/agent-hosts/{host_id}/registry/{entry}/install"
        ))
        .header("authorization", authz(&u.access))
        .json(&json!({"version": version}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn a07_progress<S>(s: &mut S, entry: &str, stage: &str) -> Value
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等安装进度 {entry}/{stage} 超时");
        let frames = until_frame(s, "registry_install_progress", left).await;
        if let Some(f) = frames.into_iter().find(|f| {
            f["entry_id"] == entry
                && f["stage"] == stage
                && f["type"] == "registry_install_progress"
        }) {
            return f;
        }
    }
}

async fn a07_failed_error<S>(s: &mut S) -> String
where
    S: StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等 failed 调用超时");
        let frames = until_frame(s, "agent_invocation", left).await;
        if let Some(err) = frames.iter().find_map(|f| {
            (f["invocation"]["status"] == "failed")
                .then(|| f["invocation"]["error"].as_str().unwrap_or("").to_string())
        }) {
            return err;
        }
    }
}

/// 列表三种状态、sha256 失败、装上 binary 后调用成功、离线 409、别人的宿主 404、
/// 未安装时调用 failed/registry_not_installed。
#[tokio::test]
async fn a07_registry_install_and_invoke() {
    let base = spawn_named_db(&format!("chat_it_a07_{}", uniq())).await;
    let a = register(&base, &format!("a07a_{}", uniq())).await;
    let b = register(&base, &format!("a07b_{}", uniq())).await;

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent-host/tests/fake-acp-agent.mjs");
    // canonicalize 在 Windows 上会带 \\?\ 前缀，node 会把它解析成盘符目录。
    let script = script.canonicalize().unwrap();
    let script = script
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    let cmd = format!("@echo off\r\nnode \"{script}\" %*\r\n");
    let zip = a07_store_zip("run.cmd", cmd.as_bytes());
    let sha = agent_host::registry::sha256_hex(&zip);
    let bad_sha = "0".repeat(64);
    let platform = agent_host::registry::current_platform();
    let other = if platform == "darwin-aarch64" {
        "linux-x86_64"
    } else {
        "darwin-aarch64"
    };

    let data = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/wt")
        .join(format!("a07-data-{}", uniq()));
    let seeded = data.join("agents").join("bin").join("seeded").join("1.0.0");
    std::fs::create_dir_all(&seeded).unwrap();
    std::fs::write(seeded.join("run.cmd"), b"@echo off\r\n").unwrap();

    // 先占端口，JSON 里的 archive 才能写成这个地址。文件在 accept 之前放进表。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let registry = json!({
        "version": 1,
        "agents": [
            a07_agent("seeded", "Seeded", "already extracted", &format!("{origin}/icon.svg"), platform, a07_bin(&format!("{origin}/none.zip"), "", "run.cmd")),
            a07_agent("widget", "Widget", "install me", &format!("{origin}/icon.svg"), platform, a07_bin(&format!("{origin}/widget.zip"), &sha, "run.cmd")),
            a07_agent("maconly", "Mac Only", "wrong platform", "", other, a07_bin(&format!("{origin}/none.zip"), "", "run")),
            a07_agent("badsum", "Bad Sum", "sha mismatch", "", platform, a07_bin(&format!("{origin}/widget.zip"), &bad_sha, "run.cmd")),
        ]
    });
    let mut files: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    files.insert("/registry.json".into(), registry.to_string().into_bytes());
    files.insert("/widget.zip".into(), zip);
    files.insert(
        "/icon.svg".into(),
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
    );
    let files = std::sync::Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let files = files.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/");
                let (status, body) = match files.get(path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", b"missing".to_vec()),
                };
                let ctype = if path.ends_with(".json") {
                    "application/json"
                } else if path.ends_with(".svg") {
                    "image/svg+xml"
                } else {
                    "application/octet-stream"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });

    let (host, host_id) =
        a07_spawn_host(&base, &a, &format!("{origin}/registry.json"), data.clone()).await;
    let (mut sa, _) = tokio_tungstenite::connect_async(ws_url(&base, &a).await)
        .await
        .unwrap();

    let (st, list) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 200, "{list}");
    let entries = list["entries"].as_array().unwrap();
    let status_of = |id: &str| {
        entries
            .iter()
            .find(|e| e["id"] == id)
            .unwrap_or_else(|| panic!("缺条目 {id}: {entries:?}"))["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(status_of("seeded"), "ready");
    assert_eq!(status_of("widget"), "installable");
    assert_eq!(status_of("maconly"), "unsupported");
    assert!(
        entries.iter().any(
            |e| e["id"] == "maconly" && e["reason"].as_str().unwrap_or("").contains("没有对应")
        ),
        "{entries:?}"
    );
    let kinds: std::collections::HashSet<_> = entries
        .iter()
        .map(|e| e["status"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains("ready") && kinds.contains("installable") && kinds.contains("unsupported")
    );

    // 登记表形状只校验格式：版本没装也能创建。
    let miss_name = format!("m{}", uniq());
    let created = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": miss_name,
            "host_id": host_id,
            "runtime": {"registry": {"id": "widget", "version": "9.9.9"}},
            "cwd": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let miss: Value = created.json().await.unwrap();
    assert_eq!(miss["runtime"]["registry"]["id"], "widget");
    assert_eq!(miss["runtime"]["registry"]["version"], "9.9.9");
    let miss_id = miss["id"].as_str().unwrap();
    let miss_conv = miss["conversation_id"].as_str().unwrap().to_string();

    let bad = http()
        .patch(format!("{base}/v1/agents/{miss_id}"))
        .header("authorization", authz(&a.access))
        .json(&json!({"runtime": {"registry": {"id": "../x", "version": "1"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let bad: Value = bad.json().await.unwrap();
    assert_eq!(bad["error"], "bad_runtime");

    let (st, body) = a07_registry(&base, &b, &host_id).await;
    assert_eq!(st, 404, "别人的宿主应 404: {body}");
    assert_eq!(
        a07_install(&base, &b, &host_id, "widget", "1.0.0").await,
        404
    );
    let (st, _) = a07_registry(&base, &a, &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(st, 404);

    send_ws(&mut sa, &miss_conv, "miss1", "echo:nope").await;
    let err = a07_failed_error(&mut sa).await;
    assert_eq!(err, "registry_not_installed", "{err}");

    assert_eq!(
        a07_install(&base, &a, &host_id, "badsum", "1.0.0").await,
        202
    );
    let failed = a07_progress(&mut sa, "badsum", "失败").await;
    assert_eq!(failed["host_id"], host_id);
    assert!(
        failed["error"].as_str().unwrap_or("").contains("sha256"),
        "{failed}"
    );

    assert_eq!(
        a07_install(&base, &a, &host_id, "widget", "1.0.0").await,
        202
    );
    let done = a07_progress(&mut sa, "widget", "完成").await;
    assert_eq!(done["host_id"], host_id);
    assert!(
        done.get("error").is_none() || done["error"].is_null(),
        "{done}"
    );

    let (st, list) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 200, "{list}");
    let widget = list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "widget")
        .unwrap();
    assert_eq!(widget["status"], "ready", "{widget}");

    let ok_name = format!("w{}", uniq());
    let created = http()
        .post(format!("{base}/v1/agents"))
        .header("authorization", authz(&a.access))
        .json(&json!({
            "name": ok_name,
            "host_id": host_id,
            "runtime": {"registry": {"id": "widget", "version": "1.0.0"}},
            "cwd": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let agent: Value = created.json().await.unwrap();
    let conv = agent["conversation_id"].as_str().unwrap().to_string();
    send_ws(&mut sa, &conv, "ok1", "echo:pong-a07").await;
    let deadline = std::time::Instant::now() + A01Dur::from_secs(20);
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        buf.extend(frames_for(&mut sa, left.min(A01Dur::from_millis(500))).await);
        let failed = buf
            .iter()
            .find(|f| f["type"] == "agent_invocation" && f["invocation"]["status"] == "failed");
        if let Some(f) = failed {
            panic!("登记表 Agent 调用失败: {}", f["invocation"]["error"]);
        }
        let replied = buf.iter().any(|f| {
            f["type"] == "message"
                && f["message"]["agent"].is_object()
                && f["message"]["content"] == "pong-a07"
        });
        let succeeded = buf.iter().any(|f| {
            f["type"] == "agent_invocation"
                && f["invocation"]["status"] == "succeeded"
                && f["invocation"]["agent_id"] == agent["id"]
        });
        if replied && succeeded {
            break;
        }
    }
    let replied = buf.iter().any(|f| {
        f["type"] == "message"
            && f["message"]["agent"].is_object()
            && f["message"]["content"] == "pong-a07"
    });
    if !replied {
        let msgs: Value = http()
            .get(format!("{base}/v1/conversations/{conv}/messages"))
            .header("authorization", authz(&a.access))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        panic!("没有看到 Agent 回复。ws={buf:?} rest={msgs}");
    }

    host.shutdown().await;
    tokio::time::sleep(A01Dur::from_millis(400)).await;
    let (st, body) = a07_registry(&base, &a, &host_id).await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(body["error"], "host_offline");
    assert_eq!(
        a07_install(&base, &a, &host_id, "widget", "1.0.0").await,
        409
    );
}
