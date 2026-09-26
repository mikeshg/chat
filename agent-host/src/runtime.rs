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

/// 一次调用的输入。
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub spec: AgentSpec,
    pub prompt: String,
    /// spec.cwd 为空时的工作目录
    pub data_dir: PathBuf,
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
