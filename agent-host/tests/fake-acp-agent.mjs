// 假 ACP agent（NDJSON over stdio），供 agent-host 与 server 集成测试使用。
// 启动参数：
//   --mcp=http   initialize 里声明 agentCapabilities.mcpCapabilities.http
//   --mcp=stdio  不声明 http 能力（宿主改用 stdio 桥接子进程注入）
// prompt 行为：
//   echo:<text>        → 至少 2 个 agent_message_chunk 分块，合计恰为 <text>
//   long:<n>           → 总分块长度 = n（单块 ≤ 997 字符，强迫多块）
//   sleep:<ms>         → 等 ms 毫秒后回 "done"
//   tool:read_history  → 经注入的 MCP 服务 tools/call read_conversation_history，原样回出文本
//   fail               → JSON-RPC error
//   crash              → 直接退出（进程码 2）
//   其他                → 回 "ok"
import readline from "node:readline";
import { spawn } from "node:child_process";

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

const mcpMode = (process.argv.find((a) => a.startsWith("--mcp=")) || "").slice(6);
let sessionId = "fake-session";
let mcpServers = [];

// ---- 注入工具的 MCP 客户端（http 直连 / stdio 桥接子进程） ----
let mcpSeq = 0;
let mcpChild = null;
const mcpPending = new Map();

const chatServer = () => (mcpServers || []).find((s) => s.name === "chat");

function mcpHttp(url, method, params, withId) {
  const msg = { jsonrpc: "2.0", method, params };
  if (withId) msg.id = ++mcpSeq;
  return fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(msg),
  }).then(async (r) => {
    const text = await r.text();
    return text ? JSON.parse(text) : null;
  });
}

function mcpStdio(method, params, withId) {
  const srv = chatServer();
  if (!srv) return Promise.reject(new Error("no chat mcp server"));
  if (!mcpChild) {
    // 桥接条目：{"name":"chat","command":<桥接程序>,"args":[..., "<url>"]}
    mcpChild = spawn(srv.command, srv.args ?? [], { stdio: ["pipe", "pipe", "inherit"] });
    const sub = readline.createInterface({ input: mcpChild.stdout });
    sub.on("line", (line) => {
      let v;
      try {
        v = JSON.parse(line);
      } catch {
        return;
      }
      if (v.id !== undefined && mcpPending.has(v.id)) {
        mcpPending.get(v.id)(v);
        mcpPending.delete(v.id);
      }
    });
    mcpChild.on("exit", (c) => {
      for (const [, resolve] of mcpPending) {
        resolve({ error: { code: -32000, message: `bridge exited ${c}` } });
      }
      mcpPending.clear();
      mcpChild = null;
    });
  }
  const msg = { jsonrpc: "2.0", method, params };
  if (!withId) {
    mcpChild.stdin.write(JSON.stringify(msg) + "\n");
    return Promise.resolve(null);
  }
  msg.id = ++mcpSeq;
  return new Promise((resolve) => {
    mcpPending.set(msg.id, resolve);
    mcpChild.stdin.write(JSON.stringify(msg) + "\n");
  });
}

function mcpCall(method, params, withId = true) {
  const srv = chatServer();
  if (!srv) return Promise.reject(new Error("no chat mcp server"));
  if (srv.type === "http") return mcpHttp(srv.url, method, params, withId);
  return mcpStdio(method, params, withId);
}

async function runReadHistory() {
  await mcpCall("initialize", {
    protocolVersion: "2025-03-26",
    capabilities: {},
    clientInfo: { name: "fake-acp-agent", version: "0" },
  });
  await mcpCall("notifications/initialized", {}, false);
  const r = await mcpCall("tools/call", {
    name: "read_conversation_history",
    arguments: {},
  });
  const content = r?.result?.content;
  if (Array.isArray(content)) return content.map((c) => c.text ?? "").join("\n");
  return JSON.stringify(r);
}

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
    // --mcp=http 才声明 http 能力；stdio/不带参数 = 只支持 stdio
    const caps =
      mcpMode === "http" ? { mcpCapabilities: { http: true } } : {};
    return result(id, { protocolVersion: 1, agentCapabilities: caps, authMethods: [] });
  }
  if (method === "session/new") {
    sessionId = "fake-session";
    mcpServers = params?.mcpServers ?? [];
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
    if (text === "tool:read_history") {
      runReadHistory()
        .then((out) => {
          chunk(sessionId, out || "(empty)");
          result(id, { stopReason: "end_turn" });
        })
        .catch((e) => {
          chunk(sessionId, `tool error: ${e?.message || e}`);
          result(id, { stopReason: "end_turn" });
        });
      return;
    }
    if (text === "fail") return rpcErr(id, "fake failure");
    if (text === "crash") process.exit(2);
    chunk(sessionId, "ok");
    return result(id, { stopReason: "end_turn" });
  }
  return rpcErr(id, `unknown method ${method}`);
});
