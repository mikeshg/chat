"""聊天服务器 webhook bot 的 Python SDK。只用标准库，单文件，拷过去就能用。

    from chatbot import Bot

    bot = Bot("https://chat.example.com", token="入站令牌", secret="签名密钥")
    bot.send("部署完成")                         # 发到和主人的私聊

    @bot.on_message
    def reply(event):                           # 私聊它、或在群里 @ 它时调用
        return "收到：" + event.text            # 返回字符串 = 回复；返回 None = 不回

    bot.run(port=8080)                          # 回调地址填 http://<这台机器>:8080/
"""

import hashlib
import hmac
import json
import urllib.error
import urllib.request
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

__all__ = ["Bot", "Event", "ChatError", "verify"]

# 服务器只等回调 10 秒；回调体不会超过这么大
MAX_BODY = 1024 * 1024


class ChatError(Exception):
    """服务器拒绝了请求。status 401 = 令牌不对，403 = bot 不在那个会话里。"""

    def __init__(self, status, message):
        super().__init__(f"{status}: {message}")
        self.status = status
        self.message = message


@dataclass
class Event:
    """一次回调：bot 在私聊里收到消息，或在群里被 @。"""

    bot_id: str
    conversation_id: str
    conversation_type: str  # "dm" / "group"
    message: dict  # id, sender_id, sender_name, content, mentions, ...

    @property
    def text(self):
        return self.message.get("content") or ""


def verify(secret, body, signature):
    """校验 X-Chat-Signature 头：sha256=HMAC-SHA256(签名密钥, 请求体)。"""
    want = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(want, signature or "")


class Bot:
    def __init__(self, server, token=None, secret=None, timeout=10):
        """server：聊天服务器地址；token：入站令牌（发消息用）；secret：签名密钥（收回调用）。"""
        self.server = server.rstrip("/")
        self.token = token
        self.secret = secret
        self.timeout = timeout
        self._handler = None

    def send(self, content, conversation_id=None, client_msg_id=None):
        """以 bot 身份发一条文字。不带 conversation_id 就发到和主人的私聊。
        client_msg_id：幂等键，重试时传同一个值不会重复发。
        返回 {"id", "conversation_id", "seq"}。"""
        if not self.token:
            raise ValueError("send() 需要 token")
        body = {"content": content}
        if conversation_id:
            body["conversation_id"] = conversation_id
        if client_msg_id:
            body["client_msg_id"] = client_msg_id
        req = urllib.request.Request(
            self.server + "/v1/hooks",
            data=json.dumps(body).encode(),
            headers={
                "Authorization": "Bearer " + self.token,
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            try:
                msg = json.load(e).get("error", "")
            except ValueError:
                msg = e.reason
            raise ChatError(e.code, msg) from None

    def on_message(self, fn):
        """装饰器：注册回调处理函数 fn(event) -> str | None。"""
        self._handler = fn
        return fn

    def handle(self, body, signature):
        """处理一次回调请求，返回 (HTTP 状态码, 响应 JSON)。
        想挂到 Flask / FastAPI 等自己的 web 框架上时直接调它。"""
        if not self.secret:
            raise ValueError("收回调需要 secret")
        if not verify(self.secret, body, signature):
            return 401, {"error": "bad signature"}
        try:
            d = json.loads(body)
            event = Event(d["bot_id"], d["conversation_id"], d["conversation_type"], d["message"])
        except (ValueError, KeyError, TypeError):
            return 400, {"error": "bad payload"}
        reply = self._handler(event) if self._handler else None
        return 200, ({"content": reply} if reply else {})

    def run(self, host="0.0.0.0", port=8080):
        """起一个 HTTP 服务收回调，阻塞到 Ctrl+C。任何路径都接。"""
        bot = self

        class H(BaseHTTPRequestHandler):
            def do_POST(self):
                n = int(self.headers.get("Content-Length") or 0)
                if n > MAX_BODY:
                    return self._reply(413, {"error": "too large"})
                body = self.rfile.read(n)
                try:
                    status, out = bot.handle(body, self.headers.get("X-Chat-Signature"))
                except Exception as e:  # 处理函数出错不该拖垮服务
                    self.log_error("handler failed: %r", e)
                    status, out = 500, {"error": "handler failed"}
                self._reply(status, out)

            def _reply(self, status, out):
                data = json.dumps(out).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        srv = ThreadingHTTPServer((host, port), H)
        print(f"bot listening on http://{host}:{port}/")
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            srv.server_close()
