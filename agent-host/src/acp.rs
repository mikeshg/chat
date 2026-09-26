// ACP（Agent Client Protocol）客户端：子进程 stdio 上的 JSON-RPC 2.0（NDJSON）。
//
// 每次调用仍拉起一个全新 Agent 进程：
//   initialize → 恢复已有会话或 session/new → session/prompt
//   → 等到 prompt 响应后杀进程。
// 恢复：agent 声明 sessionCapabilities.resume 时用 session/resume；
// 否则 loadSession=true 时用 session/load（响应返回前的回放不计入正文）；
// 都不支持或恢复调用失败则退回 session/new，不把恢复失败当成这次调用失败。
//
// 宿主作为 ACP client 声明空 clientCapabilities：理论上 agent 不应发起
// fs/terminal 等反向请求；万一发起（含 session/request_permission）一律拒绝。

pub use crate::runtime::AgentSpec;
use crate::runtime::{
    AgentOption, AgentRuntime, Events, RunEvent, RunFuture, RunOutput, RunRequest,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

/// ACP 运行时：每次调用起一个 agent 子进程。
pub struct AcpRuntime;

impl AgentRuntime for AcpRuntime {
    fn run(&self, req: RunRequest, events: Events) -> RunFuture {
        Box::pin(async move { run_invocation(&req, events).await })
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
    /// session/load 响应返回前的历史回放，不能算进本次回复
    suppress_chunks: bool,
}

/// 跑一次调用：起进程、握手、发 prompt、收全部分块文本（同时逐段发 RunEvent::Text）。
/// Err 为人类可读原因，调用方截断。
pub async fn run_invocation(req: &RunRequest, events: Events) -> Result<RunOutput, String> {
    let spec = &req.spec;
    let dir: PathBuf = if spec.cwd.is_empty() {
        req.data_dir.clone()
    } else {
        PathBuf::from(&spec.cwd)
    };
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 宿主关闭/任务被 abort 时自动杀子进程
        .kill_on_drop(true);
    // 在继承的环境上追加 / 覆盖，不清掉 PATH 等
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    let mut child = cmd
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
        suppress_chunks: false,
    };
    let r = drive(&mut pipe, &dir, req).await;
    let _ = child.kill().await;
    r.map(|d| RunOutput {
        text: std::mem::take(&mut pipe.text),
        session_id: d.session_id,
        modes: d.modes,
        models: d.models,
    })
}

/// drive 的结果：本次使用的会话 id，以及 session/new 报告的可选项（恢复会话时没有）
struct Driven {
    session_id: Option<String>,
    modes: Option<Vec<AgentOption>>,
    models: Option<Vec<AgentOption>>,
}

fn cap_resume(caps: &Value) -> bool {
    match caps.pointer("/sessionCapabilities/resume") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

/// initialize 响应里声明了 sessionCapabilities.additionalDirectories（true 或 {}）才传额外目录。
fn supports_additional_dirs(init: &Value) -> bool {
    match init.pointer("/agentCapabilities/sessionCapabilities/additionalDirectories") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

fn cap_load(caps: &Value) -> bool {
    caps.get("loadSession").and_then(Value::as_bool) == Some(true)
}

fn session_id_of(result: &Value, fallback: Option<&str>) -> Result<String, String> {
    if let Some(s) = result.get("sessionId").and_then(Value::as_str) {
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }
    if let Some(s) = fallback {
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }
    Err("缺少 sessionId".into())
}

fn option_list(v: Option<&Value>, id_keys: &[&str]) -> Option<Vec<AgentOption>> {
    let arr = v?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                let id = id_keys.iter().find_map(|k| item[*k].as_str())?;
                let name = item["name"].as_str().unwrap_or(id);
                Some(AgentOption {
                    id: id.to_string(),
                    name: name.to_string(),
                })
            })
            .collect(),
    )
}

fn in_options(id: &str, list: &Option<Vec<AgentOption>>) -> bool {
    list.as_ref()
        .is_some_and(|xs| xs.iter().any(|o| o.id == id))
}

async fn drive(pipe: &mut Pipe, dir: &Path, req: &RunRequest) -> Result<Driven, String> {
    let init = pipe
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await?;
    let caps = init
        .get("agentCapabilities")
        .cloned()
        .unwrap_or(Value::Null);
    // 能续上已有会话就续上（probe 不续）：模式、模型、额外目录、附加指令都沿用那个会话，不再设置
    if !req.probe {
        if let Some(sid) = resume_session(pipe, dir, req.resume_session.as_deref(), &caps).await {
            pipe.call(
                "session/prompt",
                json!({"sessionId": sid,
                       "prompt": [{"type": "text", "text": req.prompt}]}),
            )
            .await?;
            return Ok(Driven {
                session_id: Some(sid),
                modes: None,
                models: None,
            });
        }
    }
    // 新建会话：额外目录、模式、模型、附加指令都只挂在这条分支上
    let mut params = json!({"cwd": dir, "mcpServers": []});
    if supports_additional_dirs(&init) && !req.extra_dirs.is_empty() {
        params["additionalDirectories"] = json!(req.extra_dirs);
    }
    let new = pipe.call("session/new", params).await?;
    let modes = option_list(new.pointer("/modes/availableModes"), &["id"]);
    // ACP 模型 id 字段是 modelId；也认 id，方便假 agent 两种写法
    let models = option_list(new.pointer("/models/availableModels"), &["modelId", "id"]);
    if req.probe {
        return Ok(Driven {
            session_id: None,
            modes,
            models,
        });
    }
    let sid = session_id_of(&new, None)?;
    let session_id = json!(sid);
    if !req.mode.is_empty() && in_options(&req.mode, &modes) {
        pipe.call(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": req.mode}),
        )
        .await?;
    }
    if !req.model.is_empty() && in_options(&req.model, &models) {
        pipe.call(
            "session/set_model",
            json!({"sessionId": session_id, "modelId": req.model}),
        )
        .await?;
    }
    let mut blocks = Vec::new();
    if !req.instructions.is_empty() {
        blocks.push(json!({"type": "text", "text": req.instructions}));
    }
    blocks.push(json!({"type": "text", "text": req.prompt}));
    pipe.call(
        "session/prompt",
        json!({"sessionId": session_id, "prompt": blocks}),
    )
    .await?;
    Ok(Driven {
        session_id: Some(sid),
        modes,
        models,
    })
}

/// 按能力恢复会话；成功返回会话 id。没有能力或恢复失败返回 None（调用方新开，不算失败）。
async fn resume_session(
    pipe: &mut Pipe,
    dir: &Path,
    resume: Option<&str>,
    caps: &Value,
) -> Option<String> {
    let sid = resume.filter(|s| !s.is_empty())?;
    let params = json!({"sessionId": sid, "cwd": dir, "mcpServers": []});
    if cap_resume(caps) {
        if let Ok(r) = pipe.call("session/resume", params).await {
            return session_id_of(&r, Some(sid)).ok();
        }
    } else if cap_load(caps) {
        // load 会先回放历史：响应返回前的 agent_message_chunk 不算本次回复
        pipe.suppress_chunks = true;
        let loaded = pipe.call("session/load", params).await;
        pipe.suppress_chunks = false;
        if let Ok(r) = loaded {
            return session_id_of(&r, Some(sid)).ok();
        }
    }
    None
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
            // load 的历史回放发生在响应返回之前，不能混进本次回复
            if self.suppress_chunks {
                return;
            }
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
