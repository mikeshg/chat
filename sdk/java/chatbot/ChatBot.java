package chatbot;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;
import java.util.function.Function;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * 聊天服务器 webhook bot 的 Java SDK。JDK 11+，只用 JDK 自带的类，单文件。
 *
 * <pre>{@code
 * ChatBot bot = new ChatBot("https://chat.example.com", "入站令牌", "签名密钥");
 * bot.send("部署完成");                        // 发到和主人的私聊
 * bot.onMessage(ev -> "收到：" + ev.text());    // 私聊它、或在群里 @ 它时调用；返回 null = 不回
 * bot.run(8080);                              // 回调地址填 http://<这台机器>:8080/
 * }</pre>
 */
public class ChatBot {
    /** 回调体不会超过这么大 */
    private static final int MAX_BODY = 1 << 20;

    private final String server;
    private final String token;
    private final String secret;
    private final HttpClient client =
            HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(10)).build();
    private volatile Function<Event, String> handler;

    /** server：聊天服务器地址；token：入站令牌（发消息用）；secret：签名密钥（收回调用）。 */
    public ChatBot(String server, String token, String secret) {
        this.server = server.replaceAll("/+$", "");
        this.token = token;
        this.secret = secret;
    }

    /** 一次回调：bot 在私聊里收到消息，或在群里被 @。 */
    public static final class Event {
        public final String botId;
        public final String conversationId;
        /** "dm" / "group" */
        public final String conversationType;
        /** id, sender_id, sender_name, content, kind, mentions, ... */
        public final Map<String, Object> message;

        Event(String botId, String conversationId, String conversationType, Map<String, Object> message) {
            this.botId = botId;
            this.conversationId = conversationId;
            this.conversationType = conversationType;
            this.message = message;
        }

        public String text() {
            Object c = message.get("content");
            return c instanceof String ? (String) c : "";
        }
    }

    /** send 的结果 */
    public static final class Sent {
        public final String id;
        public final String conversationId;
        public final long seq;

        Sent(String id, String conversationId, long seq) {
            this.id = id;
            this.conversationId = conversationId;
            this.seq = seq;
        }
    }

    /** 服务器拒绝了请求。status 401 = 令牌不对，403 = bot 不在那个会话里。 */
    public static final class ChatException extends IOException {
        private static final long serialVersionUID = 1L;
        public final int status;
        public final String error;

        ChatException(int status, String error) {
            super(status + ": " + error);
            this.status = status;
            this.error = error;
        }
    }

    /** handle 的结果：HTTP 状态码 + 响应 JSON */
    public static final class Reply {
        public final int status;
        public final String json;

        Reply(int status, String json) {
            this.status = status;
            this.json = json;
        }
    }

    /** 校验 X-Chat-Signature 头：sha256=HMAC-SHA256(签名密钥, 请求体)。 */
    public static boolean verify(String secret, byte[] body, String signature) {
        if (signature == null) return false;
        String want = "sha256=" + hex(hmac(secret, body));
        return MessageDigest.isEqual(
                want.getBytes(StandardCharsets.UTF_8), signature.getBytes(StandardCharsets.UTF_8));
    }

    /** 以 bot 身份发到和主人的私聊。 */
    public Sent send(String content) throws IOException, InterruptedException {
        return sendTo(null, content);
    }

    /** 以 bot 身份发到指定会话；conversationId 为 null = 和主人的私聊。 */
    public Sent sendTo(String conversationId, String content) throws IOException, InterruptedException {
        String body = "{\"content\":" + Json.quote(content)
                + (conversationId == null ? "" : ",\"conversation_id\":" + Json.quote(conversationId)) + "}";
        HttpRequest req = HttpRequest.newBuilder(URI.create(server + "/v1/hooks"))
                .timeout(Duration.ofSeconds(10))
                .header("Authorization", "Bearer " + token)
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString(body))
                .build();
        HttpResponse<String> resp = client.send(req, HttpResponse.BodyHandlers.ofString());
        Map<String, Object> out;
        try {
            out = Json.object(resp.body());
        } catch (IllegalArgumentException e) {
            out = new LinkedHashMap<>();
        }
        if (resp.statusCode() / 100 != 2) {
            Object err = out.get("error");
            throw new ChatException(resp.statusCode(), err instanceof String ? (String) err : "");
        }
        return new Sent((String) out.get("id"), (String) out.get("conversation_id"),
                ((Number) out.get("seq")).longValue());
    }

    /** 注册回调处理函数；返回非空字符串即作为回复。服务器只等 10 秒。 */
    public ChatBot onMessage(Function<Event, String> fn) {
        this.handler = fn;
        return this;
    }

    /** 处理一次回调请求。用 Spring 等别的框架时直接调它。 */
    @SuppressWarnings("unchecked")
    public Reply handle(byte[] body, String signature) {
        if (!verify(secret, body, signature)) return new Reply(401, "{\"error\":\"bad signature\"}");
        Event ev;
        try {
            Map<String, Object> d = Json.object(new String(body, StandardCharsets.UTF_8));
            ev = new Event((String) d.get("bot_id"), (String) d.get("conversation_id"),
                    (String) d.get("conversation_type"), (Map<String, Object>) d.get("message"));
            if (ev.botId == null || ev.message == null) throw new IllegalArgumentException("missing fields");
        } catch (IllegalArgumentException | ClassCastException e) {
            return new Reply(400, "{\"error\":\"bad payload\"}");
        }
        Function<Event, String> h = handler;
        String reply = h == null ? null : h.apply(ev);
        if (reply == null || reply.isEmpty()) return new Reply(200, "{}");
        return new Reply(200, "{\"content\":" + Json.quote(reply) + "}");
    }

    /** 在 port 上收回调，任何路径都接。返回已启动的服务，stop(0) 停掉。 */
    public HttpServer run(int port) throws IOException {
        HttpServer srv = HttpServer.create(new InetSocketAddress(port), 0);
        srv.setExecutor(Executors.newCachedThreadPool());
        srv.createContext("/", this::serve);
        srv.start();
        System.err.println("bot listening on http://0.0.0.0:" + srv.getAddress().getPort() + "/");
        return srv;
    }

    private void serve(HttpExchange ex) throws IOException {
        Reply r;
        if (!"POST".equals(ex.getRequestMethod())) {
            r = new Reply(405, "{\"error\":\"POST only\"}");
        } else {
            byte[] body;
            try (InputStream in = ex.getRequestBody()) {
                body = in.readNBytes(MAX_BODY + 1);
            }
            if (body.length > MAX_BODY) {
                r = new Reply(413, "{\"error\":\"too large\"}");
            } else {
                try {
                    r = handle(body, ex.getRequestHeaders().getFirst("X-Chat-Signature"));
                } catch (RuntimeException e) { // 处理函数出错不该拖垮服务
                    System.err.println("chatbot: handler failed: " + e);
                    r = new Reply(500, "{\"error\":\"handler failed\"}");
                }
            }
        }
        byte[] out = r.json.getBytes(StandardCharsets.UTF_8);
        ex.getResponseHeaders().set("Content-Type", "application/json");
        ex.sendResponseHeaders(r.status, out.length);
        try (OutputStream os = ex.getResponseBody()) {
            os.write(out);
        }
    }

    static byte[] hmac(String secret, byte[] body) {
        try {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(secret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
            return m.doFinal(body);
        } catch (java.security.GeneralSecurityException e) {
            throw new IllegalStateException(e); // 每个 JDK 都有 HmacSHA256
        }
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }

    /** 最小 JSON：解析成 Map / List / String / Long / Double / Boolean / null。JDK 没有自带的。 */
    static final class Json {
        private final String s;
        private int i;

        private Json(String s) {
            this.s = s;
        }

        @SuppressWarnings("unchecked")
        static Map<String, Object> object(String text) {
            Json p = new Json(text);
            Object v = p.value();
            p.ws();
            if (p.i != text.length() || !(v instanceof Map)) throw new IllegalArgumentException("not a JSON object");
            return (Map<String, Object>) v;
        }

        static String quote(String v) {
            StringBuilder b = new StringBuilder("\"");
            for (char c : v.toCharArray()) {
                if (c == '"' || c == '\\') b.append('\\').append(c);
                else if (c < 0x20) b.append(String.format("\\u%04x", (int) c));
                else b.append(c);
            }
            return b.append('"').toString();
        }

        private Object value() {
            ws();
            if (i >= s.length()) throw err();
            char c = s.charAt(i);
            if (c == '{') {
                i++;
                Map<String, Object> m = new LinkedHashMap<>();
                if (peek('}')) return m;
                do {
                    ws();
                    String k = string();
                    ws();
                    expect(':');
                    m.put(k, value());
                    ws();
                } while (eat(','));
                expect('}');
                return m;
            }
            if (c == '[') {
                i++;
                List<Object> l = new ArrayList<>();
                if (peek(']')) return l;
                do {
                    l.add(value());
                    ws();
                } while (eat(','));
                expect(']');
                return l;
            }
            if (c == '"') return string();
            if (s.startsWith("true", i)) { i += 4; return Boolean.TRUE; }
            if (s.startsWith("false", i)) { i += 5; return Boolean.FALSE; }
            if (s.startsWith("null", i)) { i += 4; return null; }
            int start = i;
            while (i < s.length() && "+-0123456789.eE".indexOf(s.charAt(i)) >= 0) i++;
            String n = s.substring(start, i);
            try {
                return n.matches("-?\\d+") ? (Object) Long.parseLong(n) : (Object) Double.parseDouble(n);
            } catch (NumberFormatException e) {
                throw err();
            }
        }

        private String string() {
            expect('"');
            StringBuilder b = new StringBuilder();
            while (true) {
                if (i >= s.length()) throw err();
                char c = s.charAt(i++);
                if (c == '"') return b.toString();
                if (c != '\\') { b.append(c); continue; }
                if (i >= s.length()) throw err();
                char e = s.charAt(i++);
                switch (e) {
                    case 'n': b.append('\n'); break;
                    case 't': b.append('\t'); break;
                    case 'r': b.append('\r'); break;
                    case 'b': b.append('\b'); break;
                    case 'f': b.append('\f'); break;
                    case 'u':
                        if (i + 4 > s.length()) throw err();
                        try {
                            b.append((char) Integer.parseInt(s.substring(i, i + 4), 16));
                        } catch (NumberFormatException x) {
                            throw err();
                        }
                        i += 4;
                        break;
                    default: b.append(e); // \" \\ \/
                }
            }
        }

        private void ws() {
            while (i < s.length() && Character.isWhitespace(s.charAt(i))) i++;
        }

        private boolean peek(char c) {
            ws();
            if (i < s.length() && s.charAt(i) == c) { i++; return true; }
            return false;
        }

        private boolean eat(char c) {
            return peek(c);
        }

        private void expect(char c) {
            if (!peek(c)) throw err();
        }

        private IllegalArgumentException err() {
            return new IllegalArgumentException("bad JSON at " + i);
        }
    }
}
