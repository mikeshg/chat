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
// fs/terminal 等反向请求；万一发起一律拒绝。session/request_permission 按
// 权限档位自动放行，超档则等服务端的审批决定（A12）。

use crate::permission;
pub use crate::runtime::AgentSpec;
use crate::runtime::{
    AgentOption, AgentRuntime, ApprovalLink, ApprovalNotice, Events, InjectedMcp, LocalFile,
    RunEvent, RunFuture, RunOutput, RunRequest, StepRecord,
};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

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
    /// 本次拒绝的权限请求次数
    permissions_rejected: u32,
    /// session/load 响应返回前的历史回放，不能算进本次回复
    suppress_chunks: bool,
    /// 进行中的 tool_call：id → (title, kind)
    open_steps: HashMap<String, (String, String)>,
    /// 已完成步骤（按收尾顺序）
    steps: Vec<StepRecord>,
    /// 权限档位（空串已归一成 read_only）
    tier: String,
    /// 工作目录在前，后面是额外目录
    roots: Vec<PathBuf>,
    approvals: Option<ApprovalLink>,
    cancel: watch::Receiver<bool>,
}

/// 跑一次调用：起进程、握手、发 prompt、收全部分块文本（同时逐段发 RunEvent::Text）。
/// Err 为人类可读原因，调用方截断。
pub async fn run_invocation(req: RunRequest, events: Events) -> Result<RunOutput, String> {
    let spec = &req.spec;
    let dir: PathBuf = if spec.cwd.is_empty() {
        req.data_dir.clone()
    } else {
        PathBuf::from(&spec.cwd)
    };
    // PATH + PATHEXT；.cmd / .bat 经 cmd.exe 启动（含自定义命令和 npm 垫片）
    let mut cmd = crate::command::prepare(&spec.command, &spec.args);
    cmd.current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 宿主关闭/任务被 abort 时自动杀子进程
        .kill_on_drop(true);
    // 在继承的环境上追加 / 覆盖，不清掉 PATH 等
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    // A05：产出文件目录；agent 往这里写文件，宿主在调用成功后逐个上传回会话
    if let Some(o) = &req.outbox {
        cmd.env("CHAT_AGENT_OUTBOX", o);
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
        permissions_rejected: 0,
        suppress_chunks: false,
        open_steps: HashMap::new(),
        steps: Vec::new(),
        tier: if req.permission_tier.is_empty() {
            "read_only".into()
        } else {
            req.permission_tier.clone()
        },
        roots: permission_roots(&dir, &req.extra_dirs),
        approvals: req.approvals.clone(),
        cancel: req.cancel.clone(),
    };
    // 取消信号单独克隆一份：drive 只读 req，等待取消需要可变的 Receiver
    let mut cancel = req.cancel.clone();
    let r = drive(&mut pipe, &dir, &req, &mut cancel).await;
    let _ = child.kill().await;
    r.map(|d| RunOutput {
        text: std::mem::take(&mut pipe.text),
        permissions_rejected: pipe.permissions_rejected,
        session_id: d.session_id,
        modes: d.modes,
        models: d.models,
        steps: std::mem::take(&mut pipe.steps),
    })
}

/// drive 的结果：本次使用的会话 id，以及 session/new 报告的可选项（恢复会话时没有）
#[derive(Default)]
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

/// A05：initialize 响应里声明了 promptCapabilities.image（true 或 {}）→ 图片走 image 块。
fn cap_image(init: &Value) -> bool {
    match init.pointer("/agentCapabilities/promptCapabilities/image") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

/// 附件 → ACP 内容块。声明了 image 能力的图片走 base64 image 块；
/// 其余（含没声明能力的图片、读不出内容的）走 resource_link。
fn attachment_block(f: &LocalFile, image_ok: bool) -> Value {
    if image_ok && f.mime.starts_with("image/") {
        if let Ok(bytes) = std::fs::read(&f.path) {
            return json!({
                "type": "image",
                "mimeType": f.mime,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
    }
    let size = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
    json!({
        "type": "resource_link",
        "uri": file_uri(&f.path),
        "name": f.name,
        "mimeType": f.mime,
        "size": size,
    })
}

/// file:///… 形式：Windows 反斜杠转正斜杠，盘符前补 /。
fn file_uri(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s = format!("/{s}");
    }
    format!("file://{s}")
}

/// 附件块 + 用户文本 +（有 outbox 时的）产出提示，组成 session/prompt 的块列表。
/// 新建会话的附加指令块仍由调用方放在最前。
fn prompt_blocks(req: &RunRequest, image_ok: bool) -> Vec<Value> {
    let mut blocks: Vec<Value> = req
        .attachments
        .iter()
        .map(|f| attachment_block(f, image_ok))
        .collect();
    blocks.push(json!({"type": "text", "text": req.prompt}));
    if let Some(o) = &req.outbox {
        blocks.push(json!({"type": "text", "text": format!(
            "如需把文件发给用户，把文件保存到 {}", o.display()
        )}));
    }
    blocks
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

/// 主人没选模式时，切到「凡事先问」的模式，让档位决定放行什么。codex-acp 用
/// `_meta.kind = "standard"` 标它；它的默认模式 auto_review 会自己批掉请求，档位就管不到了。
/// 没有这个标记的 agent（如 Claude 默认就先问）不动。
fn ask_first_mode(modes: Option<&Value>) -> Option<String> {
    let modes = modes?;
    let id = modes["availableModes"]
        .as_array()?
        .iter()
        .find(|m| m.pointer("/_meta/kind").and_then(Value::as_str) == Some("standard"))?["id"]
        .as_str()?;
    (modes["currentModeId"].as_str() != Some(id)).then(|| id.to_string())
}

fn in_options(id: &str, list: &Option<Vec<AgentOption>>) -> bool {
    list.as_ref()
        .is_some_and(|xs| xs.iter().any(|o| o.id == id))
}

/// Agent A13：按 Agent 声明的能力把注入的 MCP 服务折成 ACP mcpServers 条目。
/// Agent 声明 mcpCapabilities.http → http 形态直连；否则有桥接配置 → stdio
/// 桥接条目（URL 追加为最后一个参数）；都不行 → 不注入并告警。
fn mcp_entries(init: &Value, injected: &[InjectedMcp]) -> Vec<Value> {
    let http_ok = init
        .pointer("/agentCapabilities/mcpCapabilities/http")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = Vec::new();
    for m in injected {
        if http_ok {
            out.push(json!({"type":"http","name":m.name,"url":m.http_url,"headers":[]}));
        } else if let Some((cmd, args)) = &m.bridge {
            let mut a = args.clone();
            a.push(m.http_url.clone());
            out.push(json!({"name":m.name,"command":cmd,"args":a,"env":[]}));
        } else {
            tracing::warn!(
                "Agent 未声明 http MCP 能力且未配置桥接：{} 的注入工具不可用",
                m.name
            );
        }
    }
    out
}

/// 主人取消时照样返回 Ok（调用方看取消标志决定要不要回 finish）。
async fn drive(
    pipe: &mut Pipe,
    dir: &Path,
    req: &RunRequest,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Driven, String> {
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
    if *cancel.borrow() {
        return Ok(Driven::default());
    }
    // A05：图片能不能走 image 块看 initialize 声明的能力
    let image_ok = cap_image(&init);
    // 注入的工具（A13）：新建和恢复会话都要带上
    let mcp = json!(mcp_entries(&init, &req.mcp_servers));
    // 能续上已有会话就续上（probe 不续）：模式、模型、额外目录、附加指令都沿用那个会话，不再设置
    if !req.probe {
        if let Some(sid) =
            resume_session(pipe, dir, req.resume_session.as_deref(), &caps, &mcp).await
        {
            // A05：恢复路径同样带附件块与 outbox 提示
            let prompt = json!(prompt_blocks(req, image_ok));
            prompt_or_cancel(pipe, &json!(sid), prompt, cancel).await?;
            return Ok(Driven {
                session_id: Some(sid),
                modes: None,
                models: None,
            });
        }
    }
    // 新建会话：额外目录、模式、模型、附加指令都只挂在这条分支上
    let mut params = json!({"cwd": dir, "mcpServers": mcp});
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
    if *cancel.borrow() {
        let _ = pipe
            .notify("session/cancel", json!({"sessionId": session_id}))
            .await;
        return Ok(Driven::default());
    }
    let mode = if req.mode.is_empty() {
        ask_first_mode(new.get("modes"))
    } else {
        in_options(&req.mode, &modes).then(|| req.mode.clone())
    };
    if let Some(mode) = mode {
        pipe.call(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": mode}),
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
    // A05：附件块插在指令之后、用户文本之前
    blocks.extend(prompt_blocks(req, image_ok));
    prompt_or_cancel(pipe, &session_id, json!(blocks), cancel).await?;
    Ok(Driven {
        session_id: Some(sid),
        modes,
        models,
    })
}

/// 发 session/prompt 并等它返回；期间主人取消 → 通知 session/cancel，最多再等 CANCEL_GRACE，
/// 之后由调用方结束子进程。
async fn prompt_or_cancel(
    pipe: &mut Pipe,
    session_id: &Value,
    prompt: Value,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let id = pipe
        .send_req(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": prompt}),
        )
        .await?;
    if pipe.wait_resp_or_cancel(id, cancel).await? {
        let _ = pipe
            .notify("session/cancel", json!({"sessionId": session_id}))
            .await;
        let _ = tokio::time::timeout(CANCEL_GRACE, pipe.wait_resp(id)).await;
    }
    Ok(())
}

/// 按能力恢复会话；成功返回会话 id。没有能力或恢复失败返回 None（调用方新开，不算失败）。
async fn resume_session(
    pipe: &mut Pipe,
    dir: &Path,
    resume: Option<&str>,
    caps: &Value,
    mcp: &Value,
) -> Option<String> {
    let sid = resume.filter(|s| !s.is_empty())?;
    let params = json!({"sessionId": sid, "cwd": dir, "mcpServers": mcp});
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

    /// agent→client 请求：写盘/跑终端仍拒绝。权限请求按档位自动放行或等审批。
    async fn answer_request(&mut self, msg: &Value) -> Result<(), String> {
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or_default();
        let resp = if method == "session/request_permission" {
            let resp = self.permission_response(id, &msg["params"]).await?;
            // 没被放行（取消或选了拒绝类选项）的次数：真 agent 冒烟用它断言权限请求确实走到了宿主
            if !granted(&resp, &msg["params"]) {
                self.permissions_rejected += 1;
            }
            resp
        } else {
            json!({"jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":"unsupported by chat agent host"}})
        };
        self.write(&resp).await
    }

    /// Ok(response) 写回 agent。Err 表示审批通道断了，整次调用失败（不把取消写成成功）。
    async fn permission_response(&mut self, id: Value, params: &Value) -> Result<Value, String> {
        let tool = permission::summarize_tool(&params["toolCall"]);
        let options = params.get("options").cloned().unwrap_or(Value::Null);
        let kind = tool["kind"].as_str().unwrap_or("other");
        let locations = permission::location_strings(&tool["locations"]);
        if permission::within_tier(&self.tier, kind, &locations, &self.roots) {
            if let Some(option_id) = permission::pick_allow(&options) {
                return Ok(selected(&id, &option_id));
            }
        }
        // 没有可选项就没法等主人挑，直接取消。
        if !permission::has_option(&options) {
            return Ok(cancelled_outcome(&id));
        }
        let Some(link) = self.approvals.clone() else {
            return Ok(cancelled_outcome(&id));
        };
        let request_id = Uuid::new_v4().to_string();
        let (tx, mut rx) = oneshot::channel();
        link.registry.insert(request_id.clone(), tx);
        if link
            .tx
            .send(ApprovalNotice {
                request_id: request_id.clone(),
                tool_call: tool,
                options,
            })
            .is_err()
        {
            link.registry.finish(&request_id);
            return Err("审批通道已关闭".into());
        }
        if *self.cancel.borrow() {
            link.registry.finish(&request_id);
            return Ok(cancelled_outcome(&id));
        }
        let decided = loop {
            tokio::select! {
                biased;
                res = self.cancel.changed() => {
                    if res.is_err() || *self.cancel.borrow() {
                        link.registry.finish(&request_id);
                        return Ok(cancelled_outcome(&id));
                    }
                }
                ans = &mut rx => {
                    link.registry.finish(&request_id);
                    break ans;
                }
            }
        };
        match decided {
            Ok(option_id) => Ok(selected(&id, &option_id)),
            // 连接断开清掉了等待槽：不要再回一个让 agent 继续的结果。
            Err(_) => Err("审批等待被中断".into()),
        }
    }

    fn on_notify(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            return;
        }
        // load 的历史回放发生在响应返回之前，不能混进本次回复（正文和步骤都不算）
        if self.suppress_chunks {
            return;
        }
        let upd = &params["update"];
        match upd["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(t) = upd.pointer("/content/text").and_then(Value::as_str) {
                    self.text.push_str(t);
                    let _ = self.events.send(RunEvent::Text(t.to_string()));
                }
            }
            // A03：tool_call → StepStart；tool_call_update 终态 → StepEnd + 步骤记录
            Some("tool_call") => {
                if let Some(id) = upd["toolCallId"].as_str() {
                    let title = upd["title"].as_str().unwrap_or_default().to_string();
                    let kind = upd["kind"].as_str().unwrap_or("other").to_string();
                    self.open_steps
                        .insert(id.to_string(), (title.clone(), kind.clone()));
                    let _ = self.events.send(RunEvent::StepStart {
                        id: id.to_string(),
                        title,
                        kind,
                    });
                }
            }
            Some("tool_call_update") => {
                let Some(id) = upd["toolCallId"].as_str() else {
                    return;
                };
                // 只有终态算“收尾”：completed 成功，failed/stopped 等非成功 → ok=false
                let ok = match upd["status"].as_str() {
                    Some("completed") => true,
                    Some("failed") | Some("stopped") => false,
                    _ => return,
                };
                let (title, kind) = self
                    .open_steps
                    .remove(id)
                    .unwrap_or_else(|| (String::new(), "other".into()));
                self.steps.push(StepRecord {
                    id: id.to_string(),
                    title,
                    kind,
                    ok,
                });
                let _ = self.events.send(RunEvent::StepEnd {
                    id: id.to_string(),
                    ok,
                });
            }
            _ => {}
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

fn permission_roots(cwd: &Path, extra: &[String]) -> Vec<PathBuf> {
    let mut roots = vec![cwd.to_path_buf()];
    for dir in extra {
        let dir = dir.trim();
        if !dir.is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }
    roots
}

fn selected(id: &Value, option_id: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,
        "result":{"outcome":{"outcome":"selected","optionId":option_id}}})
}

fn cancelled_outcome(id: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,
        "result":{"outcome":{"outcome":"cancelled"}}})
}

/// 权限回复是否放行：选中的选项 kind 含 allow 才算；取消或拒绝类选项都不算。
fn granted(resp: &Value, params: &Value) -> bool {
    let outcome = &resp["result"]["outcome"];
    if outcome["outcome"].as_str() != Some("selected") {
        return false;
    }
    let picked = outcome["optionId"].as_str().unwrap_or_default();
    params["options"]
        .as_array()
        .and_then(|os| os.iter().find(|o| o["optionId"].as_str() == Some(picked)))
        .and_then(|o| o["kind"].as_str())
        .is_some_and(|k| k.contains("allow"))
}

#[cfg(test)]
mod mode_tests {
    use super::ask_first_mode;
    use serde_json::json;

    #[test]
    fn picks_standard_mode_unless_already_current() {
        let codex = json!({"currentModeId": "agent", "availableModes": [
            {"id": "read-only", "_meta": {"kind": "standard"}},
            {"id": "agent", "_meta": {"kind": "auto_review"}},
        ]});
        assert_eq!(ask_first_mode(Some(&codex)).as_deref(), Some("read-only"));
        let current = json!({"currentModeId": "read-only", "availableModes": [
            {"id": "read-only", "_meta": {"kind": "standard"}},
        ]});
        assert_eq!(ask_first_mode(Some(&current)), None);
        let claude = json!({"currentModeId": "default", "availableModes": [{"id": "default"}, {"id": "plan"}]});
        assert_eq!(ask_first_mode(Some(&claude)), None);
        assert_eq!(ask_first_mode(None), None);
    }
}
