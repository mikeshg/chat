// ACP 服务端：initialize / session/new / session/load / session/set_model /
// session/prompt / session/cancel。
//
// 每会话一个底座 Agent；prompt 期间把 AgentEvent 翻成 session/update 推给
// 宿主，工具放行由 PermGate 钩子先走 session/request_permission。
// stdout 全是协议帧；mcp_servers 连不上、存档失败等都只记 stderr。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent::agent::{Agent, AgentEvent, AgentMessage, BeforeTool, Hooks, RunStop, Tool};
use agent::llm::{self, Message, OpenAiChat, OpenAiResponses, Provider};
use futures_util::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use super::store::{self, Archive};
use super::{mcp_client, tools, Rpc};

/// 一轮运行最多多少个模型回合（防工具循环跑飞）。
const TURN_LIMIT: u32 = 50;
/// tool_call_update 里回传的工具结果文本上限。
const TOOL_RESULT_PREVIEW: usize = 20000;

fn rpc_err(message: impl Into<String>) -> (i64, String) {
    (-32603, message.into())
}

/// 服务端下发的模型端点（CHAT_BUILTIN_ENDPOINTS 数组的一项）。
#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    pub id: String,
    pub name: String,
    /// "chat"（默认）| "responses"
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
}

impl Endpoint {
    fn from_json(v: &Value) -> Option<Self> {
        let id = v.get("id")?.as_str()?.trim().to_string();
        let base_url = v
            .get("base_url")?
            .as_str()?
            .trim_end_matches('/')
            .to_string();
        if id.is_empty() || base_url.is_empty() {
            return None;
        }
        Some(Self {
            name: v
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&id)
                .to_string(),
            protocol: v
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("chat")
                .to_string(),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            models: v
                .get("models")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            id,
            base_url,
        })
    }
}

fn endpoints_from_env() -> Vec<Endpoint> {
    std::env::var("CHAT_BUILTIN_ENDPOINTS")
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(Endpoint::from_json)
        .collect()
}

fn state_dir() -> PathBuf {
    std::env::var("CHAT_BUILTIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("chat-builtin"))
}

/// "<端点 id>/<模型>" → (端点, 模型名)；空串 = 第一个端点第一个模型。
fn resolve_model<'a>(
    endpoints: &'a [Endpoint],
    model_id: &str,
) -> Result<(&'a Endpoint, String), (i64, String)> {
    if endpoints.is_empty() {
        return Err(rpc_err("管理员还没有配置模型"));
    }
    if model_id.is_empty() {
        let ep = &endpoints[0];
        let Some(model) = ep.models.first() else {
            return Err(rpc_err("管理员还没有配置模型"));
        };
        return Ok((ep, model.clone()));
    }
    let Some((ep_id, model)) = model_id.split_once('/') else {
        return Err(rpc_err("未知模型"));
    };
    let Some(ep) = endpoints.iter().find(|e| e.id == ep_id) else {
        return Err(rpc_err("未知模型"));
    };
    if !ep.models.iter().any(|m| m == model) {
        return Err(rpc_err("未知模型"));
    }
    Ok((ep, model.to_string()))
}

/// 端点 + 模型 → Provider；返回生效的完整模型 id（"<端点>/<模型>"）。
fn provider_for(
    endpoints: &[Endpoint],
    model_id: &str,
) -> Result<(Arc<dyn Provider>, String), (i64, String)> {
    let (ep, model) = resolve_model(endpoints, model_id)?;
    let provider: Arc<dyn Provider> = if ep.protocol == "responses" {
        Arc::new(OpenAiResponses::new(&ep.base_url, &ep.api_key, &model))
    } else {
        Arc::new(OpenAiChat::new(&ep.base_url, &ep.api_key, &model).echo_reasoning(true))
    };
    Ok((provider, format!("{}/{}", ep.id, model)))
}

/// ACP session/new 的 models 字段。
fn models_json(endpoints: &[Endpoint], current: &str) -> Value {
    let available: Vec<Value> = endpoints
        .iter()
        .flat_map(|e| {
            e.models.iter().map(move |m| {
                json!({
                    "modelId": format!("{}/{}", e.id, m),
                    "name": format!("{} · {}", e.name, m),
                })
            })
        })
        .collect();
    json!({"currentModelId": current, "availableModels": available})
}

fn system_prompt(cwd: &Path) -> String {
    format!(
        "你是在用户电脑上工作的编码助手。工作目录：{}，操作系统：{}。\
         用 read_file / list_dir / grep 看代码，用 write_file / edit_file / shell 改东西、跑命令。\
         改文件前先读，不要臆测文件内容。回答简洁，用简体中文。",
        cwd.display(),
        std::env::consts::OS,
    )
}

/// session/prompt 的 prompt 块 → 给模型的纯文本。图片块模型看不了，
/// 留个占位说明；resource_link / resource 把路径写进文本，模型想用
/// 可以自己 read_file。
fn prompt_text(params: &Value) -> String {
    let mut parts = Vec::new();
    for block in params
        .get("prompt")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some("image") => {
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/*");
                parts.push(format!("[图片：{mime}，当前模型不可见]"));
            }
            Some("resource_link") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                parts.push(format!("[附件 {name}：{uri}]"));
            }
            Some("resource") => {
                let res = block.get("resource").cloned().unwrap_or(Value::Null);
                let uri = res.get("uri").and_then(Value::as_str).unwrap_or("");
                let text = res.get("text").and_then(Value::as_str);
                match text {
                    Some(t) => parts.push(format!("[附件 {uri}]\n{t}")),
                    None => parts.push(format!("[附件：{uri}]")),
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// 工具放行的会话记忆：allow_always 按 kind+locations 记住。
struct Gate {
    rpc: Rpc,
    session_id: String,
    cwd: PathBuf,
    allowed: Mutex<HashSet<String>>,
}

impl Gate {
    fn remember_key(kind: &str, locations: &[String]) -> String {
        format!("{kind}\u{1}{}", locations.join("\u{1}"))
    }
}

/// 底座钩子：每个工具调用前向宿主发 session/request_permission。
struct PermGate {
    gate: Arc<Gate>,
}

#[async_trait::async_trait]
impl Hooks for PermGate {
    async fn before_tool(&self, call: &llm::ToolCall, args: &Value) -> BeforeTool {
        let g = &self.gate;
        let (title, kind, locations) = tools::describe(&call.name, args, &g.cwd);
        let key = Gate::remember_key(&kind, &locations);
        if g.allowed.lock().unwrap().contains(&key) {
            return BeforeTool::Allow;
        }
        let params = json!({
            "sessionId": g.session_id,
            "toolCall": {
                "toolCallId": call.id,
                "title": title,
                "kind": kind,
                "status": "pending",
                "locations": locations.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
                "rawInput": args,
            },
            "options": [
                {"optionId":"allow_once","name":"允许一次","kind":"allow_once"},
                {"optionId":"allow_always","name":"总是允许","kind":"allow_always"},
                {"optionId":"reject_once","name":"拒绝","kind":"reject_once"},
            ],
        });
        let chosen = match g.rpc.call("session/request_permission", params).await {
            Ok(v) => v
                .pointer("/outcome/optionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Err(e) => {
                return BeforeTool::Block {
                    reason: format!("权限请求失败：{e}"),
                    terminate: false,
                };
            }
        };
        match chosen.as_str() {
            "allow_once" => BeforeTool::Allow,
            "allow_always" => {
                g.allowed.lock().unwrap().insert(key);
                BeforeTool::Allow
            }
            _ => BeforeTool::Block {
                reason: "用户拒绝了这次操作".into(),
                terminate: false,
            },
        }
    }
}

/// 正在跑的 run 的取消入口（底座的 CancellationToken 不经 agent 重导出，
/// 收进闭包里存着，避免为它单加一个依赖）。
type CancelFn = Box<dyn Fn() + Send + Sync>;

/// 一个会话：底座 Agent + 当前模型 + 正在跑的 run 的取消令牌。
pub(crate) struct Session {
    id: String,
    cwd: PathBuf,
    state_dir: PathBuf,
    endpoints: Arc<Vec<Endpoint>>,
    mcp_tools: Vec<Arc<dyn Tool>>,
    rpc: Rpc,
    gate: Arc<Gate>,
    model: Mutex<String>,
    agent: Mutex<Option<Agent>>,
    current: Mutex<Option<CancelFn>>,
    /// session/cancel 可能赶在 prompt 存令牌之前到；置位后 prompt 起始时补取消
    want_cancel: AtomicBool,
}

impl Session {
    fn create(
        id: String,
        cwd: PathBuf,
        endpoints: Arc<Vec<Endpoint>>,
        state_dir: PathBuf,
        rpc: Rpc,
        mcp_tools: Vec<Arc<dyn Tool>>,
        model_id: &str,
        transcript: Vec<AgentMessage>,
    ) -> Result<Arc<Self>, (i64, String)> {
        let (provider, effective) = provider_for(&endpoints, model_id).or_else(|e| {
            // load 的存档模型可能已被管理员删掉：回退默认模型
            if model_id.is_empty() {
                Err(e)
            } else {
                provider_for(&endpoints, "")
            }
        })?;
        let gate = Arc::new(Gate {
            rpc: rpc.clone(),
            session_id: id.clone(),
            cwd: cwd.clone(),
            allowed: Mutex::new(HashSet::new()),
        });
        let agent = build_agent(provider, &cwd, &gate, &mcp_tools, transcript);
        Ok(Arc::new(Self {
            id,
            cwd,
            state_dir,
            endpoints,
            mcp_tools,
            rpc,
            gate,
            model: Mutex::new(effective),
            agent: Mutex::new(Some(agent)),
            current: Mutex::new(None),
            want_cancel: AtomicBool::new(false),
        }))
    }

    fn current_model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// session/set_model：换 provider、保留 transcript。
    fn set_model(&self, model_id: &str) -> Result<(), (i64, String)> {
        let (provider, effective) = provider_for(&self.endpoints, model_id)?;
        let transcript = self
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.transcript())
            .unwrap_or_default();
        let agent = build_agent(provider, &self.cwd, &self.gate, &self.mcp_tools, transcript);
        *self.agent.lock().unwrap() = Some(agent);
        *self.model.lock().unwrap() = effective;
        Ok(())
    }

    fn cancel(&self) {
        self.want_cancel.store(true, Ordering::SeqCst);
        if let Some(f) = self.current.lock().unwrap().as_ref() {
            f();
        }
    }

    /// session/load：把恢复的 transcript 翻成 session/update 回放给宿主。
    async fn replay(&self) {
        let transcript = self
            .agent
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.transcript())
            .unwrap_or_default();
        for message in &transcript {
            let update = match message {
                AgentMessage::Llm(Message::User { text }) => {
                    json!({"sessionUpdate":"user_message_chunk",
                        "content":{"type":"text","text":text}})
                }
                AgentMessage::Llm(Message::Assistant(a)) if !a.text.is_empty() => {
                    json!({"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":a.text}})
                }
                _ => continue,
            };
            self.rpc.update(&self.id, update).await;
        }
    }

    /// session/prompt：跑一次到底，期间把事件翻成 session/update。
    async fn prompt(&self, text: String) -> Result<String, (i64, String)> {
        let agent = self
            .agent
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| rpc_err("会话未初始化"))?;
        let mut run = agent
            .prompt(text)
            .map_err(|e| rpc_err(format!("发起运行失败: {e}")))?;
        self.want_cancel.store(false, Ordering::SeqCst);
        let canceller = run.canceller();
        *self.current.lock().unwrap() = Some(Box::new(move || canceller.cancel()));
        // cancel 抢在令牌入槽之前到过：补一枪
        if self.want_cancel.load(Ordering::SeqCst) {
            run.cancel();
        }
        let mut stop = None;
        while let Some(event) = run.next().await {
            match event {
                AgentEvent::TextDelta(text) => {
                    self.rpc
                        .update(
                            &self.id,
                            json!({"sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":text}}),
                        )
                        .await;
                }
                AgentEvent::ReasoningDelta(text) => {
                    self.rpc
                        .update(
                            &self.id,
                            json!({"sessionUpdate":"agent_thought_chunk",
                                "content":{"type":"text","text":text}}),
                        )
                        .await;
                }
                AgentEvent::ToolStart { call } => {
                    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
                    let (title, kind, locations) = tools::describe(&call.name, &args, &self.cwd);
                    self.rpc
                        .update(
                            &self.id,
                            json!({"sessionUpdate":"tool_call",
                                "toolCallId": call.id,
                                "title": title,
                                "kind": kind,
                                "status": "pending",
                                "locations": locations.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
                                "rawInput": args}),
                        )
                        .await;
                }
                AgentEvent::ToolEnd { call_id, result } => {
                    let status = if result.is_error {
                        "failed"
                    } else {
                        "completed"
                    };
                    let text: String = result.content.chars().take(TOOL_RESULT_PREVIEW).collect();
                    self.rpc
                        .update(
                            &self.id,
                            json!({"sessionUpdate":"tool_call_update",
                                "toolCallId": call_id,
                                "status": status,
                                "content": [{"type":"content","content":{"type":"text","text":text}}]}),
                        )
                        .await;
                }
                AgentEvent::RunEnd(outcome) => stop = Some(outcome.stop),
                _ => {}
            }
        }
        *self.current.lock().unwrap() = None;
        // 本次跑完就存档：cancel / error 也把已产生的上下文留下
        store::save(
            &self.state_dir,
            &self.id,
            &Archive {
                model: self.current_model(),
                transcript: agent.transcript(),
            },
        );
        match stop {
            Some(RunStop::Finished) => Ok("end_turn".into()),
            Some(RunStop::TurnLimit) => Ok("max_turn_requests".into()),
            Some(RunStop::Cancelled) => Ok("cancelled".into()),
            Some(RunStop::Terminated) => Ok("refusal".into()),
            Some(RunStop::Error(e)) => Err(rpc_err(format!("模型调用失败：{e}"))),
            None => Err(rpc_err("运行未正常结束")),
            // RunStop 是 non_exhaustive：将来的新终态按正常结束处理
            _ => Ok("end_turn".into()),
        }
    }
}

fn build_agent(
    provider: Arc<dyn Provider>,
    cwd: &Path,
    gate: &Arc<Gate>,
    mcp_tools: &[Arc<dyn Tool>],
    transcript: Vec<AgentMessage>,
) -> Agent {
    let mut builder = Agent::builder(provider)
        .system(system_prompt(cwd))
        .turn_limit(TURN_LIMIT)
        .hooks(PermGate { gate: gate.clone() })
        .transcript(transcript);
    for tool in tools::all(cwd) {
        builder = builder.tool_arc(tool);
    }
    for tool in mcp_tools {
        builder = builder.tool_arc(tool.clone());
    }
    builder.build()
}

/// 会话目录参数：用宿主给的 cwd，拿不到用进程当前目录。
fn cwd_of(params: &Value) -> PathBuf {
    params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// ACP 服务端本体；dispatch 是同步分派，请求处理各跑一个任务，读循环不堵。
pub(crate) struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    rpc: Rpc,
    endpoints: Arc<Vec<Endpoint>>,
    state_dir: PathBuf,
    sessions: Mutex<std::collections::HashMap<String, Arc<Session>>>,
}

impl Server {
    pub(crate) fn new(rpc: Rpc) -> Self {
        Self {
            inner: Arc::new(Inner {
                rpc,
                endpoints: Arc::new(endpoints_from_env()),
                state_dir: state_dir(),
                sessions: Mutex::new(std::collections::HashMap::new()),
            }),
        }
    }

    /// 分派一帧：响应送回等在 pending 的出站请求；请求开任务处理；通知就地处理。
    pub(crate) fn dispatch(&self, msg: Value) {
        if self.inner.rpc.take_response(&msg) {
            return;
        }
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(id) = msg.get("id").cloned() {
            match method.as_str() {
                "initialize" | "session/new" | "session/load" | "session/set_model"
                | "session/prompt" => {
                    let inner = self.inner.clone();
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    tokio::spawn(async move {
                        let result = handle(&inner, &method, params).await;
                        inner.rpc.respond(&id, result).await;
                    });
                }
                _ => {
                    let rpc = self.inner.rpc.clone();
                    tokio::spawn(async move {
                        rpc.respond(&id, Err((-32601, "method not found".into())))
                            .await;
                    });
                }
            }
        } else if method == "session/cancel" {
            if let Some(sid) = msg.pointer("/params/sessionId").and_then(Value::as_str) {
                if let Some(session) = self.inner.sessions.lock().unwrap().get(sid) {
                    session.cancel();
                }
            }
        }
        // 其余通知（fs/read_text_file 等反向请求不带 id 不会发生；带来也忽略）
    }
}

async fn handle(inner: &Arc<Inner>, method: &str, params: Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true},
                "mcpCapabilities": {"http": true},
            },
        })),
        "session/new" => session_new(inner, &params).await,
        "session/load" => session_load(inner, &params).await,
        "session/set_model" => {
            let session = session_of(inner, &params)?;
            let model_id = params
                .get("modelId")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err("缺少 modelId"))?;
            session.set_model(model_id)?;
            Ok(json!({}))
        }
        "session/prompt" => {
            let session = session_of(inner, &params)?;
            let text = prompt_text(&params);
            let stop = session.prompt(text).await?;
            Ok(json!({"stopReason": stop}))
        }
        _ => Err(rpc_err("method not found")),
    }
}

fn session_of(inner: &Arc<Inner>, params: &Value) -> Result<Arc<Session>, (i64, String)> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| rpc_err("缺少 sessionId"))?;
    inner
        .sessions
        .lock()
        .unwrap()
        .get(sid)
        .cloned()
        .ok_or_else(|| rpc_err("未知会话"))
}

async fn session_new(inner: &Arc<Inner>, params: &Value) -> Result<Value, (i64, String)> {
    if inner.endpoints.is_empty() {
        return Err(rpc_err("管理员还没有配置模型"));
    }
    let cwd = cwd_of(params);
    let mcp_tools =
        mcp_client::connect_all(params.get("mcpServers").and_then(Value::as_array)).await;
    let id = Uuid::new_v4().to_string();
    let session = Session::create(
        id.clone(),
        cwd,
        inner.endpoints.clone(),
        inner.state_dir.clone(),
        inner.rpc.clone(),
        mcp_tools,
        "",
        Vec::new(),
    )?;
    inner
        .sessions
        .lock()
        .unwrap()
        .insert(id.clone(), session.clone());
    Ok(json!({
        "sessionId": id,
        "models": models_json(&inner.endpoints, &session.current_model()),
    }))
}

async fn session_load(inner: &Arc<Inner>, params: &Value) -> Result<Value, (i64, String)> {
    let sid = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| rpc_err("缺少 sessionId"))?
        .to_string();
    let Some(archive) = store::load(&inner.state_dir, &sid) else {
        return Err(rpc_err("会话已不存在或已过期"));
    };
    if inner.endpoints.is_empty() {
        return Err(rpc_err("管理员还没有配置模型"));
    }
    let cwd = cwd_of(params);
    let mcp_tools =
        mcp_client::connect_all(params.get("mcpServers").and_then(Value::as_array)).await;
    let session = Session::create(
        sid.clone(),
        cwd,
        inner.endpoints.clone(),
        inner.state_dir.clone(),
        inner.rpc.clone(),
        mcp_tools,
        &archive.model,
        archive.transcript,
    )?;
    // 先回放历史再回响应，宿主展示顺序就是发生顺序
    session.replay().await;
    inner.sessions.lock().unwrap().insert(sid, session);
    Ok(json!({}))
}
