// 命令行宿主：把一台电脑注册成 AgentHost 并在线等调用。
//
//   cargo run --example host -- \
//       --server http://127.0.0.1:8780 --user alice --password secret \
//       [--install-id <uuid>] [--label "我的电脑"]
//
// install_id 是安装级永久标识：不给的话读/写 cwd 下的
// agent_host_install_id 文件；同一账号同一 id 复用同一 host 记录。
//
// Agent A13：`--mcp-stdio-bridge <url>` 时本进程退化成 stdio MCP 桥
// （供不声明 http MCP 能力的 Agent 用）；宿主把「本程序 + 该参数」
// 填进 HostConfig.mcp_bridge，作为注入工具的 stdio 兜底。

use agent_host::{spawn, HostConfig, TicketSource};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

struct Creds {
    server: String,
    username: String,
    password: String,
    access: Mutex<String>,
    refresh: Mutex<String>,
}

impl Creds {
    async fn login(&self) -> Result<(), String> {
        let v: Value = http()
            .post(format!("{}/v1/auth/login", self.server))
            .json(&json!({
                "username": self.username,
                "password": self.password,
                "device_label": "agent-host cli",
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let access = v["access_token"]
            .as_str()
            .ok_or("登录响应缺 access_token")?;
        let refresh = v["refresh_token"].as_str().unwrap_or_default();
        *self.access.lock().unwrap() = access.to_string();
        *self.refresh.lock().unwrap() = refresh.to_string();
        Ok(())
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn ticket_source(creds: Arc<Creds>) -> Arc<dyn TicketSource> {
    Arc::new(move || {
        let c = creds.clone();
        async move {
            for attempt in 0..2 {
                let access = c.access.lock().unwrap().clone();
                let res = http()
                    .get(format!("{}/v1/ws/ticket", c.server))
                    .bearer_auth(access)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if res.status().as_u16() == 401 && attempt == 0 {
                    // 凭据过期：刷新/重登后重试一次
                    c.login().await?;
                    continue;
                }
                let v: Value = res.json().await.map_err(|e| e.to_string())?;
                return v["ticket"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("ws/ticket 响应异常: {v}"));
            }
            Err("ws/ticket 重试耗尽".into())
        }
    })
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn install_id(arg: Option<String>) -> Uuid {
    if let Some(s) = arg {
        return Uuid::parse_str(&s).expect("--install-id 需为 uuid");
    }
    let path = PathBuf::from("agent_host_install_id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(id) = Uuid::parse_str(s.trim()) {
            return id;
        }
    }
    let id = Uuid::new_v4();
    let _ = std::fs::write(&path, id.to_string());
    id
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Agent A13：桥接子进程模式（Agent 以 stdio MCP 拉起本程序时命中）
    if args.first().map(String::as_str) == Some("--mcp-stdio-bridge") {
        let url = args.get(1).cloned().unwrap_or_default();
        if let Err(e) = agent_host::mcp_bridge::run(url).await {
            eprintln!("mcp-stdio-bridge: {e}");
            std::process::exit(1);
        }
        return;
    }
    tracing_subscriber::fmt::init();
    let get = |k: &str| arg_value(&args, k);
    let server = get("--server").expect("需要 --server");
    let username = get("--user").expect("需要 --user");
    let password = get("--password").expect("需要 --password");
    let install_id = install_id(get("--install-id"));
    // 不引额外依赖：Windows COMPUTERNAME / Unix HOSTNAME，拿不到用「桌面端」
    let hostname = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "桌面端".into());
    let label = get("--label").unwrap_or(hostname);
    let platform = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    };

    let creds = Arc::new(Creds {
        server: server.clone(),
        username,
        password,
        access: Mutex::new(String::new()),
        refresh: Mutex::new(String::new()),
    });
    creds.login().await.expect("登录失败");

    // Agent A13：stdio 桥接 = 本程序 + --mcp-stdio-bridge（URL 由运行时追加）
    let mcp_bridge = std::env::current_exe().ok().map(|exe| {
        (
            exe.to_string_lossy().to_string(),
            vec!["--mcp-stdio-bridge".into()],
        )
    });
    // Agent A29：内置 agent = 同批编译的 chat-builtin-agent 二进制。
    // examples 编到 target/<profile>/examples/ 下，二进制在上一级目录。
    let builtin_agent = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_path_buf()))
        .and_then(|dir| dir.parent().map(|d| d.to_path_buf()))
        .map(|dir| {
            dir.join(if cfg!(windows) {
                "chat-builtin-agent.exe"
            } else {
                "chat-builtin-agent"
            })
        })
        .filter(|p| p.exists())
        .map(|p| (p.to_string_lossy().to_string(), Vec::new()));
    let handle = spawn(
        HostConfig {
            server_url: server,
            install_id,
            label,
            platform: platform.into(),
            mcp_bridge,
            builtin_agent,
            registry_url: get("--registry-url")
                .unwrap_or_else(|| agent_host::DEFAULT_REGISTRY_URL.into()),
            data_dir: get("--data-dir")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(agent_host::default_data_dir),
        },
        ticket_source(creds),
    );
    if let Some(id) = handle.wait_host_id().await {
        println!("agent-host online, host_id={id}");
    } else {
        eprintln!("宿主连接未建立");
    }
    tokio::signal::ctrl_c().await.ok();
    handle.shutdown().await;
}
