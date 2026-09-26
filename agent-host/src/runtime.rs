// Agent 运行时抽象（ADR-001 §12：ACP 为主、原生兜底）。
//
// 宿主只认这里的类型：一次调用 = RunRequest 进、RunEvent 流出、RunOutput 结束。
// 新能力（上下文恢复、流式步骤、注入工具、附件、配置项）往这几个结构里加字段，
// 不改 run 的签名；某个 CLI 需要原生对接时，在 runtime_for 里按 spec 选实现。

use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

/// invocation_start / agent_probe 帧里 agent 字段的运行时描述。
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: Uuid,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// 空串 = 用宿主数据目录作为 cwd
    pub cwd: String,
    /// 在继承的环境上追加 / 覆盖
    pub env: Vec<(String, String)>,
    /// 非空时，只在新建会话的 prompt 里作为第一个文本块
    pub instructions: String,
    /// 空串 = 用 agent 默认
    pub model: String,
    pub mode: String,
    pub extra_dirs: Vec<String>,
    /// read_only / workspace / full。旧快照没有这个字段时按只读。
    pub permission_tier: String,
    /// 登记表形状。调用前由宿主解析成 command / args；自定义命令时为 None。
    pub registry: Option<RegistryPin>,
}

/// agents.runtime 里的 `{ "registry": { "id", "version" } }`。
#[derive(Debug, Clone)]
pub struct RegistryPin {
    pub id: String,
    pub version: String,
}

fn json_string_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn json_env(v: &Value) -> Vec<(String, String)> {
    v.get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn registry_pin(v: &Value) -> Option<RegistryPin> {
    let r = v.pointer("/runtime/registry")?;
    let id = r.get("id")?.as_str()?.trim();
    let version = r.get("version")?.as_str()?.trim();
    if id.is_empty() || version.is_empty() {
        return None;
    }
    Some(RegistryPin {
        id: id.to_string(),
        version: version.to_string(),
    })
}

impl AgentSpec {
    /// 缺省字段视为空：旧服务端的快照不含配置项时照常跑。
    /// 登记表形状没有 command，由宿主在调用时再解析。
    pub fn from_json(v: &Value) -> Option<Self> {
        let registry = registry_pin(v);
        let (command, args) = if registry.is_some() {
            (String::new(), Vec::new())
        } else {
            (
                v.pointer("/runtime/command")?.as_str()?.to_string(),
                v.pointer("/runtime/args")?
                    .as_array()?
                    .iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect(),
            )
        };
        Some(Self {
            id: Uuid::parse_str(v.get("id")?.as_str()?).ok()?,
            name: v.get("name")?.as_str()?.to_string(),
            command,
            args,
            cwd: v
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            env: json_env(v),
            instructions: v
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            model: v
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mode: v
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            extra_dirs: json_string_list(v, "extra_dirs"),
            registry,
            permission_tier: v
                .get("permission_tier")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("read_only")
                .to_string(),
        })
    }
}

/// 一次超档审批：宿主先登记等待槽，再把通知交给连接层发出去。
#[derive(Debug)]
pub struct ApprovalNotice {
    pub request_id: String,
    pub tool_call: Value,
    pub options: Value,
}

/// 进行中的审批等待。连接层收到 approval_decision 时 resolve；
/// 调用结束或断线时 finish / clear，避免 ACP 侧一直挂着。
#[derive(Debug, Default)]
pub struct ApprovalRegistry {
    pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl ApprovalRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 登记必须发生在把帧发给服务端之前，决定才不会落空。
    pub fn insert(&self, request_id: String, tx: oneshot::Sender<String>) {
        self.pending.lock().unwrap().insert(request_id, tx);
    }

    /// 主人选定的 optionId。没有对应等待（已取消 / 已结束）则忽略。
    pub fn resolve(&self, request_id: &str, option_id: String) -> bool {
        match self.pending.lock().unwrap().remove(request_id) {
            Some(tx) => tx.send(option_id).is_ok(),
            None => false,
        }
    }

    /// 调用方不再等了：摘掉发送端。已经被 resolve 取走时是空操作。
    pub fn finish(&self, request_id: &str) {
        self.pending.lock().unwrap().remove(request_id);
    }

    /// 连接断开：全部等待以 Err 醒来。
    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

/// 一次调用上的审批通道。没有它时，超档请求直接回 cancelled，不挂起。
#[derive(Clone, Debug)]
pub struct ApprovalLink {
    pub tx: mpsc::UnboundedSender<ApprovalNotice>,
    pub registry: Arc<ApprovalRegistry>,
}

/// 宿主上报给服务端的一个模型或模式选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOption {
    pub id: String,
    pub name: String,
}

/// Agent A13：注入给 Agent 的一个 MCP 服务（本次调用的工具入口）。
/// http_url 指向宿主本地 MCP 服务；bridge 是备选 stdio 桥接（程序 + 固定参数，
/// 调 ACP session/new 时把 http_url 追加为最后一个参数）。
#[derive(Debug, Clone)]
pub struct InjectedMcp {
    pub name: String,
    pub http_url: String,
    pub bridge: Option<(String, Vec<String>)>,
}

/// Agent A05：已下载到本地的输入附件（宿主提前落盘到 <data_dir>/inv-<id>/in/）。
#[derive(Debug, Clone)]
pub struct LocalFile {
    /// 本地绝对路径
    pub path: PathBuf,
    /// 展示名（已剥路径穿越，只剩 basename）
    pub name: String,
    pub mime: String,
}

/// 一次调用的输入。
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub spec: AgentSpec,
    pub prompt: String,
    /// spec.cwd 为空时的工作目录
    pub data_dir: PathBuf,
    /// 要续上的运行时会话 id。空则新开会话。
    pub resume_session: Option<String>,
    pub env: Vec<(String, String)>,
    pub instructions: String,
    pub model: String,
    pub mode: String,
    pub extra_dirs: Vec<String>,
    /// true：只做 initialize + session/new，不上报 prompt
    pub probe: bool,
    /// 主人取消：置 true 后 ACP 先发 session/cancel，最多再等 5 秒。
    pub cancel: watch::Receiver<bool>,
    /// Agent A13：本次调用注入的 MCP 服务（可空 = 不注入工具）
    pub mcp_servers: Vec<InjectedMcp>,
    /// 服务端下发的权限档位。空串按只读。
    pub permission_tier: String,
    /// 超档时向服务端要审批。None = 单测 / 无连接，超档直接取消。
    pub approvals: Option<ApprovalLink>,
    /// Agent A05：随调用交付的附件（已下载到 in/），排在指令文本块之前
    pub attachments: Vec<LocalFile>,
    /// Agent A05：产出文件目录；Some 时导出 CHAT_AGENT_OUTBOX 并追加提示文本块
    pub outbox: Option<PathBuf>,
}

impl RunRequest {
    /// 从快照拼一次普通调用（resume_session 由调用方按需再填）。probe 另走 `for_probe`。
    pub fn for_invocation(spec: AgentSpec, prompt: String, data_dir: PathBuf) -> Self {
        Self::build(spec, prompt, data_dir, false)
    }

    pub fn for_probe(spec: AgentSpec, data_dir: PathBuf) -> Self {
        Self::build(spec, String::new(), data_dir, true)
    }

    fn build(spec: AgentSpec, prompt: String, data_dir: PathBuf, probe: bool) -> Self {
        Self {
            resume_session: None,
            env: spec.env.clone(),
            instructions: spec.instructions.clone(),
            model: spec.model.clone(),
            mode: spec.mode.clone(),
            extra_dirs: spec.extra_dirs.clone(),
            permission_tier: spec.permission_tier.clone(),
            spec,
            prompt,
            data_dir,
            probe,
            // 默认没人会取消（发送端已丢弃）；普通调用由调用方换成真正的取消信号
            cancel: watch::channel(false).1,
            mcp_servers: Vec::new(),
            approvals: None,
            attachments: Vec::new(),
            outbox: None,
        }
    }
}

/// 运行过程中的事件（按发生顺序）。
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// agent 回复正文的一段增量
    Text(String),
    /// 一步工具调用开始（ACP tool_call）
    StepStart {
        id: String,
        title: String,
        kind: String,
    },
    /// 一步工具调用收尾；ok=false = ACP 报了非 success 终态（如 failed）
    StepEnd { id: String, ok: bool },
}

/// 一次调用的结果。
#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// 完整回复正文（= 全部 Text 事件拼接）
    pub text: String,
    /// 本次拒绝了几次 session/request_permission（本票一律拒绝，A12 才审批）
    pub permissions_rejected: u32,
    /// 本次实际使用的运行时会话 id（新开或恢复成功）
    pub session_id: Option<String>,
    /// session/new 带了 modes.availableModes 时为 Some（可以是空列表）
    pub modes: Option<Vec<AgentOption>>,
    /// session/new 带了 models.availableModels 时为 Some
    pub models: Option<Vec<AgentOption>>,
    /// 已完成的步骤（顺序保持）；单个失败不使整次调用失败
    pub steps: Vec<StepRecord>,
}

/// 一条已完成的步骤记录（随 RunOutput 走 invocation_finish 上报）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepRecord {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub ok: bool,
}

/// 事件出口；接收端丢弃时发送静默失败，运行不受影响。
pub type Events = mpsc::UnboundedSender<RunEvent>;

pub type RunFuture = Pin<Box<dyn Future<Output = Result<RunOutput, String>> + Send>>;

pub trait AgentRuntime: Send + Sync + 'static {
    /// Err 为人类可读原因（调用方负责截断）。
    fn run(&self, req: RunRequest, events: Events) -> RunFuture;
}

/// 按 spec 选运行时。目前只有 ACP；原生实现加在这里。
pub fn runtime_for(_spec: &AgentSpec) -> Arc<dyn AgentRuntime> {
    Arc::new(crate::acp::AcpRuntime)
}
