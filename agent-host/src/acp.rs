// ACP（Agent Client Protocol）客户端：子进程 stdio 上的 JSON-RPC 2.0（NDJSON）。
//
// 每次调用拉起一个全新 Agent 进程：
//   initialize → session/new → session/prompt（收 session/update 流式分块）
//   → 等到 prompt 响应后杀进程。A01 不支持多轮续上下文，保持最简单实现。
//
// 宿主作为 ACP client 声明空 clientCapabilities：理论上 agent 不应发起
// fs/terminal 等反向请求；万一发起（含 session/request_permission）一律拒绝。

pub use crate::runtime::AgentSpec;
use crate::runtime::{AgentRuntime, Events, RunEvent, RunFuture, RunOutput, RunRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

/// 收到 session/cancel 后，最多再等这么久让 session/prompt 返回，然后杀进程。
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// ACP 运行时：每次调用起一个 agent 子进程。
pub struct AcpRuntime;

impl AgentRuntime for AcpRuntime {
    fn run(&self, req: RunRequest, events: Events) -> RunFuture {
        Box::pin(async move { run_invocation(req, events).await })
    }
}

enum In {
    Msg(Value),
    Eof,
}

struct Pipe {
    stdin: ChildStdin,
    rx: mpsc::UnboundedReceiver<In>,
    next_id: u64,
    /// 已累积的 agent_message_chunk 文本
    text: String,
    /// stderr 尾（诊断用，封顶 ~2KB）
    err: Arc<Mutex<String>>,
    events: Events,
}

/// 跑一次调用：起进程、握手、发 prompt、收全部分块文本（同时逐段发 RunEvent::Text）。
/// Err 为人类可读原因，调用方截断。
pub async fn run_invocation(mut req: RunRequest, events: Events) -> Result<RunOutput, String> {
    let spec = &req.spec;
    let dir: PathBuf = if spec.cwd.is_empty() {
        req.data_dir.clone()
    } else {
        PathBuf::from(&spec.cwd)
    };
    let mut child = Command::new(&spec.command)
        .args(&spec.args)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 宿主关闭/任务被 abort 时自动杀子进程
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn `{}` 失败: {e}", spec.command))?;

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let err = Arc::new(Mutex::new(String::new()));

    // stderr 只收集不展示（agent 自己的日志），留给错误信息做上下文
    if let Some(mut se) = child.stderr.take() {
        let err2 = err.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            use tokio::io::AsyncReadExt;
            loop {
                match se.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = err2.lock().unwrap();
                        if g.len() < 2048 {
                            g.push_str(&String::from_utf8_lossy(&buf[..n]));
                            let t: String = g.chars().take(2048).collect();
                            *g = t;
                        }
                    }
                }
            }
        });
    }

    let (tx, rx) = mpsc::unbounded_channel::<In>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&l) {
                        if tx.send(In::Msg(v)).is_err() {
                            return;
                        }
                    }
                }
                _ => {
                    let _ = tx.send(In::Eof);
                    return;
                }
            }
        }
    });

    let mut pipe = Pipe {
        stdin,
        rx,
        next_id: 1,
        text: String::new(),
        err,
        events,
    };
    let prompt = req.prompt.clone();
    let r = drive(&mut pipe, &dir, &prompt, &mut req.cancel).await;
    let _ = child.kill().await;
    r.map(|_| RunOutput {
        text: std::mem::take(&mut pipe.text),
    })
}

/// Ok(()) = prompt 正常结束或已按取消流程收尾（调用方用 cancel 标志决定要不要回 finish）。
async fn drive(
    pipe: &mut Pipe,
    dir: &Path,
    prompt: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    pipe.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await?;
    if *cancel.borrow() {
        return Ok(());
    }
    let new = pipe
        .call("session/new", json!({"cwd": dir, "mcpServers": []}))
        .await?;
    let session_id = new["sessionId"].clone();
    if *cancel.borrow() {
        let _ = pipe
            .notify("session/cancel", json!({"sessionId": session_id}))
            .await;
        return Ok(());
    }
    let id = pipe
        .send_req(
            "session/prompt",
            json!({"sessionId": session_id,
                   "prompt": [{"type": "text", "text": prompt}]}),
        )
        .await?;
    if pipe.wait_resp_or_cancel(id, cancel).await? {
        // 先通知 agent，再给 session/prompt 最多 5 秒；到点由调用方杀进程
        let _ = pipe
            .notify("session/cancel", json!({"sessionId": session_id}))
            .await;
        match tokio::time::timeout(CANCEL_GRACE, pipe.wait_resp(id)).await {
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
        }
    }
    Ok(())
}

impl Pipe {
    async fn send_req(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    /// ACP 通知（无 id，不等响应）。session/cancel 走这里。
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    /// 发一个 JSON-RPC 请求并等到它的响应；期间顺带处理
    /// agent→client 反向请求与 session/update 通知。
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send_req(method, params).await?;
        self.wait_resp(id).await
    }

    async fn wait_resp(&mut self, id: u64) -> Result<Value, String> {
        loop {
            let msg = match self.rx.recv().await {
                Some(In::Msg(v)) => v,
                _ => return Err(self.exited()),
            };
            if let Some(r) = self.take_resp(id, msg).await? {
                return r;
            }
        }
    }

    /// true = 取消信号已到（prompt 响应还没回来）。
    /// 发送端已丢（没人会取消）时退回普通等待，避免 changed() 立刻失败把循环转死。
    async fn wait_resp_or_cancel(
        &mut self,
        id: u64,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<bool, String> {
        let mut cancel_open = true;
        loop {
            if cancel_open && *cancel.borrow() {
                return Ok(true);
            }
            if !cancel_open {
                let msg = match self.rx.recv().await {
                    Some(In::Msg(v)) => v,
                    _ => return Err(self.exited()),
                };
                if let Some(r) = self.take_resp(id, msg).await? {
                    r?;
                    return Ok(false);
                }
                continue;
            }
            tokio::select! {
                biased;
                res = cancel.changed() => {
                    match res {
                        Ok(()) if *cancel.borrow() => return Ok(true),
                        Ok(()) => {}
                        Err(_) => cancel_open = false,
                    }
                }
                msg = self.rx.recv() => {
                    let msg = match msg {
                        Some(In::Msg(v)) => v,
                        _ => return Err(self.exited()),
                    };
                    if let Some(r) = self.take_resp(id, msg).await? {
                        r?;
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Some = 这帧就是本次请求的响应；None = 已当作旁路消息消化掉。
    async fn take_resp(
        &mut self,
        id: u64,
        msg: Value,
    ) -> Result<Option<Result<Value, String>>, String> {
        if msg.get("id").and_then(Value::as_u64) == Some(id)
            && (msg.get("result").is_some() || msg.get("error").is_some())
        {
            return Ok(Some(match msg.get("error") {
                Some(e) => Err(trunc(e["message"].as_str().unwrap_or("agent error"))),
                None => Ok(msg["result"].clone()),
            }));
        }
        if msg.get("id").is_some() && msg.get("method").is_some() {
            self.answer_request(&msg).await?;
        } else if let Some(m) = msg.get("method").and_then(Value::as_str) {
            self.on_notify(m, &msg["params"]);
        }
        Ok(None)
    }

    /// agent→client 请求：A01 一律不许写盘/跑终端；权限请求也选拒绝项。
    async fn answer_request(&mut self, msg: &Value) -> Result<(), String> {
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or_default();
        let resp = if method == "session/request_permission" {
            let deny = msg
                .pointer("/params/options")
                .and_then(Value::as_array)
                .and_then(|os| {
                    os.iter()
                        .find(|o| o["kind"].as_str().unwrap_or_default().contains("reject"))
                        .and_then(|o| o["optionId"].as_str())
                        .map(str::to_string)
                });
            match deny {
                Some(option_id) => json!({"jsonrpc":"2.0","id":id,
                    "result":{"outcome":{"outcome":"selected","optionId":option_id}}}),
                None => json!({"jsonrpc":"2.0","id":id,
                    "result":{"outcome":{"outcome":"cancelled"}}}),
            }
        } else {
            json!({"jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":"unsupported by chat agent host"}})
        };
        self.write(&resp).await
    }

    fn on_notify(&mut self, method: &str, params: &Value) {
        if method == "session/update"
            && params
                .pointer("/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("agent_message_chunk")
        {
            if let Some(t) = params
                .pointer("/update/content/text")
                .and_then(Value::as_str)
            {
                self.text.push_str(t);
                let _ = self.events.send(RunEvent::Text(t.to_string()));
            }
        }
    }

    async fn write(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| self.with_err(format!("stdin 写入失败: {e}")))
    }

    fn exited(&self) -> String {
        self.with_err("agent 进程提前退出".to_string())
    }

    fn with_err(&self, base: String) -> String {
        let tail = self.err.lock().unwrap();
        if tail.trim().is_empty() {
            trunc(&base)
        } else {
            trunc(&format!("{base}；stderr: {}", tail.trim()))
        }
    }
}

fn trunc(s: &str) -> String {
    s.chars().take(500).collect()
}
