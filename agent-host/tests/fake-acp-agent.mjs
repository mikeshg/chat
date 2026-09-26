// 假 ACP agent（NDJSON over stdio），供 agent-host 与 server 集成测试使用。
// prompt 行为：
//   echo:<text>  → 至少 2 个 agent_message_chunk 分块，合计恰为 <text>
//   long:<n>     → 总分块长度 = n（单块 ≤ 997 字符，强迫多块）
//   sleep:<ms>   → 等 ms 毫秒后回 "done"
//   fail         → JSON-RPC error
//   crash        → 直接退出（进程码 2）
//   其他          → 回 "ok"
import readline from "node:readline";

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

rl.on("line", (line) => {
  let m;
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  const { id, method, params } = m;
  if (id === undefined) return; // 通知一律不回

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
      setTimeout(() => {
        chunk(sessionId, "done");
        result(id, { stopReason: "end_turn" });
      }, ms);
      return;
    }
    if (text === "fail") return rpcErr(id, "fake failure");
    if (text === "crash") process.exit(2);
    chunk(sessionId, "ok");
    return result(id, { stopReason: "end_turn" });
  }
  return rpcErr(id, `unknown method ${method}`);
});
