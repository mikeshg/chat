// Agent A01：桌面端宿主桥接（仅 desktop 编译；Android 不包含）。
//
// 宿主库（agent-host crate）在每次（重）连前向 TicketSource 要一张一次性 WS
// ticket。桌面端没有现成的凭据上下文——登录态在前端 localStorage，由前端 API
// 层负责刷新——所以这里把取票请求经事件 `agent-host://need-ticket` 转给前端，
// 前端调 GET /v1/ws/ticket 后用 agent_host_ticket 命令回填 oneshot。
//
// install_id 持久化在应用数据目录的 agent-install-id 文件里，首启生成后复用；
// 同一台电脑换用户登录在服务端是 (user_id, install_id) 不同的另一条记录。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;
use uuid::Uuid;

type Pending = Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<String, String>>>>>;

pub struct AgentHostState {
    handle: Mutex<Option<agent_host::HostHandle>>,
    pending: Pending,
}

impl AgentHostState {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// TicketSource 的桌面实现：发事件给前端，等 agent_host_ticket 命令回填。
struct FrontTickets {
    app: AppHandle,
    pending: Pending,
}

impl agent_host::TicketSource for FrontTickets {
    fn ticket(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        let app = self.app.clone();
        let pending = self.pending.clone();
        Box::pin(async move {
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(request_id, tx);
            if let Err(e) = app.emit(
                "agent-host://need-ticket",
                serde_json::json!({ "request_id": request_id }),
            ) {
                pending.lock().unwrap().remove(&request_id);
                return Err(format!("emit need-ticket: {e}"));
            }
            rx.await.map_err(|_| "前端未回填 ticket".to_string())?
        })
    }
}

fn install_id(app: &AppHandle) -> Result<Uuid, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create app data dir: {e}"))?;
    let file = dir.join("agent-install-id");
    if let Ok(text) = std::fs::read_to_string(&file) {
        if let Ok(id) = text.trim().parse::<Uuid>() {
            return Ok(id);
        }
    }
    let id = Uuid::new_v4();
    std::fs::write(&file, id.to_string()).map_err(|e| format!("write install id: {e}"))?;
    Ok(id)
}

/// 首次登记用的名称：机器名，拿不到用「桌面端」。与服务端 60 字上限对齐。
fn default_host_label() -> String {
    let name = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "桌面端".into());
    name.chars().take(60).collect()
}

/// 登录后由前端调用：启动（或重启）本机宿主。
/// `label` 为空时用机器名（拿不到用「桌面端」）。
/// 服务端只在第一次登记写入名称，之后以服务器为准，重连不再覆盖。
#[tauri::command]
pub async fn agent_host_start(
    app: AppHandle,
    state: State<'_, AgentHostState>,
    server_url: String,
    label: String,
) -> Result<(), String> {
    // 幂等：已在跑就先停掉旧宿主（换账号/换服务器都算重启）
    let old = state.handle.lock().unwrap().take();
    if let Some(old) = old {
        old.shutdown().await;
    }
    // Agent A13：stdio MCP 桥接 = 本程序 + --agent-mcp-bridge（main.rs 入口拦截）；
    // Agent 不声明 http MCP 能力时经它拿到注入工具。
    let mcp_bridge = std::env::current_exe().ok().map(|exe| {
        (
            exe.to_string_lossy().to_string(),
            vec!["--agent-mcp-bridge".to_string()],
        )
    });
    let given = label.trim();
    let label = if given.is_empty() {
        default_host_label()
    } else {
        given.chars().take(60).collect()
    };
    let cfg = agent_host::HostConfig {
        server_url,
        install_id: install_id(&app)?,
        label,
        platform: std::env::consts::OS.to_string(),
        mcp_bridge,
        registry_url: agent_host::DEFAULT_REGISTRY_URL.into(),
        data_dir: agent_host::default_data_dir(),
    };
    let tickets: Arc<dyn agent_host::TicketSource> = Arc::new(FrontTickets {
        app: app.clone(),
        pending: state.pending.clone(),
    });
    let handle = agent_host::spawn(cfg, tickets);
    *state.handle.lock().unwrap() = Some(handle);
    Ok(())
}

/// 取出正在跑的宿主。托盘退出用它，避免把锁留过 await。
pub(crate) fn take_handle(this: &AgentHostState) -> Option<agent_host::HostHandle> {
    this.handle.lock().unwrap().take()
}

/// 退出登录时由前端调用：停宿主（关 WS、杀在跑的 Agent 进程、不再重连）。
#[tauri::command]
pub async fn agent_host_stop(state: State<'_, AgentHostState>) -> Result<(), String> {
    if let Some(h) = take_handle(&state) {
        h.shutdown().await;
    }
    Ok(())
}

/// 前端回填取票结果：ticket 或 error 二选一。
#[tauri::command]
pub fn agent_host_ticket(
    state: State<'_, AgentHostState>,
    request_id: String,
    ticket: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    let id = request_id
        .parse::<Uuid>()
        .map_err(|_| "bad request_id".to_string())?;
    if let Some(tx) = state.pending.lock().unwrap().remove(&id) {
        let _ = tx.send(match ticket {
            Some(t) => Ok(t),
            None => Err(error.unwrap_or_else(|| "前端未提供 ticket".to_string())),
        });
    }
    Ok(())
}
