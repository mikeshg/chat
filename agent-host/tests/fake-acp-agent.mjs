// 假 ACP agent（NDJSON over stdio），供 agent-host 与 server 集成测试使用。
// prompt 行为：
//   echo:<text>  → 至少 2 个 agent_message_chunk 分块，合计恰为 <text>
//   long:<n>     → 总分块长度 = n（单块 ≤ 997 字符，强迫多块）
//   sleep:<ms>   → 等 ms 毫秒后回 "done"
//   fail         → JSON-RPC error
//   crash        → 直接退出（进程码 2）
//   其他          → 回 "ok"
// session/cancel（通知）：进行中的 sleep 立刻以 stopReason "cancelled" 返回。
// 启动参数 --ignore-cancel：忽略取消，用来测宿主 5 秒后强制结束子进程。
import readline from "node:readline";

const ignoreCancel = process.argv.includes("--ignore-cancel");

const rl = readline.createInterface({ input: process.stdin });
const send = (o) => process.stdout.write(JSON.stringify(o) + "\n");
const result = (id, r) => send({ jsonrpc: "2.0", id, result: r });
const rpcErr = (id, message) => send({ jsonrpc: "2.0", id, error: { code: -32000, message } });
const chunk = (sessionId, text) =>
  send({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text },
      },
    },
  });

let sessionId = "fake-session";
let pendingSleep = null;

rl.on("line", (line) => {
  let m;
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  const { id, method, params } = m;
  if (method === "session/cancel") {
    if (ignoreCancel || !pendingSleep) return;
    clearTimeout(pendingSleep.timer);
    const promptId = pendingSleep.id;
    pendingSleep = null;
    return result(promptId, { stopReason: "cancelled" });
  }
  if (id === undefined) return; // 其余通知不回

  if (method === "initialize") {
    return result(id, { protocolVersion: 1, agentCapabilities: {}, authMethods: [] });
  }
  if (method === "session/new") {
    sessionId = "fake-session";
    return result(id, { sessionId });
  }
  if (method === "session/prompt") {
    const text = (params?.prompt ?? []).map((p) => p?.text ?? "").join("");
    if (text.startsWith("echo:")) {
      const out = text.slice(5);
      const mid = Math.max(1, Math.floor(out.length / 2));
      chunk(sessionId, out.slice(0, mid));
      chunk(sessionId, out.slice(mid));
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("long:")) {
      let left = parseInt(text.slice(5), 10) || 0;
      while (left > 0) {
        const k = Math.min(997, left);
        chunk(sessionId, "x".repeat(k));
        left -= k;
      }
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("sleep:")) {
      const ms = parseInt(text.slice(6), 10) || 0;
      const timer = setTimeout(() => {
        pendingSleep = null;
        chunk(sessionId, "done");
        result(id, { stopReason: "end_turn" });
      }, ms);
      pendingSleep = { id, timer };
      return;
    }
    if (text === "fail") return rpcErr(id, "fake failure");
    if (text === "crash") process.exit(2);
    chunk(sessionId, "ok");
    return result(id, { stopReason: "end_turn" });
  }
  return rpcErr(id, `unknown method ${method}`);
});
