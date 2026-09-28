// 宿主 WS 连接循环：连上→host_hello→host_ready→收 invocation_start 起任务。

use crate::outbound::OutItem;
use crate::runtime::{
    self, AgentOption, AgentSpec, ApprovalLink, ApprovalNotice, InjectedMcp, RunEvent, RunOutput,
};
use crate::Shared;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
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

    let running: Vec<String> = hello_running(s)
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    let mut pending = s.approvals.request_ids();
    pending.sort();
    sink.send(Message::Text(
        json!({
            "type": "host_hello",
            "install_id": s.cfg.install_id,
            "label": s.cfg.label,
            "platform": s.cfg.platform,
            // 旧服务端忽略未知字段。进行中含 finish 已生成但还没写进套接字的。
            "running": running,
            "pending_approvals": pending,
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

    // 写循环吃的是跨连接队列：断线只摘掉这一条，调用任务继续往队列里放。
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutItem>();
    let gen = s.lane.engage(out_tx);
    let lane = s.lane.clone();
    let writer = tokio::spawn(async move {
        while let Some(item) = out_rx.recv().await {
            let (seq, msg) = match item {
                OutItem::Eph(m) => (None, m),
                OutItem::Durable { seq, body } => (Some(seq), Message::Text(body.into())),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
            if let Some(seq) = seq {
                lane.ack(gen, seq);
            }
        }
        let _ = sink.close().await;
    });
    s.lane.attach_writer(writer);
    // 本函数以任何方式结束（读完、出错、宿主 shutdown 把任务取消）都摘掉出站并关掉套接字。
    // finish / 审批请求留在队列里；等审批的调用不叫醒。
    let _guard = LaneGuard {
        lane: s.lane.clone(),
    };

    read_loop(s, &mut stream).await
}

/// connect_once 结束时关掉当前连接的写侧。Drop 覆盖中途取消，避免套接字被写任务握着不放。
struct LaneGuard {
    lane: crate::outbound::Outbound,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        self.lane.retire();
    }
}

/// 还在跑的调用，加上 finish 已经生成但套接字还没写成功的。
fn hello_running(s: &Shared) -> Vec<Uuid> {
    let mut ids: HashSet<Uuid> = s.cancels.lock().unwrap().keys().copied().collect();
    for id in s.lane.invocation_ids() {
        ids.insert(id);
    }
    let mut v: Vec<Uuid> = ids.into_iter().collect();
    v.sort();
    v
}

async fn read_loop(
    s: &Arc<Shared>,
    stream: &mut futures_util::stream::SplitStream<Sock>,
) -> Result<(), String> {
    loop {
        let msg = stream
            .next()
            .await
            .ok_or("ws 关闭")?
            .map_err(|e| e.to_string())?;
        match msg {
            Message::Ping(p) => {
                s.lane.emit_ephemeral(Message::Pong(p));
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
                    "invocation_start" => start_invocation(s, &v),
                    "agent_probe" => start_probe(s, &v),
                    "registry_list" => start_registry_list(s, &v),
                    "registry_install" => start_registry_install(s, &v),
                    "invocation_cancel" => {
                        // 不直接 abort：让 ACP 先发 session/cancel，最多再等 5 秒才杀进程。
                        // 这次调用不再回 invocation_finish。
                        // 还没登记的 id 先记住，后面的 start 直接丢掉。
                        if let Some(id) = v["invocation_id"]
                            .as_str()
                            .and_then(|x| Uuid::parse_str(x).ok())
                        {
                            if let Some(tx) = s.cancels.lock().unwrap().get(&id) {
                                let _ = tx.send(true);
                            } else {
                                s.pre_cancel.remember(id);
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

fn start_invocation(s: &Arc<Shared>, v: &Value) {
    let Some(inv_id) = v["invocation_id"]
        .as_str()
        .and_then(|x| Uuid::parse_str(x).ok())
    else {
        return;
    };
    // 取消先到：不起进程，也不回 finish（服务端已经是终态）。
    if s.pre_cancel.take(inv_id) {
        return;
    }
    if s.cancels.lock().unwrap().contains_key(&inv_id) {
        return;
    }
    let spec = match AgentSpec::from_json(&v["agent"]) {
        Some(x) => x,
        None => {
            send_finish(&s.lane, inv_id, Err("agent 描述非法".into()), Vec::new(), 0);
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
    let lane = s.lane.clone();
    let lane_finish = lane.clone();
    let running = s.running.clone();
    let cancels = s.cancels.clone();
    let dir = s.data_dir.clone();
    let registry_url = s.cfg.registry_url.clone();
    let cancel_flag = cancel_rx.clone();
    let ask_lane = lane.clone();
    let task = tokio::spawn(async move {
        // 映射的生命周期钉在这次调用上：任务结束/被 abort 时 drop
        let _mcp_handle = mcp_handle;
        let spec = match materialize(spec, &dir, &registry_url).await {
            Ok(s) => s,
            Err(e) => {
                if !*cancel_flag.borrow() {
                    send_finish(&lane_finish, inv_id, Err(e), Vec::new(), 0);
                }
                running.lock().unwrap().remove(&inv_id);
                cancels.lock().unwrap().remove(&inv_id);
                return;
            }
        };
        let bridge = tokio::spawn(forward_approvals(ask_rx, ask_lane, inv_id));
        // A05：工作目录随任务存活，结束时整树删除（inv-<id>/）
        let (_wd, in_dir, out_dir) = crate::files::WorkDir::create(&dir, inv_id);
        let rt = runtime::runtime_for(&spec);
        // A03：事件经 invocation_event 帧外发；文本段 ~100ms 窗口合并。
        // 离线时增量丢掉，finish 里仍有全文。
        let (events, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let lane_ev = lane.clone();
        let fwd = tokio::spawn(forward_events(
            ev_rx,
            move |m| lane_ev.emit_ephemeral(m),
            inv_id,
        ));
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
                send_options(&lane, agent_id, out, false);
            }
            send_finish(&lane_finish, inv_id, r, files, files_skipped);
        }
        running.lock().unwrap().remove(&inv_id);
        cancels.lock().unwrap().remove(&inv_id);
    });
    s.running.lock().unwrap().insert(inv_id, task);
}

/// 只握手并新建会话，读程序提供的模型 / 模式，不发 prompt。
/// 桌面端添加 Agent 前直接调它（不经过服务端）。
pub async fn probe(
    spec: AgentSpec,
    data_dir: std::path::PathBuf,
    registry_url: &str,
) -> Result<RunOutput, String> {
    let spec = materialize(spec, &data_dir, registry_url).await?;
    let rt = runtime::runtime_for(&spec);
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    rt.run(runtime::RunRequest::for_probe(spec, data_dir), events)
        .await
}

/// agent_probe：成功时照旧发 agent_options；帧里带 request_id 时另回
/// probe_result（含失败原因），服务端据此同步应答。
fn start_probe(s: &Arc<Shared>, v: &Value) {
    let request_id = v.get("request_id").cloned().filter(|r| !r.is_null());
    let lane = s.lane.clone();
    let Some(spec) = AgentSpec::from_json(&v["agent"]) else {
        if let Some(rid) = request_id {
            send_probe_result(&lane, rid, Err("运行时描述不完整".into()));
        }
        return;
    };
    let agent_id = spec.id;
    let running = s.running.clone();
    let dir = s.data_dir.clone();
    let registry_url = s.cfg.registry_url.clone();
    let key = Uuid::new_v4();
    let task = tokio::spawn(async move {
        let got = probe(spec, dir, &registry_url).await;
        match &got {
            Ok(out) => send_options(&lane, agent_id, out, true),
            Err(e) => warn!("probe 失败: {e}"),
        }
        if let Some(rid) = request_id {
            send_probe_result(&lane, rid, got);
        }
        running.lock().unwrap().remove(&key);
    });
    s.running.lock().unwrap().insert(key, task);
}

fn send_probe_result(
    lane: &crate::outbound::Outbound,
    request_id: Value,
    got: Result<RunOutput, String>,
) {
    let v = match got {
        Ok(out) => json!({
            "type": "probe_result",
            "request_id": request_id,
            "modes": options_json(out.modes.as_deref()),
            "models": options_json(out.models.as_deref()),
        }),
        Err(e) => json!({"type": "probe_result", "request_id": request_id, "error": e}),
    };
    lane.emit_ephemeral(Message::Text(v.to_string().into()));
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

fn start_registry_list(s: &Arc<Shared>, v: &Value) {
    let request_id = v["request_id"].clone();
    let s = s.clone();
    tokio::spawn(async move {
        let outcome = crate::registry::list_local(&s.data_dir, &s.cfg.registry_url).await;
        let msg = json!({
            "type": "registry_result",
            "request_id": request_id,
            "entries": outcome.entries,
            "error": outcome.error,
        });
        s.lane.emit_ephemeral(Message::Text(msg.to_string().into()));
    });
}

fn start_registry_install(s: &Arc<Shared>, v: &Value) {
    let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
    let version = v["version"].as_str().unwrap_or_default().to_string();
    let s = s.clone();
    tokio::spawn(async move {
        let entry_for_cb = entry_id.clone();
        let lane = s.lane.clone();
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
                lane.emit_ephemeral(Message::Text(msg.to_string().into()));
            },
        )
        .await;
    });
}

fn send_options(lane: &crate::outbound::Outbound, agent_id: Uuid, got: &RunOutput, force: bool) {
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
    lane.emit_ephemeral(Message::Text(v.to_string().into()));
}

fn options_json(list: Option<&[AgentOption]>) -> Vec<Value> {
    list.unwrap_or(&[])
        .iter()
        .map(|o| json!({"id": o.id, "name": o.name}))
        .collect()
}

/// RunEvent 流 → invocation_event 帧。Text 事件在 100ms 窗口内合并成一帧；
/// 步骤事件先发走积压的文本再原样透传。通道关闭 = 运行结束，冲刷后退出。
async fn forward_events<F>(mut rx: mpsc::UnboundedReceiver<RunEvent>, emit: F, inv_id: Uuid)
where
    F: Fn(Message) + Send,
{
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
                    send_event(&emit, inv_id, &mut buf,
                        json!({"type":"step_start","id":id,"title":title,"kind":kind}));
                    deadline = None;
                }
                Some(RunEvent::StepEnd { id, ok }) => {
                    send_event(&emit, inv_id, &mut buf,
                        json!({"type":"step_end","id":id,"ok":ok}));
                    deadline = None;
                }
                None => {
                    send_event(&emit, inv_id, &mut buf, Value::Null);
                    return;
                }
            },
            _ = wait_deadline(deadline) => {
                send_event(&emit, inv_id, &mut buf, Value::Null);
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
fn send_event(emit: &impl Fn(Message), inv_id: Uuid, buf: &mut String, ev: Value) {
    if !buf.is_empty() {
        let t = std::mem::take(buf);
        send_event_frame(emit, inv_id, json!({"type":"text","text":t}));
    }
    if !ev.is_null() {
        send_event_frame(emit, inv_id, ev);
    }
}

/// 把超档请求转成 approval_request。离线时进补发队列，不中断等待。
async fn forward_approvals(
    mut rx: mpsc::UnboundedReceiver<ApprovalNotice>,
    lane: crate::outbound::Outbound,
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
        lane.emit_durable(inv_id, frame.to_string());
    }
}

fn send_event_frame(emit: &impl Fn(Message), inv_id: Uuid, ev: Value) {
    emit(Message::Text(
        json!({"type":"invocation_event","invocation_id":inv_id,"event":ev})
            .to_string()
            .into(),
    ));
}

/// A05：files 是本次调用上传到服务端的产出附件 id（按上传顺序），
/// files_skipped 是因配额被拒的数量（服务端追加提示到正文）。
fn send_finish(
    lane: &crate::outbound::Outbound,
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
    // 离线也要留下，重连后补发。正文和步骤都在这一帧里。
    lane.emit_durable(id, v.to_string());
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
        let fwd = tokio::spawn(async move {
            forward_events(
                rx,
                move |m| {
                    let _ = out.send(m);
                },
                inv,
            )
            .await;
        });
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
        let fwd = tokio::spawn(async move {
            forward_events(
                rx,
                move |m| {
                    let _ = out.send(m);
                },
                inv,
            )
            .await;
        });
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
