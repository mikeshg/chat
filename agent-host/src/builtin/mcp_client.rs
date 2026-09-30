// Chat 注入的 http MCP 服务 → 底座 Tool。
//
// 协议走 Streamable HTTP 的 JSON-RPC over POST（不做 SSE 订阅）：
//   initialize → notifications/initialized → tools/list
// 每个工具包成 "<服务名>__<工具名>"，参数 schema 原样透传，调用走 tools/call。
// 单个服务连不上只记 stderr 跳过，不拖垮会话。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent::agent::{Tool, ToolContext, ToolError, ToolOutput};
use agent::llm::ToolSpec;
use serde_json::{json, Value};

/// 单个 MCP 请求的超时。
const MCP_TIMEOUT: Duration = Duration::from_secs(15);

/// 对 session/new / session/load 参数里的 mcpServers 数组建连接，
/// 返回全部可用工具。
pub(crate) async fn connect_all(servers: Option<&Vec<Value>>) -> Vec<Arc<dyn Tool>> {
    let mut out = Vec::new();
    let Some(servers) = servers else {
        return out;
    };
    for server in servers {
        if server.get("type").and_then(Value::as_str) != Some("http") {
            continue;
        }
        let name = server
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("mcp")
            .to_string();
        match connect_one(server).await {
            Ok(mut tools) => out.append(&mut tools),
            Err(e) => eprintln!("builtin: MCP 服务 {name} 不可用，已跳过: {e}"),
        }
    }
    out
}

async fn connect_one(server: &Value) -> Result<Vec<Arc<dyn Tool>>, String> {
    let name = server["name"]
        .as_str()
        .ok_or("mcpServers 条目缺 name")?
        .to_string();
    let url = server["url"]
        .as_str()
        .ok_or("mcpServers 条目缺 url")?
        .to_string();
    let headers: Vec<(String, String)> = server
        .get("headers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|h| {
                    Some((
                        h["name"].as_str()?.to_string(),
                        h["value"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(MCP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    rpc_call(&http, &url, &headers, 1, "initialize", init_params()).await?;
    // initialized 通知：发出即可，不等结果
    let _ = post(
        &http,
        &url,
        &headers,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    let list = rpc_call(&http, &url, &headers, 2, "tools/list", json!({})).await?;
    let tools = list
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let next_id = Arc::new(AtomicU64::new(3));
    Ok(tools
        .into_iter()
        .filter_map(|t| {
            let remote = t.get("name")?.as_str()?.to_string();
            let spec = ToolSpec {
                name: format!("{name}__{remote}"),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"})),
            };
            Some(Arc::new(McpTool {
                http: http.clone(),
                url: url.clone(),
                headers: headers.clone(),
                remote_name: remote,
                spec,
                next_id: next_id.clone(),
            }) as Arc<dyn Tool>)
        })
        .collect())
}

fn init_params() -> Value {
    json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {"name": "chat-builtin-agent", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// POST 一个 JSON-RPC 请求/通知；返回响应 JSON。Accept 带 event-stream
/// 是协议要求；真收到 SSE 体时取最后一个 data: 负载当响应。
async fn post(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<Value, String> {
    let mut req = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(body);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        return Ok(v);
    }
    // SSE 响应：取最后一条 data: 负载
    if let Some(v) = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .last()
    {
        return Ok(v);
    }
    Err("响应不是 JSON".into())
}

async fn rpc_call(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let v = post(http, url, headers, &body).await?;
    if let Some(e) = v.get("error") {
        return Err(e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP 调用失败")
            .to_string());
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// 一个注入 MCP 工具：tools/call 调用远端，文本结果拼回给模型。
struct McpTool {
    http: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    remote_name: String,
    spec: ToolSpec,
    next_id: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let result = rpc_call(
            &self.http,
            &self.url,
            &self.headers,
            id,
            "tools/call",
            json!({"name": self.remote_name, "arguments": args}),
        )
        .await
        .map_err(ToolError::new)?;
        // content[] 里的 text 块拼成结果文本；其余类型原样转 JSON
        let mut parts = Vec::new();
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                    _ => parts.push(block.to_string()),
                }
            }
        }
        if parts.is_empty() {
            if let Some(sc) = result.get("structuredContent") {
                parts.push(sc.to_string());
            }
        }
        let content = parts.join("\n");
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(ToolError::new(if content.is_empty() {
                "MCP 工具返回错误".to_string()
            } else {
                content
            }));
        }
        Ok(ToolOutput {
            content,
            ..ToolOutput::default()
        })
    }
}
