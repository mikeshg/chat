// ACP 客户端测试：跑 tests/fake-acp-agent.mjs（node）。

use agent_host::acp::run_invocation;
use agent_host::runtime::{AgentSpec, LocalFile, RunEvent, RunOutput, RunRequest};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

fn script() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fake-acp-agent.mjs")
        .to_string_lossy()
        .to_string()
}

fn fake() -> AgentSpec {
    AgentSpec {
        id: Uuid::new_v4(),
        name: "fake".into(),
        command: "node".into(),
        args: vec![script()],
        cwd: String::new(),
        env: Vec::new(),
        instructions: String::new(),
        model: String::new(),
        mode: String::new(),
        extra_dirs: Vec::new(),
        registry: None,
        builtin: None,
        permission_tier: "read_only".into(),
    }
}

fn req(prompt: &str) -> RunRequest {
    runtime_req(fake(), prompt)
}

fn runtime_req(spec: AgentSpec, prompt: &str) -> RunRequest {
    RunRequest::for_invocation(spec, prompt.into(), std::env::temp_dir())
}

/// 带取消信号的请求（A04 的取消用例）
fn cancellable(spec: AgentSpec, prompt: &str, cancel: watch::Receiver<bool>) -> RunRequest {
    let mut r = runtime_req(spec, prompt);
    r.cancel = cancel;
    r
}

async fn run(prompt: &str) -> Result<RunOutput, String> {
    let (events, _) = mpsc::unbounded_channel();
    run_invocation(req(prompt), events).await
}

#[tokio::test]
async fn acp_echo() {
    let out = run("echo:hello world").await.expect("echo 应成功");
    assert_eq!(out.text, "hello world");
}

#[tokio::test]
async fn acp_fail() {
    let e = run("fail").await.expect_err("fail 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_crash() {
    let e = run("crash").await.expect_err("crash 应报错");
    assert!(!e.is_empty());
}

#[tokio::test]
async fn acp_long_chunks_concat() {
    let out = run("long:5000").await.expect("long 应成功");
    assert_eq!(out.text.len(), 5000);
}

#[tokio::test]
async fn acp_emits_text_events_in_order() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let out = run_invocation(req("echo:hello world"), events)
        .await
        .expect("echo 应成功");
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    assert!(chunks.len() >= 2, "假 agent 至少分 2 段回出");
    assert_eq!(chunks.concat(), out.text);
}

#[tokio::test]
async fn acp_whoami_applies_config() {
    let mut spec = fake();
    spec.env = vec![("FAKE_ECHO".into(), "sekrit".into())];
    spec.instructions = "INST-a06".into();
    spec.model = "smart".into();
    spec.mode = "code".into();
    spec.extra_dirs = vec!["D:/extra".into()];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(runtime_req(spec, "whoami"), events)
        .await
        .expect("whoami 应成功");
    assert!(out.text.contains("mode=code"), "{}", out.text);
    assert!(out.text.contains("model=smart"), "{}", out.text);
    assert!(out.text.contains("FAKE_ECHO=sekrit"), "{}", out.text);
    assert!(out.text.contains("first=INST-a06"), "{}", out.text);
    assert!(out.text.contains("dirs=D:/extra"), "{}", out.text);
    assert_eq!(out.modes.as_ref().map(|m| m.len()), Some(2));
    assert_eq!(out.models.as_ref().map(|m| m.len()), Some(2));
}

#[tokio::test]
async fn acp_probe_reports_options_without_prompt() {
    let (events, _) = mpsc::unbounded_channel();
    let mut r = req("");
    r.probe = true;
    let out = run_invocation(r, events).await.expect("probe 应成功");
    assert!(
        out.text.is_empty(),
        "probe 不应发 prompt，正文应为空: {}",
        out.text
    );
    assert_eq!(out.modes.unwrap().len(), 2);
    assert_eq!(out.models.unwrap().len(), 2);
}

/// 桌面端添加前直接调的 probe：读到可选项；命令起不来时返回原因。
#[tokio::test]
async fn host_probe_reads_options_and_reports_failure() {
    let out = agent_host::probe(fake(), std::env::temp_dir(), "")
        .await
        .expect("probe 应成功");
    assert_eq!(out.models.unwrap()[1].id, "smart");
    let mut bad = fake();
    bad.command = "chat-no-such-cmd-probe".into();
    let err = agent_host::probe(bad, std::env::temp_dir(), "")
        .await
        .unwrap_err();
    assert!(!err.is_empty());
}

#[tokio::test]
async fn acp_ignores_mode_outside_options() {
    let mut spec = fake();
    spec.mode = "nope".into();
    spec.model = "nope".into();
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(runtime_req(spec, "whoami"), events)
        .await
        .expect("越界模式应忽略并继续");
    assert!(out.text.contains("mode=ask"), "{}", out.text);
    assert!(out.text.contains("model=fast"), "{}", out.text);
}

fn fake_caps(caps: &str) -> AgentSpec {
    let mut spec = fake();
    spec.args.push(format!("--caps={caps}"));
    spec
}

async fn invoke(
    spec: AgentSpec,
    prompt: &str,
    resume: Option<&str>,
) -> Result<(RunOutput, Vec<String>), String> {
    let (events, mut rx) = mpsc::unbounded_channel();
    let mut req = runtime_req(spec, prompt);
    req.resume_session = resume.map(str::to_string);
    let out = run_invocation(req, events).await?;
    let mut chunks = Vec::new();
    while let Ok(RunEvent::Text(t)) = rx.try_recv() {
        chunks.push(t);
    }
    Ok((out, chunks))
}

#[tokio::test]
async fn acp_resume_recalls_prior_prompt() {
    let spec = fake_caps("resume");
    let (first, _) = invoke(spec.clone(), "echo:one", None)
        .await
        .expect("第一次应成功");
    let sid = first.session_id.clone().expect("新会话应有 session_id");
    assert_eq!(first.text, "one");
    let (second, chunks) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("恢复后 recall 应成功");
    assert_eq!(second.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(second.text, "echo:one");
    assert_eq!(chunks.concat(), second.text);
}

#[tokio::test]
async fn acp_load_filters_replay_and_keeps_session() {
    let spec = fake_caps("load");
    let (first, _) = invoke(spec.clone(), "echo:REPLAYME", None)
        .await
        .expect("第一次应成功");
    let sid = first.session_id.clone().expect("session_id");
    assert_eq!(first.text, "REPLAYME");
    let (second, chunks) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("load 后续接应成功");
    assert_eq!(second.session_id.as_deref(), Some(sid.as_str()));
    // 回放的旧回复是整段 "REPLAYME"；混进来会变成 "REPLAYMEecho:REPLAYME"
    assert_eq!(second.text, "echo:REPLAYME");
    assert_eq!(chunks.concat(), second.text);
    assert!(
        chunks.iter().all(|c| c != "REPLAYME"),
        "回放块不应进入事件: {chunks:?}"
    );
}

#[tokio::test]
async fn acp_without_session_caps_starts_fresh() {
    let spec = fake_caps("none");
    let (first, _) = invoke(spec.clone(), "echo:one", None)
        .await
        .expect("无能力时新会话应成功");
    let sid = first.session_id.clone().expect("session_id");
    let (second, _) = invoke(spec, "recall", Some(&sid))
        .await
        .expect("退回新会话后调用仍应成功");
    assert_eq!(second.text, "(empty)");
    assert_ne!(second.session_id.as_deref(), Some(sid.as_str()));
    assert!(second.session_id.is_some());
}

#[tokio::test]
async fn acp_resume_error_falls_back_to_new_session() {
    let spec = fake_caps("resume");
    let (out, _) = invoke(spec, "recall", Some("missing-session"))
        .await
        .expect("恢复失败应退回新会话而不是报错");
    assert_eq!(out.text, "(empty)");
    assert!(out.session_id.is_some());
    assert_ne!(out.session_id.as_deref(), Some("missing-session"));
}

/// session/cancel 让 sleep 立刻以 cancelled 结束，不等满 sleep。
#[tokio::test]
async fn acp_cancel_stops_sleep() {
    let (tx, rx) = watch::channel(false);
    let (events, _) = mpsc::unbounded_channel();
    let handle = tokio::spawn(run_invocation(
        cancellable(fake(), "sleep:10000", rx),
        events,
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t0 = std::time::Instant::now();
    tx.send(true).unwrap();
    let out = tokio::time::timeout(Duration::from_secs(4), handle)
        .await
        .expect("合作取消应在 4s 内返回")
        .unwrap()
        .expect("取消路径不报错");
    assert!(t0.elapsed() < Duration::from_secs(4));
    assert_ne!(out.text, "done");
}

/// --ignore-cancel：通知无效，约 5 秒后进程被杀掉，调用返回。
#[tokio::test]
async fn acp_ignore_cancel_kills_after_grace() {
    let (tx, rx) = watch::channel(false);
    let (events, _) = mpsc::unbounded_channel();
    let mut spec = fake();
    spec.args.push("--ignore-cancel".into());
    let handle = tokio::spawn(run_invocation(cancellable(spec, "sleep:30000", rx), events));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t0 = std::time::Instant::now();
    tx.send(true).unwrap();
    let out = tokio::time::timeout(Duration::from_secs(8), handle)
        .await
        .expect("5s 宽限后应返回");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(8),
        "强制结束应约 5s，实际 {elapsed:?}"
    );
    match out.unwrap() {
        Ok(o) => assert_ne!(o.text, "done"),
        Err(_) => {}
    }
}

// ===== Agent A03：tool_call / tool_call_update → RunEvent::Step* =====

/// steps:2 应按序产生 StepStart×2 + StepEnd(ok=true)×2，文本事件拼接等于完整输出。
#[tokio::test]
async fn acp_steps_emit_start_end_events() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let req = req("steps:2");
    let out = run_invocation(req, events).await.expect("steps:2 应成功");
    let mut evs = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        evs.push(ev);
    }
    let starts: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::StepStart { id, title, kind } => {
                Some((id.clone(), title.clone(), kind.clone()))
            }
            _ => None,
        })
        .collect();
    let ends: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::StepEnd { id, ok } => Some((id.clone(), *ok)),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            (
                "step-1".to_string(),
                "步骤 1".to_string(),
                "other".to_string()
            ),
            (
                "step-2".to_string(),
                "步骤 2".to_string(),
                "other".to_string()
            ),
        ]
    );
    assert_eq!(
        ends,
        vec![("step-1".to_string(), true), ("step-2".to_string(), true)]
    );
    // 每个 StepStart 必须先于对应 StepEnd（按事件序）
    for id in ["step-1", "step-2"] {
        let si = evs
            .iter()
            .position(|e| matches!(e, RunEvent::StepStart { id: x, .. } if x == id));
        let ei = evs
            .iter()
            .position(|e| matches!(e, RunEvent::StepEnd { id: x, .. } if x == id));
        assert!(si < ei, "{id} 的 start 必须先于 end: {evs:?}");
    }
    let text: String = evs
        .iter()
        .filter_map(|e| match e {
            RunEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, out.text);
    assert!(out.text.ends_with("done"));
}

/// stepfail：该步 StepEnd(ok=false)，调用本身仍成功（文本含 done）。
#[tokio::test]
async fn acp_stepfail_emits_failed_end() {
    let (events, mut rx) = mpsc::unbounded_channel();
    let req = req("stepfail");
    let out = run_invocation(req, events).await.expect("stepfail 应成功");
    let mut saw_fail = false;
    while let Ok(ev) = rx.try_recv() {
        if let RunEvent::StepEnd { id, ok } = ev {
            assert_eq!(id, "step-x");
            assert!(!ok, "step-x 应以失败结束");
            saw_fail = true;
        }
    }
    assert!(saw_fail, "应收到 StepEnd(ok=false)");
    assert!(out.text.contains("done"));
}

// ===== Agent A12：没有服务端通道时，档位内自动放行，超档回 cancelled → rejected =====

#[tokio::test]
async fn acp_perm_read_only_auto_allows_read_and_rejects_edit() {
    let out = run("perm:read:notes.txt").await.expect("目录内 read 应自动放行");
    assert_eq!(out.text, "allowed:allow");
    let outside = if cfg!(windows) { "C:/Windows/win.ini" } else { "/etc/passwd" };
    let out = run(&format!("perm:read:{outside}"))
        .await
        .expect("目录外 read 应取消而不是挂起");
    assert_eq!(out.text, "rejected", "目录外的 read 不能自动放行");
    let out = run("perm:edit:C:/outside/a.rs")
        .await
        .expect("超档应取消而不是挂起");
    assert_eq!(out.text, "rejected");
}

#[tokio::test]
async fn acp_perm_workspace_allows_edit_inside_only() {
    let dir = std::env::temp_dir().join(format!("chat-a12-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let inside = dir.join("note.txt");
    let mut spec = fake();
    spec.permission_tier = "workspace".into();
    spec.cwd = dir.to_string_lossy().to_string();
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(
        runtime_req(spec.clone(), &format!("perm:edit:{}", inside.display())),
        events,
    )
    .await
    .expect("目录内 edit 应自动放行");
    assert_eq!(out.text, "allowed:allow");

    let outside = std::env::temp_dir().join(format!("chat-a12-out-{}", uuid::Uuid::new_v4()));
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(
        runtime_req(spec.clone(), &format!("perm:edit:{}", outside.display())),
        events,
    )
    .await
    .expect("目录外 edit 无审批通道时应取消");
    assert_eq!(out.text, "rejected");

    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(
        runtime_req(spec, &format!("perm:execute:{}", inside.display())),
        events,
    )
    .await
    .expect("execute 无审批通道时应取消");
    assert_eq!(out.text, "rejected");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn acp_perm_full_allows_execute() {
    let mut spec = fake();
    spec.permission_tier = "full".into();
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(runtime_req(spec, "perm:execute:C:/anywhere"), events)
        .await
        .expect("full 应自动放行");
    assert_eq!(out.text, "allowed:allow");
}

// ===== Agent A05 =====
// 附件进提示块：image/* 且 agent 声明 promptCapabilities.image → image 块（base64）；
// 其余一律 resource_link（file:// uri + name + mime + size）。
// outbox → CHAT_AGENT_OUTBOX 环境变量，假 agent 用 makefile/makebig 写产出文件。

fn tmp_file(name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("a05-acp-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let fp = dir.join(name);
    std::fs::write(&fp, bytes).unwrap();
    (dir, fp)
}

fn attach(path: PathBuf, name: &str, mime: &str) -> LocalFile {
    LocalFile {
        path,
        name: name.into(),
        mime: mime.into(),
    }
}

/// 图片 + 有 image 能力 → image 块；假 agent 回报解码后的字节数
#[tokio::test]
async fn a05_image_attachment_becomes_image_block() {
    let (dir, fp) = tmp_file("pic.png", &[0x89, 0x50, 0x4e, 0x47, 1, 2, 3]);
    let mut r = req("inspect");
    r.attachments = vec![attach(fp, "pic.png", "image/png")];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("inspect 应成功");
    assert!(out.text.contains("image:image/png:7"), "{}", out.text);
    assert!(!out.text.contains("link:"), "{}", out.text);
    std::fs::remove_dir_all(&dir).ok();
}

/// 非图片恒为 resource_link；--no-image 时图片也走 link
#[tokio::test]
async fn a05_resource_link_blocks() {
    // 非图片（能力开着也走 link）
    let (dir, fp) = tmp_file("note.txt", b"hi");
    let mut r = req("inspect");
    r.attachments = vec![attach(fp, "note.txt", "text/plain")];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("inspect 应成功");
    assert!(out.text.contains("link:note.txt:true"), "{}", out.text);
    assert!(!out.text.contains("image:"), "{}", out.text);

    // --no-image + 图片 → link
    let (d2, fp2) = tmp_file("p.png", &[1, 2, 3]);
    let mut spec = fake();
    spec.args.push("--no-image".into());
    let mut r = runtime_req(spec, "inspect");
    r.attachments = vec![attach(fp2, "p.png", "image/png")];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("inspect 应成功");
    assert!(out.text.contains("link:p.png:true"), "{}", out.text);
    assert!(!out.text.contains("image:"), "{}", out.text);

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&d2).ok();
}

/// 附件块在指令文本块之前：inspect 遍历 prompt 块顺序即附件顺序
#[tokio::test]
async fn a05_attachments_before_text_in_order() {
    let (dir, f1) = tmp_file("z.txt", b"z");
    let (_d2, f2) = tmp_file("a.png", &[9]);
    let mut r = req("inspect");
    r.attachments = vec![
        attach(f1, "z.txt", "text/plain"),
        attach(f2, "a.png", "image/png"),
    ];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("inspect 应成功");
    let zi = out.text.find("link:z.txt:true").unwrap();
    let ai = out.text.find("image:image/png:1").unwrap();
    assert!(zi < ai, "附件块应保持 attachments 顺序: {}", out.text);
    std::fs::remove_dir_all(&dir).ok();
}

/// outbox 目录经 CHAT_AGENT_OUTBOX 传到子进程；makefile 写文件成功
#[tokio::test]
async fn a05_outbox_env_reaches_agent() {
    let out_dir = std::env::temp_dir().join(format!("a05-ob-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut r = req("makefile:done.txt:yes");
    r.outbox = Some(out_dir.clone());
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("makefile 应成功");
    assert_eq!(out.text, "ok");
    let content = std::fs::read_to_string(out_dir.join("done.txt")).unwrap();
    assert_eq!(content, "yes");
    std::fs::remove_dir_all(&out_dir).ok();
}

/// 恢复会话路径同样带附件：resume 时 attachments 仍进提示块
#[tokio::test]
async fn a05_resume_carries_attachments() {
    let spec = fake_caps("resume");
    let (first, _) = invoke(spec.clone(), "echo:one", None)
        .await
        .expect("第一次应成功");
    let sid = first.session_id.clone().expect("session_id");

    let (dir, fp) = tmp_file("r.txt", b"r");
    let mut r = runtime_req(spec, "inspect");
    r.resume_session = Some(sid);
    r.attachments = vec![attach(fp, "r.txt", "text/plain")];
    let (events, _) = mpsc::unbounded_channel();
    let out = run_invocation(r, events).await.expect("resume 应成功");
    assert!(out.text.contains("link:r.txt:true"), "{}", out.text);
    std::fs::remove_dir_all(&dir).ok();
}

// 启动信息不算回复：建会话时 agent 自己发的消息，以及 pi-acp 在 _meta 里声明、晚于 prompt 才补发的同一段文字
async fn run_with_banner(mode: &str, prompt: &str) -> RunOutput {
    let mut spec = fake();
    spec.args.push(format!("--banner={mode}"));
    let (events, _) = mpsc::unbounded_channel();
    run_invocation(runtime_req(spec, prompt), events)
        .await
        .expect("应成功")
}

#[tokio::test]
async fn acp_drops_message_sent_before_prompt() {
    let out = run_with_banner("before", "echo:hello").await;
    assert_eq!(out.text, "hello");
}

#[tokio::test]
async fn acp_drops_declared_startup_info_even_after_prompt() {
    let out = run_with_banner("pi", "echo:hello").await;
    assert_eq!(out.text, "hello");
}
