#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Agent A13：桥接子进程模式。`<本程序> --agent-mcp-bridge <url>` 时
    // 只跑 stdin/stdout ↔ 本地 MCP HTTP 转发，不起窗口。
    // （Agent 进程以 stdio MCP 拉起本程序时命中；stdio 管道由父进程提供，
    // windows 子系统不影响被 spawn 时的管道可用性。）
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--agent-mcp-bridge") {
        let url = args.get(pos + 1).cloned().unwrap_or_default();
        let r = tauri::async_runtime::block_on(agent_host::mcp_bridge::run(url));
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    // Agent A29：内置 agent 子进程模式。`<本程序> --builtin-agent` 时只跑
    // stdin/stdout 上的 ACP 服务，不起窗口。
    if args.iter().any(|a| a == "--builtin-agent") {
        let r = tauri::async_runtime::block_on(agent_host::builtin::run_stdio());
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    chat_app_lib::run();
}
