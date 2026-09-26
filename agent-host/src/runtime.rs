// Agent 运行时抽象（ADR-001 §12：ACP 为主、原生兜底）。
//
// 宿主只认这里的类型：一次调用 = RunRequest 进、RunEvent 流出、RunOutput 结束。
// 新能力（上下文恢复、流式步骤、注入工具、附件、配置项）往这几个结构里加字段，
// 不改 run 的签名；某个 CLI 需要原生对接时，在 runtime_for 里按 spec 选实现。

use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
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

impl AgentSpec {
    /// 缺省字段视为空：旧服务端的快照不含配置项时照常跑。
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
        })
    }
}

/// 宿主上报给服务端的一个模型或模式选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOption {
    pub id: String,
    pub name: String,
}

/// 一次调用的输入。配置项与 AgentSpec 同步一份，运行时读这里。
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub spec: AgentSpec,
    pub prompt: String,
    /// spec.cwd 为空时的工作目录
    pub data_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub instructions: String,
    pub model: String,
    pub mode: String,
    pub extra_dirs: Vec<String>,
    /// true：只做 initialize + session/new，不上报 prompt
    pub probe: bool,
}

impl RunRequest {
    /// 从快照拼一次普通调用。probe 另走 `for_probe`。
    pub fn for_invocation(spec: AgentSpec, prompt: String, data_dir: PathBuf) -> Self {
        Self::build(spec, prompt, data_dir, false)
    }

    pub fn for_probe(spec: AgentSpec, data_dir: PathBuf) -> Self {
        Self::build(spec, String::new(), data_dir, true)
    }

    fn build(spec: AgentSpec, prompt: String, data_dir: PathBuf, probe: bool) -> Self {
        Self {
            env: spec.env.clone(),
            instructions: spec.instructions.clone(),
            model: spec.model.clone(),
            mode: spec.mode.clone(),
            extra_dirs: spec.extra_dirs.clone(),
            spec,
            prompt,
            data_dir,
            probe,
        }
    }
}

/// 运行过程中的事件（按发生顺序）。
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// agent 回复正文的一段增量
    Text(String),
}

/// 一次调用的结果。
#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// 完整回复正文（= 全部 Text 事件拼接）
    pub text: String,
    /// session/new 带了 modes.availableModes 时为 Some（可以是空列表）
    pub modes: Option<Vec<AgentOption>>,
    /// session/new 带了 models.availableModels 时为 Some
    pub models: Option<Vec<AgentOption>>,
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
