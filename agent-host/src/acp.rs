// ACP（Agent Client Protocol）客户端：子进程 stdio 上的 JSON-RPC 2.0（NDJSON）。
//
// 每次调用拉起一个全新 Agent 进程：
//   initialize → session/new → session/prompt（收 session/update 流式分块）
//   → 等到 prompt 响应后杀进程。A01 不支持多轮续上下文，保持最简单实现。
//
// 宿主作为 ACP client 声明空 clientCapabilities：理论上 agent 不应发起
// fs/terminal 等反向请求；万一发起（含 session/request_permission）一律拒绝。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;
use uuid::Uuid;

/// invocation_start 帧里 agent 字段的运行时描述。
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: Uuid,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// 空串 = 用宿主数据目录作为 cwd
    pub cwd: String,
}

impl AgentSpec {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: Uuid::parse_str(v.get("id")?.as_str()?).ok()?,
            name: v.get("name")?.as_str()?.to_string(),
            command: v.pointer("/runtime/command")?.as_str()?.to_string(),
            args: v
                .pointer("/runtime/args")?
                .as_array()?
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect(),
            cwd: v
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
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
}

/// 跑一次调用：起进程、握手、发 prompt、收全部分块文本。
/// Ok(累计文本)；Err(人类可读原因，调用方截断)。
pub async fn run_invocation(
    spec: &AgentSpec,
    prompt: &str,
    data_dir: &Path,
) -> Result<String, String> {
    let dir: PathBuf = if spec.cwd.is_empty() {
        data_dir.to_path_buf()
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
    };
    let r = drive(&mut pipe, &dir, prompt).await;
    let _ = child.kill().await;
    r.map(|_| std::mem::take(&mut pipe.text))
}

async fn drive(pipe: &mut Pipe, dir: &Path, prompt: &str) -> Result<(), String> {
    pipe.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await?;
    let new = pipe
        .call("session/new", json!({"cwd": dir, "mcpServers": []}))
        .await?;
    let session_id = new["sessionId"].clone();
    pipe.call(
        "session/prompt",
        json!({"sessionId": session_id,
               "prompt": [{"type": "text", "text": prompt}]}),
    )
    .await?;
    Ok(())
}

impl Pipe {
    /// 发一个 JSON-RPC 请求并等到它的响应；期间顺带处理
    /// agent→client 反向请求与 session/update 通知。
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let msg = match self.rx.recv().await {
                Some(In::Msg(v)) => v,
                _ => return Err(self.exited()),
            };
            if msg.get("id").and_then(Value::as_u64) == Some(id)
                && (msg.get("result").is_some() || msg.get("error").is_some())
            {
                return match msg.get("error") {
                    Some(e) => Err(trunc(e["message"].as_str().unwrap_or("agent error"))),
                    None => Ok(msg["result"].clone()),
                };
            }
            if msg.get("id").is_some() && msg.get("method").is_some() {
                self.answer_request(&msg).await?;
            } else if let Some(m) = msg.get("method").and_then(Value::as_str) {
                self.on_notify(m, &msg["params"]);
            }
        }
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
