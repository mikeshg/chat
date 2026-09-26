// 宿主 WS 连接循环：连上→host_hello→host_ready→收 invocation_start 起任务。

use crate::runtime::{
    self, AgentOption, AgentSpec, ApprovalLink, ApprovalNotice, InjectedMcp, RunEvent, RunOutput,
};
use crate::Shared;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::warn;
use uuid::Uuid;

type Sock = WebSocketStream<MaybeTlsStream<TcpStream>>;
// 出站帧统一走这个队列（连接读循环 + 各 invocation 任务共用）
type Out = mpsc::UnboundedSender<Message>;

pub(crate) async fn run(s: Arc<Shared>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if s.superseded.load(Ordering::SeqCst) {
            tracing::info!("被更新的宿主连接顶替，不再重连");
            return;
        }
        match connect_once(&s).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                warn!("宿主连接失败: {e}");
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        if s.superseded.load(Ordering::SeqCst) {
            return;
        }
        let _ = s.host_id.send(None);
        tokio::time::sleep(backoff).await;
    }
}

fn ws_url(server: &str, ticket: &str) -> String {
    let base = server
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    format!("{base}/v1/ws?ticket={ticket}&role=host")
}

/// server_url 的 http(s) 基址（注入工具回拨服务端 REST 用）
fn http_base(server: &str) -> String {
    server
        .replace("wss://", "https://")
        .replace("ws://", "http://")
}

async fn connect_once(s: &Arc<Shared>) -> Result<(), String> {
    let ticket = s.tickets.ticket().await?;
    let (sock, _) = connect_async(ws_url(&s.cfg.server_url, &ticket))
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    let (mut sink, mut stream) = sock.split();

    sink.send(Message::Text(
        json!({
            "type": "host_hello",
            "install_id": s.cfg.install_id,
            "label": s.cfg.label,
            "platform": s.cfg.platform,
        })
        .to_string()
        .into(),
    ))
    .await
    .map_err(|e| format!("host_hello: {e}"))?;

    // 服务端 10s 内应回 host_ready，否则视为握手失败
    let ready = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .map_err(|_| "host_ready 超时".to_string())?
        .ok_or("ws 提前关闭")?
        .map_err(|e| e.to_string())?;
    let ready: Value = serde_json::from_str(
        ready
            .to_text()
            .map_err(|_| "host_ready 非文本帧".to_string())?,
    )
    .map_err(|e| format!("host_ready 解析: {e}"))?;
    if ready["type"] != "host_ready" {
        return Err(format!("预期 host_ready，收到 {ready}"));
    }
    let host_id = Uuid::parse_str(ready["host_id"].as_str().unwrap_or_default())
        .map_err(|_| "host_ready 缺合法 host_id".to_string())?;
    let _ = s.host_id.send(Some(host_id));
    tracing::info!(%host_id, "宿主已就绪");

    // 写循环：读循环与 invocation 任务共用一个出站队列
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let res = read_loop(s, &mut stream, &out_tx).await;
    writer.abort();
    // 连接没了：还在等审批的调用以错误醒来，不要一直占着 agent 进程。
    s.approvals.clear();
    res
}

async fn read_loop(
    s: &Arc<Shared>,
    stream: &mut futures_util::stream::SplitStream<Sock>,
    out: &Out,
) -> Result<(), String> {
    loop {
        let msg = stream
            .next()
            .await
            .ok_or("ws 关闭")?
            .map_err(|e| e.to_string())?;
        match msg {
            Message::Ping(p) => {
                let _ = out.send(Message::Pong(p));
            }
            Message::Close(_) => return Ok(()),
            Message::Text(t) => {
                let v: Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v["type"].as_str().unwrap_or_default() {
                    // 同一 (user, install_id) 的新连接顶掉本连接：不再重连
                    "host_superseded" => {
                        s.superseded.store(true, Ordering::SeqCst);
                        return Ok(());
                    }
                    "invocation_start" => start_invocation(s, out, &v),
                    "agent_probe" => start_probe(s, out, &v),
                    "registry_list" => start_registry_list(s, out, &v),
                    "registry_install" => start_registry_install(s, out, &v),
                    "invocation_cancel" => {
                        // 不直接 abort：让 ACP 先发 session/cancel，最多再等 5 秒才杀进程。
                        // 这次调用不再回 invocation_finish。
                        if let Some(id) = v["invocation_id"]
                            .as_str()
                            .and_then(|x| Uuid::parse_str(x).ok())
                        {
                            if let Some(tx) = s.cancels.lock().unwrap().get(&id) {
                                let _ = tx.send(true);
                            }
                        }
                    }
                    // A12：主人对超档操作的决定。request_id 对不上就忽略。
                    "approval_decision" => {
                        if let (Some(rid), Some(oid)) = (
                            v["request_id"].as_str(),
                            v["option_id"].as_str().filter(|s| !s.is_empty()),
                        ) {
                            s.approvals.resolve(rid, oid.to_string());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn start_invocation(s: &Arc<Shared>, out: &Out, v: &Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|x| Uuid::parse_str(x).ok())
    else {
        return;
    };
    let spec = match AgentSpec::from_json(&v["agent"]) {
        Some(x) => x,
        None => {
            send_finish(out, inv_id, Err("agent 描述非法".into()), Vec::new(), 0);
            return;
        }
    };
    let prompt = v["prompt"].as_str().unwrap_or_default().to_string();
    // 没有存过运行时会话时 session_id 为 null，宿主就新开
    let resume_session = v
        .pointer("/context/session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let agent_id = spec.id;
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    // 先登记再 spawn：read_loop 是单线程收帧，取消帧不会插在登记完成之前
    s.cancels.lock().unwrap().insert(inv_id, cancel_tx);
    // Agent A13：有 tools_token 且本地 MCP 服务在跑 → 登记一条调用级映射。
    // McpHandle 随调用任务存活：成功/失败/取消/abort 都会 drop → 摘映射（404）。
    let tools_token = v["tools_token"].as_str().map(str::to_string);
    // 档位和审批通道随这次调用的 MCP 映射一起登记：send_message 在 read_only 时要能发起审批。
    let (ask_tx, ask_rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalNotice>();
    let approvals = ApprovalLink {
        tx: ask_tx,
        registry: s.approvals.clone(),
    };
    let (mcp_servers, mcp_handle) = match (s.mcp.as_ref(), tools_token.clone()) {
        (Some(mcp), Some(token)) => {
            let h = mcp.register_with(
                http_base(&s.cfg.server_url),
                token,
                spec.permission_tier.clone(),
                Some(approvals.clone()),
            );
            (
                vec![InjectedMcp {
                    name: "chat".into(),
                    http_url: h.url.clone(),
                    bridge: s.cfg.mcp_bridge.clone(),
                }],
                Some(h),
            )
        }
        (None, Some(_)) => {
            warn!("本地 MCP 服务未启动，调用 {inv_id} 不注入工具");
            (Vec::new(), None)
        }
        _ => (Vec::new(), None),
    };
    // Agent A05：附件清单与工作目录（<data_dir>/inv-<id>/{in,out}）
    let att_metas = crate::files::parse_attachments(&v["attachments"]);
    let http_base = http_base(&s.cfg.server_url);
    let http = s.http.clone();
    let out2 = out.clone();
    let running = s.running.clone();
    let cancels = s.cancels.clone();
    let dir = s.data_dir.clone();
    let registry_url = s.cfg.registry_url.clone();
    let cancel_flag = cancel_rx.clone();
    let ask_out = out.clone();
    let task = tokio::spawn(async move {
        // 映射的生命周期钉在这次调用上：任务结束/被 abort 时 drop
        let _mcp_handle = mcp_handle;
        let spec = match materialize(spec, &dir, &registry_url).await {
            Ok(s) => s,
            Err(e) => {
                if !*cancel_flag.borrow() {
                    send_finish(&out2, inv_id, Err(e), Vec::new(), 0);
                }
                running.lock().unwrap().remove(&inv_id);
                cancels.lock().unwrap().remove(&inv_id);
                return;
            }
        };
        let bridge = tokio::spawn(forward_approvals(ask_rx, ask_out, inv_id));
        // A05：工作目录随任务存活，结束时整树删除（inv-<id>/）
        let (_wd, in_dir, out_dir) = crate::files::WorkDir::create(&dir, inv_id);
        let rt = runtime::runtime_for(&spec);
        // A03：事件经 invocation_event 帧外发；文本段 ~100ms 窗口合并
        let (events, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let fwd = tokio::spawn(forward_events(ev_rx, out2.clone(), inv_id));
        let mut req = runtime::RunRequest::for_invocation(spec, prompt, dir);
        req.resume_session = resume_session;
        req.cancel = cancel_rx;
        req.mcp_servers = mcp_servers;
        req.approvals = Some(approvals);
        // A05：先下载输入附件再起 agent；下载失败的条目跳过
        req.attachments =
            crate::files::download_inputs(&http, &http_base, &in_dir, &att_metas).await;
        req.outbox = Some(out_dir.clone());
        let r = rt.run(req, events).await;
        bridge.abort();
        // run 结束 = events sender 释放，转发循环冲刷剩余文本后自行退出
        let _ = fwd.await;
        // 取消过的调用不回 invocation_finish（服务端已标 cancelled，再回也会被忽略）
        if !*cancel_flag.borrow() {
            // A05：凭据只在调用进行中有效——先传产出文件再回 finish
            let (files, files_skipped) = match &r {
                Ok(_) => {
                    crate::files::upload_outbox(&http, &http_base, &out_dir, tools_token.as_deref())
                        .await
                }
                Err(_) => (Vec::new(), 0),
            };
            if let Ok(out) = &r {
                // 正常调用结束后也上报一次可选项（session/new 里有才发）
                send_options(&out2, agent_id, out, false);
            }
            send_finish(&out2, inv_id, r, files, files_skipped);
        }
        running.lock().unwrap().remove(&inv_id);
        cancels.lock().unwrap().remove(&inv_id);
    });
    s.running.lock().unwrap().insert(inv_id, task);
}

/// agent_probe：只握手并新建会话，把可选项报回去，不起调用。
fn start_probe(s: &Arc<Shared>, out: &Out, v: &Value) {
    let Some(spec) = AgentSpec::from_json(&v["agent"]) else {
        return;
    };
    let agent_id = spec.id;
    let out2 = out.clone();
    let running = s.running.clone();
    let dir = s.data_dir.clone();
    let registry_url = s.cfg.registry_url.clone();
    let key = Uuid::new_v4();
    let task = tokio::spawn(async move {
        let spec = match materialize(spec, &dir, &registry_url).await {
            Ok(s) => s,
            Err(e) => {
                warn!("probe 无法解析登记表运行时: {e}");
                running.lock().unwrap().remove(&key);
                return;
            }
        };
        let rt = runtime::runtime_for(&spec);
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let req = runtime::RunRequest::for_probe(spec, dir);
        if let Ok(out) = rt.run(req, events).await {
            send_options(&out2, agent_id, &out, true);
        }
        running.lock().unwrap().remove(&key);
    });
    s.running.lock().unwrap().insert(key, task);
}

/// 登记表形状在这里换成本机命令；没装就返回 registry_not_installed。
async fn materialize(
    mut spec: AgentSpec,
    data_dir: &std::path::Path,
    registry_url: &str,
) -> Result<AgentSpec, String> {
    let Some(pin) = spec.registry.clone() else {
        return Ok(spec);
    };
    let launch =
        crate::registry::resolve_launch(data_dir, registry_url, &pin.id, &pin.version).await?;
    spec.command = launch.command;
    spec.args = launch.args;
    spec.env.extend(launch.env);
    Ok(spec)
}

fn start_registry_list(s: &Arc<Shared>, out: &Out, v: &Value) {
    let request_id = v["request_id"].clone();
    let s = s.clone();
    let out = out.clone();
    tokio::spawn(async move {
        let outcome = crate::registry::list_local(&s.data_dir, &s.cfg.registry_url).await;
        let msg = json!({
            "type": "registry_result",
            "request_id": request_id,
            "entries": outcome.entries,
            "error": outcome.error,
        });
        let _ = out.send(Message::Text(msg.to_string().into()));
    });
}

fn start_registry_install(s: &Arc<Shared>, out: &Out, v: &Value) {
    let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
    let version = v["version"].as_str().unwrap_or_default().to_string();
    let s = s.clone();
    let out = out.clone();
    tokio::spawn(async move {
        let entry_for_cb = entry_id.clone();
        let out_cb = out.clone();
        crate::registry::install(
            &s.data_dir,
            &s.cfg.registry_url,
            &entry_id,
            &version,
            move |stage, err| {
                let mut msg = json!({
                    "type": "registry_install_progress",
                    "entry_id": entry_for_cb,
                    "stage": stage,
                });
                if let Some(e) = err {
                    if !e.is_empty() {
                        msg["error"] = json!(e);
                    }
                }
                let _ = out_cb.send(Message::Text(msg.to_string().into()));
            },
        )
        .await;
    });
}

fn send_options(out: &Out, agent_id: Uuid, got: &RunOutput, force: bool) {
    if !force && got.modes.is_none() && got.models.is_none() {
        return;
    }
    let modes = options_json(got.modes.as_deref());
    let models = options_json(got.models.as_deref());
    let v = json!({
        "type": "agent_options",
        "agent_id": agent_id,
        "modes": modes,
        "models": models,
    });
    let _ = out.send(Message::Text(v.to_string().into()));
}

fn options_json(list: Option<&[AgentOption]>) -> Vec<Value> {
    list.unwrap_or(&[])
        .iter()
        .map(|o| json!({"id": o.id, "name": o.name}))
        .collect()
}

/// RunEvent 流 → invocation_event 帧。Text 事件在 100ms 窗口内合并成一帧；
/// 步骤事件先发走积压的文本再原样透传。通道关闭 = 运行结束，冲刷后退出。
async fn forward_events(mut rx: mpsc::UnboundedReceiver<RunEvent>, out: Out, inv_id: Uuid) {
    let mut buf = String::new();
    let flush_after = Duration::from_millis(100);
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(RunEvent::Text(t)) => {
                    buf.push_str(&t);
                    if deadline.is_none() {
                        deadline = Some(tokio::time::Instant::now() + flush_after);
                    }
                }
                Some(RunEvent::StepStart { id, title, kind }) => {
                    send_event(&out, inv_id, &mut buf,
                        json!({"type":"step_start","id":id,"title":title,"kind":kind}));
                    deadline = None;
                }
                Some(RunEvent::StepEnd { id, ok }) => {
                    send_event(&out, inv_id, &mut buf,
                        json!({"type":"step_end","id":id,"ok":ok}));
                    deadline = None;
                }
                None => {
                    send_event(&out, inv_id, &mut buf, Value::Null);
                    return;
                }
            },
            _ = wait_deadline(deadline) => {
                send_event(&out, inv_id, &mut buf, Value::Null);
                deadline = None;
            }
        }
    }
}

/// 有截止时间则睡到点，没有则永不就绪（select! 里安全代替 Option<Sleep>）。
async fn wait_deadline(d: Option<tokio::time::Instant>) {
    match d {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// 冲刷积压文本（若有）后再发 ev（Null = 只冲刷不发步骤帧）。
fn send_event(out: &Out, inv_id: Uuid, buf: &mut String, ev: Value) {
    if !buf.is_empty() {
        let t = std::mem::take(buf);
        send_event_frame(out, inv_id, json!({"type":"text","text":t}));
    }
    if !ev.is_null() {
        send_event_frame(out, inv_id, ev);
    }
}

/// 把超档请求原样转成 approval_request 帧。通道关闭 = 这次调用结束。
async fn forward_approvals(
    mut rx: mpsc::UnboundedReceiver<ApprovalNotice>,
    out: Out,
    inv_id: Uuid,
) {
    while let Some(ask) = rx.recv().await {
        let frame = json!({
            "type": "approval_request",
            "invocation_id": inv_id,
            "request_id": ask.request_id,
            "tool_call": ask.tool_call,
            "options": ask.options,
        });
        if out.send(Message::Text(frame.to_string().into())).is_err() {
            break;
        }
    }
}

fn send_event_frame(out: &Out, inv_id: Uuid, ev: Value) {
    let _ = out.send(Message::Text(
        json!({"type":"invocation_event","invocation_id":inv_id,"event":ev})
            .to_string()
            .into(),
    ));
}

/// A05：files 是本次调用上传到服务端的产出附件 id（按上传顺序），
/// files_skipped 是因配额被拒的数量（服务端追加提示到正文）。
fn send_finish(
    out: &Out,
    id: Uuid,
    r: Result<RunOutput, String>,
    files: Vec<String>,
    files_skipped: usize,
) {
    let v = match r {
        Ok(o) => json!({"type":"invocation_finish","invocation_id":id,"ok":true,
            "text": o.text, "session_id": o.session_id, "steps": o.steps,
            "files": files, "files_skipped": files_skipped}),
        // error 截断 500 字符，避免超长 stderr 刷屏。失败不带 session_id，服务端不更新上下文。
        Err(e) => json!({"type":"invocation_finish","invocation_id":id,"ok":false,
            "error":e.chars().take(500).collect::<String>()}),
    };
    let _ = out.send(Message::Text(v.to_string().into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events_of(msgs: &[Message]) -> Vec<Value> {
        msgs.iter()
            .map(|m| serde_json::from_str::<Value>(m.to_text().unwrap()).unwrap())
            .map(|v| v["event"].clone())
            .collect()
    }

    /// 密集的 Text 事件在 100ms 窗口内合并成一帧；收尾冲刷发出余下文本。
    #[tokio::test]
    async fn merges_text_within_100ms_window() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, mut got) = mpsc::unbounded_channel();
        let inv = Uuid::new_v4();
        let fwd = tokio::spawn(forward_events(rx, out, inv));
        for t in ["a", "b", "c"] {
            tx.send(RunEvent::Text(t.into())).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(RunEvent::Text("d".into())).unwrap();
        drop(tx);
        fwd.await.unwrap();
        let mut evs = Vec::new();
        while let Ok(m) = got.try_recv() {
            evs.push(m);
        }
        let evs = events_of(&evs);
        assert_eq!(
            evs,
            vec![
                json!({"type":"text","text":"abc"}),
                json!({"type":"text","text":"d"}),
            ]
        );
    }

    /// 步骤帧先发积压文本再透传，保证事件顺序不乱。
    #[tokio::test]
    async fn step_events_flush_pending_text_first() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (out, mut got) = mpsc::unbounded_channel();
        let inv = Uuid::new_v4();
        let fwd = tokio::spawn(forward_events(rx, out, inv));
        tx.send(RunEvent::Text("partial".into())).unwrap();
        tx.send(RunEvent::StepStart {
            id: "s1".into(),
            title: "读文件".into(),
            kind: "read".into(),
        })
        .unwrap();
        tx.send(RunEvent::StepEnd {
            id: "s1".into(),
            ok: false,
        })
        .unwrap();
        drop(tx);
        fwd.await.unwrap();
        let mut evs = Vec::new();
        while let Ok(m) = got.try_recv() {
            evs.push(m);
        }
        let evs = events_of(&evs);
        assert_eq!(
            evs,
            vec![
                json!({"type":"text","text":"partial"}),
                json!({"type":"step_start","id":"s1","title":"读文件","kind":"read"}),
                json!({"type":"step_end","id":"s1","ok":false}),
            ]
        );
    }
}
