import json
import threading
import unittest
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import chatbot
from chatbot import Bot, ChatError, verify


def serve(handler):
    srv = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_port}"


class FakeChat(BaseHTTPRequestHandler):
    """假聊天服务器：令牌对就回 200，否则 401"""

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        ok = self.path == "/v1/hooks" and self.headers["Authorization"] == "Bearer good"
        status, out = (200, {"id": "m1", "conversation_id": body.get("conversation_id", "dm"), "seq": 1}) if ok else (401, {"error": "invalid bot token"})
        data = json.dumps(out).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass


class Test(unittest.TestCase):
    def test_signature_matches_server(self):
        # 与服务端 hmac_sha256 单测同一组 RFC 4231 向量
        sig = "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        self.assertTrue(verify("Jefe", b"what do ya want for nothing?", sig))
        self.assertFalse(verify("Jefe", b"tampered", sig))
        self.assertFalse(verify("Jefe", b"x", None))

    def test_send(self):
        srv, url = serve(FakeChat)
        try:
            self.assertEqual(Bot(url, token="good").send("hi")["conversation_id"], "dm")
            self.assertEqual(Bot(url + "/", token="good").send("hi", "c9")["conversation_id"], "c9")
            with self.assertRaises(ChatError) as e:
                Bot(url, token="bad").send("hi")
            self.assertEqual((e.exception.status, e.exception.message), (401, "invalid bot token"))
        finally:
            srv.shutdown()

    def test_webhook(self):
        bot = Bot("http://unused", secret="s3cret")

        @bot.on_message
        def echo(ev):
            return None if ev.conversation_type == "group" else "pong: " + ev.text

        def call(payload, secret="s3cret"):
            body = json.dumps(payload).encode()
            return bot.handle(body, "sha256=" + chatbot.hmac.new(secret.encode(), body, "sha256").hexdigest())

        ev = {"bot_id": "b", "conversation_id": "c", "conversation_type": "dm", "message": {"content": "ping"}}
        self.assertEqual(call(ev), (200, {"content": "pong: ping"}))
        self.assertEqual(call({**ev, "conversation_type": "group"}), (200, {}))
        self.assertEqual(call(ev, secret="wrong")[0], 401)
        self.assertEqual(call({"bot_id": "b"})[0], 400)

    def test_run_serves_http(self):
        bot = Bot("http://unused", secret="k")
        bot.on_message(lambda ev: "ok")
        srv = ThreadingHTTPServer(("127.0.0.1", 0), BaseHTTPRequestHandler)
        port = srv.server_port
        srv.server_close()
        threading.Thread(target=bot.run, kwargs={"host": "127.0.0.1", "port": port}, daemon=True).start()
        body = json.dumps({"bot_id": "b", "conversation_id": "c", "conversation_type": "dm", "message": {"content": "x"}}).encode()
        sig = "sha256=" + chatbot.hmac.new(b"k", body, "sha256").hexdigest()
        req = urllib.request.Request(f"http://127.0.0.1:{port}/any/path", data=body, headers={"X-Chat-Signature": sig}, method="POST")
        for _ in range(50):
            try:
                with urllib.request.urlopen(req, timeout=2) as r:
                    self.assertEqual(json.load(r), {"content": "ok"})
                    return
            except urllib.error.URLError:
                threading.Event().wait(0.05)
        self.fail("bot.run never answered")


if __name__ == "__main__":
    unittest.main()
