// Agent A13：stdio MCP 桥的可执行入口。
//   agent-mcp-bridge <mcp-url>
// stdin 上的 JSON-RPC 行 → POST 到 <mcp-url> → 响应写回 stdout。

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_default();
    if let Err(e) = agent_host::mcp_bridge::run(url).await {
        eprintln!("agent-mcp-bridge: {e}");
        std::process::exit(1);
    }
}
