// AgentHost：跑在桌面端的宿主进程。用账号凭据向服务器换 WS 票，
// 以 role=host 建立长连，接收 invocation_start，在本地以 ACP 拉起
// Agent 进程执行，回传 invocation_finish。
//
// 不支持 GUI：纯库 + CLI 示例；Tauri 壳在 app/src-tauri 侧桥接。

pub mod acp;
pub mod command;
mod conn;
pub mod mcp;
pub mod mcp_bridge;
pub mod registry;
pub mod runtime;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// 宿主连接配置。
pub struct HostConfig {
    /// 服务器地址：http(s)://… 或 ws(s)://…（自动换算成 ws 协议）
    pub server_url: String,
    /// 本机永久安装标识；同一账号同一 install_id 始终映射同一 host 记录
    pub install_id: Uuid,
    /// 展示名（机器名等），最长 60 字符
    pub label: String,
    /// windows / macos / linux
    pub platform: String,
    /// Agent A13：stdio MCP 桥接（程序 + 固定参数；MCP URL 追加为最后一个参数）。
    /// Agent 不支持 http MCP 时用；None = 无桥接兜底（此时不声明 http 能力的
    /// Agent 拿不到注入工具）。
    pub mcp_bridge: Option<(String, Vec<String>)>,
    /// ACP 登记表索引。测试指向本地假登记表。
    pub registry_url: String,
    /// 登记表缓存和安装目录。空 cwd 的 Agent 也用这里当工作目录。
    pub data_dir: std::path::PathBuf,
}

/// 官方登记表索引。
pub const DEFAULT_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// 还没指定数据目录时的默认位置（A09 再换成持久目录）。
pub fn default_data_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("chat-agent-host")
}

/// 每条宿主连接建立前取一次一次性 WS 票（内部实现用账号登录态换
/// GET /v1/ws/ticket）。实现负责处理凭据过期后的再登录。
pub trait TicketSource: Send + Sync + 'static {
    fn ticket(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
}

// 让普通闭包/异步函数直接可作为 TicketSource
impl<F, Fut> TicketSource for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    fn ticket(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        Box::pin((self)())
    }
}

/// 调用中子进程登记表：宿主关闭时需要把还在跑的 Agent 进程杀掉。
pub(crate) type Running = Arc<Mutex<HashMap<Uuid, JoinHandle<()>>>>;

/// 运行中调用的取消信号（invocation_cancel → 置 true，ACP 侧收）
pub(crate) type Cancels = Arc<Mutex<HashMap<Uuid, watch::Sender<bool>>>>;

pub(crate) struct Shared {
    pub cfg: HostConfig,
    pub tickets: Arc<dyn TicketSource>,
    /// 命中的 host_id（服务端在 host_ready 里确认）
    pub host_id: watch::Sender<Option<Uuid>>,
    /// 运行中的 invocation 任务（invocation_id -> JoinHandle）
    pub running: Running,
    /// 运行中调用的取消入口（不 abort 任务，交给 ACP 发 session/cancel）
    pub cancels: Cancels,
    /// 收到 host_superseded 后置真：同一 (user,install) 被更新的连接顶替，不再重连
    pub superseded: std::sync::atomic::AtomicBool,
    /// cwd 为空时 Agent 进程的默认工作目录
    pub data_dir: std::path::PathBuf,
    /// Agent A13：本地 MCP 服务；起不来的话本次运行不注入工具（仅告警）
    pub mcp: Option<mcp::McpServer>,
}

pub struct HostHandle {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
    host_id_rx: watch::Receiver<Option<Uuid>>,
}

impl HostHandle {
    /// 服务端分配的 host id；连接建立且收到 host_ready 后才可得
    pub fn host_id(&self) -> Option<Uuid> {
        *self.host_id_rx.borrow()
    }

    /// 等到第一次拿到 host_id；连接被顶替/关闭返回 None
    pub async fn wait_host_id(&self) -> Option<Uuid> {
        let mut rx = self.host_id_rx.clone();
        loop {
            if let Some(id) = *rx.borrow() {
                return Some(id);
            }
            if self
                .shared
                .superseded
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return None;
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    /// 主动停宿主：关 WS、杀所有在跑的 Agent 进程、结束重连循环
    pub async fn shutdown(self) {
        self.task.abort();
        {
            let mut running = self.shared.running.lock().unwrap();
            for (_, h) in running.drain() {
                h.abort();
            }
        } // MutexGuard 先释放，保证 future 为 Send
        let _ = self.task.await;
    }
}

/// 以后台任务启动宿主：拿到票→连 WS→host_hello→host_ready→
/// 循环收 invocation_start；断线指数退避重连（封顶 30s）。
pub fn spawn(cfg: HostConfig, tickets: Arc<dyn TicketSource>) -> HostHandle {
    let (host_id_tx, host_id_rx) = watch::channel::<Option<Uuid>>(None);
    let data_dir = cfg.data_dir.clone();
    let shared = Arc::new(Shared {
        data_dir,
        cfg,
        tickets,
        host_id: host_id_tx,
        running: Arc::new(Mutex::new(HashMap::new())),
        cancels: Arc::new(Mutex::new(HashMap::new())),
        superseded: std::sync::atomic::AtomicBool::new(false),
        mcp: mcp::McpServer::start(),
    });
    let _ = std::fs::create_dir_all(&shared.data_dir);
    let s2 = shared.clone();
    let task = tokio::spawn(async move { conn::run(s2).await });
    HostHandle {
        shared,
        task,
        host_id_rx,
    }
}
