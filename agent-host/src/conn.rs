// 宿主 WS 连接循环：连上→host_hello→host_ready→收 invocation_start 起任务。

use crate::runtime::{self, AgentSpec, RunRequest};
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
            send_finish(out, inv_id, Err("agent 描述非法".into()));
            return;
        }
    };
    let prompt = v["prompt"].as_str().unwrap_or_default().to_string();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    // 先登记再 spawn：read_loop 是单线程收帧，取消帧不会插在登记完成之前
    s.cancels.lock().unwrap().insert(inv_id, cancel_tx);
    let out2 = out.clone();
    let running = s.running.clone();
    let cancels = s.cancels.clone();
    let dir = s.data_dir.clone();
    let cancel_flag = cancel_rx.clone();
    let task = tokio::spawn(async move {
        let rt = runtime::runtime_for(&spec);
        // 事件暂不外发（接收端即刻丢弃）；流式输出票把它接到服务端
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let req = RunRequest {
            spec,
            prompt,
            data_dir: dir,
            cancel: cancel_rx,
        };
        let r = rt.run(req, events).await.map(|o| o.text);
        // 取消过的调用不回 invocation_finish（服务端已标 cancelled，再回也会被忽略）
        if !*cancel_flag.borrow() {
            send_finish(&out2, inv_id, r);
        }
        running.lock().unwrap().remove(&inv_id);
        cancels.lock().unwrap().remove(&inv_id);
    });
    s.running.lock().unwrap().insert(inv_id, task);
}

fn send_finish(out: &Out, id: Uuid, r: Result<String, String>) {
    let v = match r {
        Ok(text) => json!({"type":"invocation_finish","invocation_id":id,"ok":true,"text":text}),
        // error 截断 500 字符，避免超长 stderr 刷屏
        Err(e) => json!({"type":"invocation_finish","invocation_id":id,"ok":false,
            "error":e.chars().take(500).collect::<String>()}),
    };
    let _ = out.send(Message::Text(v.to_string().into()));
}
