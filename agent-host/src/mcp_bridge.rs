// Agent A13：stdio ↔ 本地 MCP HTTP 桥。
//
// 给不能吃 HTTP MCP 的 ACP Agent 兜底：Agent 把桥接进程当 stdio MCP
// 服务拉起（command=桥程序，args 尾部是 MCP URL）；桥把 stdin 上的
// 每行 JSON-RPC 消息 POST 到该 URL，有响应体就写回 stdout 一行。
//
// stdout 是协议通道：任何日志只能走 stderr，绝不能 println!。
// URL 里没有凭据；串行转发（一次一个请求）对工具调用足够。

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 跑桥接循环直到 stdin 关闭。url 形如 http://127.0.0.1:PORT/mcp/<id>。
pub async fn run(url: String) -> Result<(), String> {
    if url.is_empty() {
        return Err("用法: agent-mcp-bridge <mcp-url>".into());
    }
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => return Ok(()), // stdin 关闭/读失败 → 结束
        };
        if line.trim().is_empty() {
            continue;
        }
        match http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(line.clone())
            .send()
            .await
        {
            Ok(r) => {
                // 200=JSON-RPC 响应，202=通知无体，404=调用已结束（上游静默即可）
                let text = r.text().await.unwrap_or_default();
                if !text.trim().is_empty() {
                    stdout
                        .write_all(text.trim().as_bytes())
                        .await
                        .and_then(|_| Ok(()))
                        .map_err(|e| e.to_string())?;
                    stdout.write_all(b"\n").await.map_err(|e| e.to_string())?;
                    stdout.flush().await.map_err(|e| e.to_string())?;
                }
            }
            Err(e) => {
                // 网络层失败：若入站消息带 id，回一个 JSON-RPC error，
                // 免得 Agent 在 tools/call 上死等
                let id = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").cloned());
                if let Some(id) = id {
                    let resp = json!({"jsonrpc":"2.0","id":id,
                        "error":{"code":-32603,"message":format!("bridge http: {e}")}});
                    stdout
                        .write_all(resp.to_string().as_bytes())
                        .await
                        .map_err(|e| e.to_string())?;
                    stdout.write_all(b"\n").await.map_err(|e| e.to_string())?;
                    stdout.flush().await.map_err(|e| e.to_string())?;
                }
            }
        }
    }
}
