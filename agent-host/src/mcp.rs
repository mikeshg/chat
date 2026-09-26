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

/// 一次调用的工具绑定：上游服务端地址 + 调用凭据（只在内存里，不出现在 URL）。
/// A16：档位和审批通道一起登记，send_message 在只读档要能向主人要批准。
#[derive(Clone)]
struct Binding {
    server: String,
    token: String,
    permission_tier: String,
    approvals: Option<crate::runtime::ApprovalLink>,
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
    /// 档位缺省按只读，且没有审批通道——只给不发消息的测试用。
    pub fn register(&self, server: String, token: String) -> McpHandle {
        self.register_with(server, token, "read_only".into(), None)
    }

    /// 把这次调用的权限档位和审批通道一起登记。send_message 用它们决定要不要先问主人。
    pub fn register_with(
        &self,
        server: String,
        token: String,
        permission_tier: String,
        approvals: Option<crate::runtime::ApprovalLink>,
    ) -> McpHandle {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.routes.lock().unwrap().insert(
            id.clone(),
            Binding {
                server,
                token,
                permission_tier,
                approvals,
            },
        );
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
        name: "list_conversations",
        description: "列出主人参与的会话（群、私聊、文件传输助手、Agent 专属会话）。\
                      只能看到主人自己参与的会话，看不到主人没加入的群。\
                      每行一个：id、kind、title、members、last_seq、readonly。按最近活跃排序，最多 200 个。",
        schema: || json!({"type": "object", "properties": {}}),
        call: |st, b, args| Box::pin(list_conversations(st, b, args)),
    },
    ToolDef {
        name: "search_messages",
        description: "在主人参与的会话里搜索消息。看不到主人没参与的会话里的内容。\
                      query 是关键词；limit 可选（默认 30，最大 100）。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "搜索关键词"},
                    "limit": {"type": "integer", "description": "最多返回多少条，默认 30，上限 100"}
                },
                "required": ["query"]
            })
        },
        call: |st, b, args| Box::pin(search_messages(st, b, args)),
    },
    ToolDef {
        name: "read_conversation",
        description: "读取主人参与的某个会话的历史。conversation_id 来自 list_conversations。\
                      主人不是该会话成员时会失败。before_seq / limit 与读取当前会话历史相同（默认 50，最大 200）。\
                      只能读主人参与的会话。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "conversation_id": {"type": "string", "description": "会话 id"},
                    "before_seq": {"type": "integer", "description": "只返回 seq 小于该值的消息"},
                    "limit": {"type": "integer", "description": "最多返回多少条，默认 50，上限 200"}
                },
                "required": ["conversation_id"]
            })
        },
        call: |st, b, args| Box::pin(read_conversation(st, b, args)),
    },
    ToolDef {
        name: "send_message",
        description: "以当前 Agent 的身份往主人参与的某个会话发消息，群成员会看到发送者是这个 Agent。\
                      只能发到主人参与的会话，只读私聊会被拒绝。\
                      只读档位会先请主人批准；可改工作目录和完全放开直接发送。\
                      file_paths 是宿主本机文件路径，会先上传再随消息发出。",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "conversation_id": {"type": "string", "description": "目标会话 id"},
                    "text": {"type": "string", "description": "正文；只发文件时可以是空字符串"},
                    "file_paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "宿主本地文件路径，先上传再发送"
                    }
                },
                "required": ["conversation_id", "text"]
            })
        },
        call: |st, b, args| Box::pin(send_message(st, b, args)),
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
    Ok(format_history_body(&body))
}

/// GET /v1/agent-tools/history 或某个会话历史的结果 → `[seq 12] 张三: …` 行
fn format_history_body(body: &Value) -> String {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    if msgs.is_empty() {
        return "（无历史消息）".into();
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
    out
}

fn sends_directly(tier: &str) -> bool {
    matches!(tier, "workspace" | "full")
}

fn preview80(text: &str) -> String {
    text.chars().take(80).collect()
}

fn option_allowed(options: &Value, option_id: &str) -> bool {
    let kind = options
        .as_array()
        .and_then(|a| a.iter().find(|o| o["optionId"].as_str() == Some(option_id)))
        .and_then(|o| o["kind"].as_str())
        .unwrap_or("");
    kind.contains("allow") && !kind.contains("reject")
}

/// 审批等待结束（成功、拒绝、通道断开）都把登记摘掉，避免连接侧一直留着空槽。
struct ApprovalGuard {
    registry: std::sync::Arc<crate::runtime::ApprovalRegistry>,
    request_id: String,
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        self.registry.finish(&self.request_id);
    }
}

async fn tool_http(
    st: &McpState,
    b: &Binding,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    body: Option<&Value>,
    what: &str,
) -> Result<Value, String> {
    let url = format!("{}{path}", b.server);
    let req = match method {
        "GET" => st.http.get(&url).query(query),
        "POST" => st
            .http
            .post(&url)
            .query(query)
            .json(body.unwrap_or(&Value::Null)),
        "DELETE" => st.http.delete(&url).query(query),
        _ => return Err(format!("不支持的方法 {method}")),
    };
    let r = req
        .header("authorization", format!("Invocation {}", b.token))
        .send()
        .await
        .map_err(|e| format!("{what} 请求失败: {e}"))?;
    let status = r.status();
    let text = r
        .text()
        .await
        .map_err(|e| format!("{what} 响应读取失败: {e}"))?;
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
        return Err(format!("{what} {}: {msg}", status.as_u16()));
    }
    Ok(parsed)
}

fn format_convs(v: &Value) -> Result<String, String> {
    let rows = v.as_array().ok_or("conversations 响应不是列表")?;
    if rows.is_empty() {
        return Ok("（没有会话）".into());
    }
    let mut out = String::new();
    for c in rows {
        out.push_str(&format!(
            "id={} kind={} title={} members={} last_seq={} readonly={}\n",
            c["id"].as_str().unwrap_or(""),
            c["kind"].as_str().unwrap_or(""),
            c["title"].as_str().unwrap_or(""),
            c["member_count"].as_i64().unwrap_or(0),
            c["last_seq"].as_i64().unwrap_or(0),
            c["readonly"].as_bool().unwrap_or(false),
        ));
    }
    Ok(out)
}

async fn list_conversations(st: &McpState, b: &Binding, _args: &Value) -> Result<String, String> {
    let v = tool_http(
        st,
        b,
        "GET",
        "/v1/agent-tools/conversations",
        &[],
        None,
        "list",
    )
    .await?;
    format_convs(&v)
}

async fn search_messages(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("缺少 query")?;
    let mut owned = vec![("q", query.to_string())];
    if let Some(n) = args.get("limit").and_then(Value::as_i64) {
        owned.push(("limit", n.to_string()));
    }
    let query_ref: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let v = tool_http(
        st,
        b,
        "GET",
        "/v1/agent-tools/search",
        &query_ref,
        None,
        "search",
    )
    .await?;
    let rows = v.as_array().ok_or("search 响应不是列表")?;
    if rows.is_empty() {
        return Ok("（没有匹配的消息）".into());
    }
    let mut out = String::new();
    for m in rows {
        let mut sender = m["sender"].as_str().unwrap_or("?").to_string();
        if m["is_agent"] == true {
            sender.push_str(" (Agent)");
        }
        out.push_str(&format!(
            "[seq {}] {} / {sender}: {}\n",
            m["seq"].as_i64().unwrap_or(0),
            m["conversation_title"].as_str().unwrap_or("会话"),
            m["text"].as_str().unwrap_or("")
        ));
    }
    Ok(out)
}

async fn read_conversation(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let id = args
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("缺少 conversation_id")?;
    let mut owned = Vec::new();
    if let Some(n) = args.get("before_seq").and_then(Value::as_i64) {
        owned.push(("before_seq", n.to_string()));
    }
    if let Some(n) = args.get("limit").and_then(Value::as_i64) {
        owned.push(("limit", n.to_string()));
    }
    let query_ref: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let path = format!("/v1/agent-tools/conversations/{id}/messages");
    let v = tool_http(st, b, "GET", &path, &query_ref, None, "read").await?;
    Ok(format_history_body(&v))
}

async fn lookup_title(st: &McpState, b: &Binding, id: &str) -> Result<String, String> {
    let v = tool_http(
        st,
        b,
        "GET",
        "/v1/agent-tools/conversations",
        &[],
        None,
        "list",
    )
    .await?;
    let title = v.as_array().and_then(|arr| {
        arr.iter()
            .find(|c| c["id"].as_str() == Some(id))
            .and_then(|c| c["title"].as_str())
            .map(str::to_string)
    });
    Ok(title.unwrap_or_else(|| id.to_string()))
}

async fn await_send_approval(b: &Binding, title: &str) -> Result<(), String> {
    let Some(link) = b.approvals.clone() else {
        return Err("审批通道不可用".into());
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let tool_call = json!({
        "title": title,
        "kind": "send_message",
        "locations": [],
    });
    let options = json!([
        {"optionId": "allow_once", "name": "本次允许", "kind": "allow_once"},
        {"optionId": "reject_once", "name": "拒绝", "kind": "reject_once"}
    ]);
    let (tx, rx) = tokio::sync::oneshot::channel();
    link.registry.insert(request_id.clone(), tx);
    let _guard = ApprovalGuard {
        registry: link.registry.clone(),
        request_id: request_id.clone(),
    };
    if link
        .tx
        .send(crate::runtime::ApprovalNotice {
            request_id,
            tool_call,
            options: options.clone(),
        })
        .is_err()
    {
        return Err("审批通道已关闭".into());
    }
    let option_id = rx.await.map_err(|_| "审批等待被中断".to_string())?;
    if !option_allowed(&options, &option_id) {
        return Err("主人拒绝了发送".into());
    }
    Ok(())
}

fn file_paths(args: &Value) -> Result<Vec<String>, String> {
    let Some(raw) = args.get("file_paths").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let arr = raw.as_array().ok_or("file_paths 必须是字符串路径数组")?;
    let mut out = Vec::new();
    for item in arr {
        let s = item.as_str().ok_or("file_paths 必须是字符串路径数组")?;
        if s.is_empty() {
            return Err("file_paths 含空路径".into());
        }
        out.push(s.to_string());
    }
    Ok(out)
}

async fn upload_local(
    st: &McpState,
    b: &Binding,
    conv: &str,
    path: &str,
) -> Result<String, String> {
    let p = std::path::Path::new(path);
    let name = p
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or("无法识别文件名")?;
    let data = tokio::fs::read(p)
        .await
        .map_err(|e| format!("读文件失败: {e}"))?;
    let mime = mime_guess::from_path(p).first_or_octet_stream();
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(name.to_string())
        .mime_str(mime.as_ref())
        .map_err(|e| format!("MIME 非法: {e}"))?;
    let url = format!("{}/v1/agent-tools/files", b.server);
    let r = st
        .http
        .post(&url)
        .query(&[("conversation_id", conv)])
        .header("authorization", format!("Invocation {}", b.token))
        .multipart(reqwest::multipart::Form::new().part("file", part))
        .send()
        .await
        .map_err(|e| format!("upload 请求失败: {e}"))?;
    let status = r.status();
    let text = r
        .text()
        .await
        .map_err(|e| format!("upload 响应读取失败: {e}"))?;
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let msg =
            parsed["error"]
                .as_str()
                .unwrap_or(if text.is_empty() { "?" } else { text.as_str() });
        return Err(format!("upload {}: {msg}", status.as_u16()));
    }
    parsed["attachment_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "upload 响应没有 attachment_id".into())
}

async fn send_message(st: &McpState, b: &Binding, args: &Value) -> Result<String, String> {
    let conv = args
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("缺少 conversation_id")?;
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    let paths = file_paths(args)?;
    let tier = if b.permission_tier.is_empty() {
        "read_only"
    } else {
        b.permission_tier.as_str()
    };
    if !sends_directly(tier) {
        let title_conv = lookup_title(st, b, conv).await?;
        let title = format!("发消息到「{title_conv}」：{}", preview80(text));
        await_send_approval(b, &title).await?;
    }
    let mut ids = Vec::new();
    for path in &paths {
        ids.push(upload_local(st, b, conv, path).await?);
    }
    let body = json!({
        "conversation_id": conv,
        "text": text,
        "attachment_ids": ids,
    });
    let v = tool_http(
        st,
        b,
        "POST",
        "/v1/agent-tools/messages",
        &[],
        Some(&body),
        "send",
    )
    .await?;
    Ok(format!(
        "已发送 message_id={}",
        v["message_id"].as_str().unwrap_or("")
    ))
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
