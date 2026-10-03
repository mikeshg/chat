import chatbot.ChatBot;
import com.sun.net.httpserver.HttpServer;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.util.Map;

/** 无框架自检：javac -d out chatbot/ChatBot.java ChatBotTest.java && java -cp out ChatBotTest */
public class ChatBotTest {
    static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
        System.out.println("ok  " + what);
    }

    static String sign(String secret, String body) throws Exception {
        javax.crypto.Mac m = javax.crypto.Mac.getInstance("HmacSHA256");
        m.init(new javax.crypto.spec.SecretKeySpec(secret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
        StringBuilder s = new StringBuilder("sha256=");
        for (byte b : m.doFinal(body.getBytes(StandardCharsets.UTF_8))) s.append(String.format("%02x", b));
        return s.toString();
    }

    public static void main(String[] args) throws Exception {
        // 与服务端 hmac_sha256 单测同一组 RFC 4231 向量
        String vec = "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        check(ChatBot.verify("Jefe", "what do ya want for nothing?".getBytes(StandardCharsets.UTF_8), vec),
                "signature matches server");
        check(!ChatBot.verify("Jefe", "tampered".getBytes(StandardCharsets.UTF_8), vec), "tampered body rejected");
        check(!ChatBot.verify("Jefe", new byte[0], null), "missing signature rejected");

        // send：假聊天服务器，令牌对回 200，否则 401
        HttpServer fake = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        fake.createContext("/", ex -> {
            String body = new String(ex.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            boolean ok = ex.getRequestURI().getPath().equals("/v1/hooks")
                    && "Bearer good".equals(ex.getRequestHeaders().getFirst("Authorization"));
            String conv = body.contains("\"conversation_id\":\"c9\"") ? "c9" : "dm";
            String id = body.contains("\"client_msg_id\":\"k1\"") ? "k1" : "m1";
            String out = ok ? "{\"id\":\"" + id + "\",\"conversation_id\":\"" + conv + "\",\"seq\":1}"
                    : "{\"error\":\"invalid bot token\"}";
            byte[] b = out.getBytes(StandardCharsets.UTF_8);
            ex.sendResponseHeaders(ok ? 200 : 401, b.length);
            try (OutputStream os = ex.getResponseBody()) {
                os.write(b);
            }
        });
        fake.start();
        String url = "http://127.0.0.1:" + fake.getAddress().getPort();
        try {
            ChatBot.Sent s = new ChatBot(url + "/", "good", null).send("hi \"quoted\" 中文\n");
            check(s.conversationId.equals("dm") && s.seq == 1, "send to owner DM");
            ChatBot.Sent s2 = new ChatBot(url, "good", null).send("hi", "c9", "k1");
            check(s2.conversationId.equals("c9") && s2.id.equals("k1"), "send to conversation with idempotency key");
            try {
                new ChatBot(url, "bad", null).send("hi");
                check(false, "bad token should throw");
            } catch (ChatBot.ChatException e) {
                check(e.status == 401 && e.error.equals("invalid bot token"), "bad token -> ChatException 401");
            }
        } finally {
            fake.stop(0);
        }

        // 回调：真起 HTTP 服务
        ChatBot bot = new ChatBot("http://unused", null, "s3cret").onMessage(ev -> {
            if (ev.text().equals("boom")) throw new RuntimeException("boom");
            return ev.conversationType.equals("dm") ? "pong: " + ev.text() : null;
        });
        HttpServer srv = bot.run(0);
        HttpClient http = HttpClient.newHttpClient();
        try {
            String base = "http://127.0.0.1:" + srv.getAddress().getPort() + "/any";
            java.util.function.BiFunction<String, String, HttpResponse<String>> call = (body, secret) -> {
                try {
                    HttpRequest req = HttpRequest.newBuilder(URI.create(base))
                            .header("X-Chat-Signature", sign(secret, body))
                            .POST(HttpRequest.BodyPublishers.ofString(body)).build();
                    return http.send(req, HttpResponse.BodyHandlers.ofString());
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            };
            java.util.function.BiFunction<String, String, String> ev = (kind, text) ->
                    "{\"bot_id\":\"b\",\"conversation_id\":\"c\",\"conversation_type\":\"" + kind
                            + "\",\"message\":{\"seq\":3,\"mentions\":[\"u1\"],\"reply_to_id\":null,\"content\":\""
                            + text + "\"}}";
            HttpResponse<String> r = call.apply(ev.apply("dm", "ping \\u4e2d\\\"x\\\""), "s3cret");
            check(r.statusCode() == 200 && r.body().equals("{\"content\":\"pong: ping 中\\\"x\\\"\"}"),
                    "dm reply with escapes: " + r.body());
            r = call.apply(ev.apply("group", "ping"), "s3cret");
            check(r.statusCode() == 200 && r.body().equals("{}"), "group no reply");
            check(call.apply(ev.apply("dm", "ping"), "wrong").statusCode() == 401, "bad signature -> 401");
            check(call.apply("{\"x\":1}", "s3cret").statusCode() == 400, "bad payload -> 400");
            check(call.apply("{not json", "s3cret").statusCode() == 400, "broken JSON -> 400");
            check(call.apply(ev.apply("dm", "boom"), "s3cret").statusCode() == 500, "handler throws -> 500");
        } finally {
            srv.stop(0);
        }
        System.out.println("all passed");
        System.exit(0); // run() 的线程池不是守护线程
    }
}
