// Agent A29：内置 agent —— 一个 ACP stdio agent 程序。
//
// 包装 xiefan/agent 底座（Provider + 运行循环 + 工具/钩子），在
// stdin/stdout 上说 NDJSON JSON-RPC（ACP），宿主（acp.rs）像驱动
// 外部 agent 程序一样驱动它：initialize → session/new | session/load
// → session/prompt，期间反向收 session/request_permission。
//
// 模型端点由服务端随调用下发（环境变量 CHAT_BUILTIN_ENDPOINTS，
// JSON 数组，含 api_key）；会话存档在 CHAT_BUILTIN_STATE_DIR。
//
// stdout 只走协议帧；诊断日志一律写 stderr（eprintln!）。

mod acp_server;
mod mcp_client;
mod store;
mod tools;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

/// stdio 上的双向 JSON-RPC 通道。写侧串行化所有出站帧；发出去的
/// 请求登记在 pending 里，读循环按 id 把响应送回等待方。
#[derive(Clone)]
pub(crate) struct Rpc {
    inner: Arc<RpcInner>,
}

struct RpcInner {
    stdout: AsyncMutex<tokio::io::Stdout>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// 出站请求 id（字符串前缀，和入站的数字 id 分属两个方向不会冲突）
    next_id: AtomicU64,
}

impl Rpc {
    fn new() -> Self {
        Self {
            inner: Arc::new(RpcInner {
                stdout: AsyncMutex::new(tokio::io::stdout()),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// 写一帧（请求 / 响应 / 通知共用，保证一帧一行不重排）。
    async fn write(&self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        let mut out = self.inner.stdout.lock().await;
        out.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("stdout 写入失败: {e}"))?;
        out.flush()
            .await
            .map_err(|e| format!("stdout flush 失败: {e}"))
    }

    /// 回一个入站请求的响应帧。
    async fn respond(&self, id: &Value, result: Result<Value, (i64, String)>) {
        let frame = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        };
        let _ = self.write(&frame).await;
    }

    /// session/update 通知。
    async fn update(&self, session_id: &str, update: Value) {
        let _ = self
            .write(&json!({"jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":session_id,"update":update}}))
            .await;
    }

    /// agent→client 请求：发出后等对方响应；用于 session/request_permission。
    /// Err = 对方回了 JSON-RPC 错误或连接已断。
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = format!("o{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id.clone(), tx);
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if let Err(e) = self.write(&req).await {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.await {
            Ok(msg) => match msg.get("error") {
                Some(e) => Err(e["message"].as_str().unwrap_or("请求失败").to_string()),
                None => Ok(msg["result"].clone()),
            },
            Err(_) => Err("连接已关闭".into()),
        }
    }

    /// 入站帧若是出站请求的响应，送回等待方并返回 true。
    fn take_response(&self, msg: &Value) -> bool {
        if msg.get("method").is_some()
            || (msg.get("result").is_none() && msg.get("error").is_none())
        {
            return false;
        }
        let Some(id) = msg.get("id") else {
            return false;
        };
        let key = id
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| id.to_string());
        match self.inner.pending.lock().unwrap().remove(&key) {
            Some(tx) => {
                let _ = tx.send(msg.clone());
                true
            }
            None => false,
        }
    }
}

/// 跑 ACP stdio 服务直到 stdin 关闭（宿主 kill 子进程时 stdin 断开）。
pub async fn run_stdio() -> Result<(), String> {
    let server = acp_server::Server::new(Rpc::new());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => return Ok(()),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        server.dispatch(msg);
    }
}
