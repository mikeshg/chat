// Agent A13：宿主本地 MCP 服务（Streamable HTTP 的 JSON-RPC 子集）。
//
// 只绑 127.0.0.1 随机端口；每次调用分配随机路径 id（/mcp/<id>），
// 映射「服务端地址 + tools_token」——凭据不出现在 URL 里，也不进 Agent 环境变量。
// McpHandle 随调用任务存活；任务结束/被 abort（取消）时 drop → 摘除映射，
// 此后该路径一律 404。
//
// 方法面：initialize / notifications/initialized / tools/list / tools/call。
// 工具定义在 TOOLS 表里，后续任务加工具只需追加条目。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

/// 一次调用的工具绑定：上游服务端地址 + 调用凭据（只在内存里，不出现在 URL）
#[derive(Clone)]
struct Binding {
    server: String,
    token: String,
}

type Routes = Arc<Mutex<HashMap<String, Binding>>>;

#[derive(Clone)]
struct McpState {
    routes: Routes,
    http: reqwest::Client,
}

pub struct McpServer {
    port: u16,
    routes: Routes,
    _task: JoinHandle<()>,
}

/// 一条调用级映射；drop（含任务 abort 时 future 被销毁）即摘除
pub struct McpHandle {
    /// 完整 MCP 入口 URL，例 http://127.0.0.1:PORT/mcp/<id>
    pub url: String,
    id: String,
    routes: Routes,
}

impl Drop for McpHandle {
    fn drop(&mut self) {
        self.routes.lock().unwrap().remove(&self.id);
    }
}

impl McpServer {
    /// 起本地服务（127.0.0.1 随机端口）。需在 tokio 运行时内调用；
    /// 失败返回 None——宿主照常工作，只是不注入工具。
    pub fn start() -> Option<Self> {
        // 同步 bind 再转 tokio listener，保持 start() 同步签名（spawn 路径方便）
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        std_listener.set_nonblocking(true).ok()?;
        let port = std_listener.local_addr().ok()?.port();
        let listener = tokio::net::TcpListener::from_std(std_listener).ok()?;
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let state = McpState {
            routes: routes.clone(),
            http: reqwest::Client::builder().no_proxy().build().ok()?,
        };
        let app = Router::new()
            .route("/mcp/{id}", post(mcp_post))
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tracing::info!(port, "本地 MCP 服务已监听");
        Some(Self {
            port,
            routes,
            _task: task,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 为一次调用登记映射，返回随任务存活的 handle（drop = 摘映射）。
    /// server 是 http(s) 基址（conn 侧已把 ws(s) 换算好）。
    pub fn register(&self, server: String, token: String) -> McpHandle {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.routes
            .lock()
            .unwrap()
            .insert(id.clone(), Binding { server, token });
        McpHandle {
            url: format!("http://127.0.0.1:{}/mcp/{id}", self.port),
            id,
            routes: self.routes.clone(),
        }
    }
}

// ---------- 工具表：新工具在这里加条目 ----------

type ToolFut<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

struct ToolDef {
    name: &'static str,
    description: &'static str,
    /// inputSchema 生成器（Value 非常量，用 fn 指针进静态表）
    schema: fn() -> Value,
    /// 执行体：返回给 Agent 读的可读文本
    call: for<'a> fn(&'a McpState, &'a Binding, &'a Value) -> ToolFut<'a>,
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "read_conversation_history",
        description: "读取发起本次调用的会话的最近消息。返回最新的若干条（seq 升序）；\
                  要更早的历史传 before_seq（某条 seq 之前的消息），limit 控制条数（默认 50，最大 200）。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "before_seq": {"type": "integer", "description": "只返回 seq 小于该值的消息"},
                    "limit": {"type": "integer", "description": "最多返回多少条，默认 50，上限 200"}
                }
            })
        },
        call: |st, b, args| Box::pin(read_history(st, b, args)),
    },
    ToolDef {
        name: "create_schedule",
        description: "给当前 Agent 建一条定时任务。到点后用 prompt 调用它，结果发到目标会话。\
                      不传 target_conversation_id 时发到发起本次调用的会话。\
                      spec 是 {type:once,at} / {type:daily,time} / {type:weekly,days,time} / {type:interval,minutes}，\
                      timezone 是 IANA 名（如 Asia/Shanghai）。minutes 至少 5。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "1–40 字"},
                    "prompt": {"type": "string", "description": "到点执行的指令，1–4000 字"},
                    "spec": {"type": "object", "description": "时间表"},
                    "timezone": {"type": "string", "description": "IANA 时区"},
                    "target_conversation_id": {"type": "string", "description": "结果发到的会话，缺省为当前会话"}
                },
                "required": ["name", "prompt", "spec", "timezone"]
            })
        },
        call: |st, b, args| Box::pin(create_schedule(st, b, args)),
    },
    ToolDef {
        name: "list_schedules",
        description: "列出当前 Agent 的定时任务（名称、是否启用、创建者、下次运行时间）。",
        schema: || json!({"type": "object", "properties": {}}),
        call: |st, b, args| Box::pin(list_schedules(st, b, args)),
    },
    ToolDef {
        name: "delete_schedule",
        description: "删除当前 Agent 的一条定时任务。参数 id 来自 list_schedules。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "定时任务 id"}
                },
                "required": ["id"]
            })
        },
        call: |st, b, args| Box::pin(delete_schedule(st, b, args)),
    },
];

/// GET /v1/agent-tools/history 的结果 → `[seq 12] 张三: …` 行
async fn read_history(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let mut url = format!("{}/v1/agent-tools/history", b.server);
    let mut qs = Vec::new();
    if let Some(v) = args.get("before_seq").and_then(Value::as_i64) {
        qs.push(format!("before_seq={v}"));
    }
    if let Some(v) = args.get("limit").and_then(Value::as_i64) {
        qs.push(format!("limit={v}"));
    }
    if !qs.is_empty() {
        url.push('?');
        url.push_str(&qs.join("&"));
    }
    let r = st
        .http
        .get(&url)
        .header("authorization", format!("Invocation {}", b.token))
        .send()
        .await
        .map_err(|e| format!("history 请求失败: {e}"))?;
    let status = r.status();
    let body: Value = r
        .json()
        .await
        .map_err(|e| format!("history 响应解析失败: {e}"))?;
    if !status.is_success() {
        let msg = body["error"].as_str().unwrap_or("?");
        return Err(format!("history {}: {msg}", status.as_u16()));
    }
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    if msgs.is_empty() {
        return Ok("（无历史消息）".into());
    }
    let mut out = String::new();
    for m in &msgs {
        let mut sender = m["sender_name"].as_str().unwrap_or("?").to_string();
        if m["is_agent"] == true {
            sender.push_str(" (Agent)");
        }
        out.push_str(&format!(
            "[seq {}] {sender}: {}",
            m["seq"].as_i64().unwrap_or_default(),
            m["content"].as_str().unwrap_or_default()
        ));
        if let Some(a) = m["attachment"].as_str() {
            out.push_str(&format!(" [附件: {a}]"));
        }
        out.push('\n');
    }
    if body["has_more"] == true {
        out.push_str("（还有更早的消息：用 before_seq 继续往前翻）\n");
    }
    Ok(out)
}

async fn call_server(
    st: &McpState,
    b: &Binding,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{}{path}", b.server);
    let req = match method {
        "GET" => st.http.get(&url),
        "POST" => st.http.post(&url).json(body.unwrap_or(&Value::Null)),
        "DELETE" => st.http.delete(&url),
        _ => return Err(format!("不支持的方法 {method}")),
    };
    let r = req
        .header("authorization", format!("Invocation {}", b.token))
        .send()
        .await
        .map_err(|e| format!("schedules 请求失败: {e}"))?;
    let status = r.status();
    let text = r
        .text()
        .await
        .map_err(|e| format!("schedules 响应读取失败: {e}"))?;
    if status.as_u16() == 204 || text.trim().is_empty() {
        if status.is_success() {
            return Ok(Value::Null);
        }
    }
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let msg =
            parsed["error"]
                .as_str()
                .unwrap_or(if text.is_empty() { "?" } else { text.as_str() });
        return Err(format!("schedules {}: {msg}", status.as_u16()));
    }
    Ok(parsed)
}

async fn create_schedule(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let mut body = json!({
        "name": args.get("name").cloned().unwrap_or(Value::Null),
        "prompt": args.get("prompt").cloned().unwrap_or(Value::Null),
        "spec": args.get("spec").cloned().unwrap_or(Value::Null),
        "timezone": args.get("timezone").cloned().unwrap_or(Value::Null),
    });
    if let Some(t) = args.get("target_conversation_id").filter(|v| v.is_string()) {
        body["target_conversation_id"] = t.clone();
    }
    let v = call_server(st, b, "POST", "/v1/agent-tools/schedules", Some(&body)).await?;
    Ok(format!(
        "已创建 id={} name={} created_by={}",
        v["id"].as_str().unwrap_or(""),
        v["name"].as_str().unwrap_or(""),
        v["created_by"].as_str().unwrap_or("")
    ))
}

async fn list_schedules(st: &McpState, b: &Binding, _args: &Value) -> Result<String, String> {
    let v = call_server(st, b, "GET", "/v1/agent-tools/schedules", None).await?;
    let Some(rows) = v.as_array() else {
        return Err("schedules 响应不是列表".into());
    };
    if rows.is_empty() {
        return Ok("（没有定时任务）".into());
    }
    let mut out = String::new();
    for s in rows {
        out.push_str(&format!(
            "id={} name={} created_by={} enabled={} next={}\n",
            s["id"].as_str().unwrap_or(""),
            s["name"].as_str().unwrap_or(""),
            s["created_by"].as_str().unwrap_or(""),
            s["enabled"],
            s["next_run_at"]
        ));
    }
    Ok(out)
}

async fn delete_schedule(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let id = args["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("缺少 id")?;
    call_server(
        st,
        b,
        "DELETE",
        &format!("/v1/agent-tools/schedules/{id}"),
        None,
    )
    .await?;
    Ok("已删除".into())
}

// ---------- JSON-RPC 分发 ----------

fn rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
        .into_response()
}

fn rpc_err(id: Value, code: i64, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response()
}

async fn call_tool(st: &McpState, b: &Binding, params: &Value) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().unwrap_or_default();
    let args = &params["arguments"];
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return Err((-32602, format!("unknown tool {name}")));
    };
    // 工具执行失败按 MCP 约定走 isError 结果，而不是 JSON-RPC 层 error
    match (tool.call)(st, b, args).await {
        Ok(text) => Ok(json!({"content":[{"type":"text","text":text}]})),
        Err(e) => Ok(json!({"content":[{"type":"text","text":e}],"isError":true})),
    }
}

async fn mcp_post(State(st): State<McpState>, Path(id): Path<String>, body: Bytes) -> Response {
    // 锁内只做哈希查找，不跨 await
    let binding = { st.routes.lock().unwrap().get(&id).cloned() };
    let Some(binding) = binding else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_err(Value::Null, -32700, "parse error");
    };
    // 通知（无 id，如 notifications/initialized）：收下即 202，无响应体
    let Some(idv) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "initialize" => rpc_result(
            idv,
            json!({
                "protocolVersion": msg
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-03-26"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "chat-tools", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "tools/list" => rpc_result(
            idv,
            json!({"tools": TOOLS.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            })).collect::<Vec<_>>()}),
        ),
        "tools/call" => match call_tool(&st, &binding, &msg["params"]).await {
            Ok(v) => rpc_result(idv, v),
            Err((code, m)) => rpc_err(idv, code, &m),
        },
        _ => rpc_err(idv, -32601, "method not found"),
    }
}
