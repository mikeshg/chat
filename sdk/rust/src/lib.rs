//! 聊天服务器 webhook bot 的 Rust SDK。同步实现，不需要 async 运行时。
//!
//! ```no_run
//! use chatbot::Bot;
//!
//! let bot = Bot::new("https://chat.example.com", "入站令牌", "签名密钥");
//! bot.send("部署完成").unwrap();                    // 发到和主人的私聊
//! bot.on_message(|ev| Some(format!("收到：{}", ev.message.content))) // None = 不回
//!     .run("0.0.0.0:8080")                          // 回调地址填 http://<这台机器>:8080/
//!     .unwrap();
//! ```

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// 回调体不会超过这么大
const MAX_BODY: u64 = 1 << 20;

/// 服务器推来的消息，只列常用字段。
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub sender_id: String,
    #[serde(default)]
    pub sender_username: String,
    #[serde(default)]
    pub sender_name: String,
    #[serde(default)]
    pub content: String,
    /// text / image / file / audio / poll
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    pub reply_to_id: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

/// 一次回调：bot 在私聊里收到消息，或在群里被 @。
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub bot_id: String,
    pub conversation_id: String,
    /// dm / group
    pub conversation_type: String,
    pub message: Message,
}

/// `send` 的结果。
#[derive(Debug, Clone, Deserialize)]
pub struct Sent {
    pub id: String,
    pub conversation_id: String,
    pub seq: i64,
}

#[derive(Debug)]
pub enum Error {
    /// 服务器拒绝了请求。401 = 令牌不对，403 = bot 不在那个会话里。
    Chat { status: u16, message: String },
    /// 网络、解析等其他错误
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Chat { status, message } => write!(f, "{status}: {message}"),
            Error::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

fn mac(secret: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac takes any key");
    m.update(body);
    m
}

/// 校验 X-Chat-Signature 头：sha256=HMAC-SHA256(签名密钥, 请求体)。
pub fn verify(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Some(sig) = decode_hex(hex) else {
        return false;
    };
    mac(secret, body).verify_slice(&sig).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

type Handler = dyn Fn(&Event) -> Option<String> + Send + Sync;

#[derive(Clone)]
pub struct Bot {
    server: String,
    token: String,
    secret: String,
    agent: ureq::Agent,
    handler: Option<Arc<Handler>>,
}

impl Bot {
    /// server：聊天服务器地址；token：入站令牌（发消息用）；secret：签名密钥（收回调用）。
    /// 只发不收时 secret 传 ""，只收不发时 token 传 ""。
    pub fn new(server: &str, token: &str, secret: &str) -> Self {
        Bot {
            server: server.trim_end_matches('/').to_string(),
            token: token.to_string(),
            secret: secret.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build(),
            handler: None,
        }
    }

    /// 以 bot 身份发到和主人的私聊。
    pub fn send(&self, content: &str) -> Result<Sent, Error> {
        self.send_to(None, content)
    }

    /// 以 bot 身份发到指定会话；None = 和主人的私聊。
    pub fn send_to(&self, conversation_id: Option<&str>, content: &str) -> Result<Sent, Error> {
        let mut body = json!({ "content": content });
        if let Some(c) = conversation_id {
            body["conversation_id"] = c.into();
        }
        let resp = self
            .agent
            .post(&format!("{}/v1/hooks", self.server))
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body);
        match resp {
            Ok(r) => r.into_json().map_err(|e| Error::Other(e.to_string())),
            Err(ureq::Error::Status(status, r)) => {
                let message = r
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                Err(Error::Chat { status, message })
            }
            Err(e) => Err(Error::Other(e.to_string())),
        }
    }

    /// 注册回调处理函数；返回 Some(文字) 即作为回复。服务器只等 10 秒。
    pub fn on_message(
        mut self,
        f: impl Fn(&Event) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.handler = Some(Arc::new(f));
        self
    }

    /// 处理一次回调请求，返回 (HTTP 状态码, 响应 JSON)。用 axum 等别的框架时直接调它。
    pub fn handle(&self, body: &[u8], signature: &str) -> (u16, Value) {
        if !verify(&self.secret, body, signature) {
            return (401, json!({ "error": "bad signature" }));
        }
        let Ok(ev) = serde_json::from_slice::<Event>(body) else {
            return (400, json!({ "error": "bad payload" }));
        };
        match self.handler.as_ref().and_then(|h| h(&ev)) {
            Some(reply) if !reply.is_empty() => (200, json!({ "content": reply })),
            _ => (200, json!({})),
        }
    }

    /// 在 addr（如 "0.0.0.0:8080"）上收回调，任何路径都接。每个请求一个线程，阻塞不返回。
    pub fn run(&self, addr: &str) -> Result<(), Error> {
        let server = tiny_http::Server::http(addr).map_err(|e| Error::Other(e.to_string()))?;
        eprintln!("bot listening on http://{addr}/");
        for req in server.incoming_requests() {
            let bot = self.clone();
            std::thread::spawn(move || bot.serve(req));
        }
        Ok(())
    }

    fn serve(&self, mut req: tiny_http::Request) {
        let sig = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("X-Chat-Signature"))
            .map(|h| h.value.to_string())
            .unwrap_or_default();
        let mut body = Vec::new();
        let read = req.as_reader().take(MAX_BODY + 1).read_to_end(&mut body);
        let (status, out) = if read.is_err() || body.len() as u64 > MAX_BODY {
            (413, json!({ "error": "too large" }))
        } else {
            // 处理函数 panic 不该拖垮服务
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(&body, &sig)))
                .unwrap_or_else(|_| (500, json!({ "error": "handler failed" })))
        };
        let header = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
        let resp = tiny_http::Response::from_string(out.to_string())
            .with_status_code(status)
            .with_header(header);
        let _ = req.respond(resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn sign(secret: &str, body: &[u8]) -> String {
        let bytes = mac(secret, body).finalize().into_bytes();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256={hex}")
    }

    #[test]
    fn verify_matches_server() {
        // 与服务端 hmac_sha256 单测同一组 RFC 4231 向量
        let sig = "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert!(verify("Jefe", b"what do ya want for nothing?", sig));
        assert!(!verify("Jefe", b"tampered", sig));
        assert!(!verify("Jefe", b"x", ""));
        assert!(!verify("Jefe", b"x", "sha256=zz"));
    }

    /// 假聊天服务器：收一个请求，回固定状态码和 body，把收到的请求原文传回来
    fn fake_chat(status: &str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let status = status.to_string();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, h)
    }

    #[test]
    fn send_posts_bearer_and_parses_errors() {
        let (url, h) = fake_chat("200 OK", r#"{"id":"m1","conversation_id":"c9","seq":1}"#);
        let sent = Bot::new(&format!("{url}/"), "good", "")
            .send_to(Some("c9"), "hi")
            .unwrap();
        assert_eq!((sent.conversation_id.as_str(), sent.seq), ("c9", 1));
        let raw = h.join().unwrap().to_lowercase();
        assert!(raw.starts_with("post /v1/hooks "), "{raw}");
        assert!(raw.contains("authorization: bearer good"), "{raw}");

        let (url, _h) = fake_chat("401 Unauthorized", r#"{"error":"invalid bot token"}"#);
        match Bot::new(&url, "bad", "").send("hi") {
            Err(Error::Chat { status, message }) => {
                assert_eq!((status, message.as_str()), (401, "invalid bot token"))
            }
            other => panic!("want Chat error, got {other:?}"),
        }
    }

    #[test]
    fn webhook_over_http() {
        let bot = Bot::new("http://unused", "", "s3cret").on_message(|ev| {
            if ev.message.content == "boom" {
                panic!("boom");
            }
            (ev.conversation_type == "dm").then(|| format!("pong: {}", ev.message.content))
        });
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr = format!("127.0.0.1:{port}");
        {
            let (bot, addr) = (bot.clone(), addr.clone());
            std::thread::spawn(move || bot.run(&addr));
        }
        let ev = |kind: &str, text: &str| {
            format!(
                r#"{{"bot_id":"b","conversation_id":"c","conversation_type":"{kind}","message":{{"id":"m","conversation_id":"c","seq":1,"sender_id":"u","content":"{text}"}}}}"#
            )
        };
        let call = |body: &str, secret: &str| -> (u16, Value) {
            for _ in 0..50 {
                let r = ureq::post(&format!("http://{addr}/any"))
                    .set("X-Chat-Signature", &sign(secret, body.as_bytes()))
                    .send_string(body);
                match r {
                    Ok(r) => return (r.status(), r.into_json().unwrap()),
                    Err(ureq::Error::Status(c, r)) => return (c, r.into_json().unwrap()),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            panic!("bot never answered");
        };
        assert_eq!(
            call(&ev("dm", "ping"), "s3cret"),
            (200, json!({"content": "pong: ping"}))
        );
        assert_eq!(call(&ev("group", "ping"), "s3cret"), (200, json!({})));
        assert_eq!(call(&ev("dm", "ping"), "wrong").0, 401);
        assert_eq!(call(r#"{"x":1}"#, "s3cret").0, 400);
        assert_eq!(call(&ev("dm", "boom"), "s3cret").0, 500);
    }
}
