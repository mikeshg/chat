// 假 ACP agent（NDJSON over stdio），供 agent-host 与 server 集成测试使用。
// prompt 行为：
//   echo:<text>  → 至少 2 个 agent_message_chunk 分块，合计恰为 <text>
//   long:<n>     → 总分块长度 = n（单块 ≤ 997 字符，强迫多块）
//   sleep:<ms>   → 等 ms 毫秒后回 "done"
//   fail         → JSON-RPC error
//   crash        → 直接退出（进程码 2）
//   recall       → 本会话之前收到过的全部指令，用 | 连接；没有则 (empty)
//   whoami       → 当前 mode / model、环境变量 FAKE_ECHO、第一个文本块、额外目录
//   其他          → 回 "ok"
//
// 会话跨进程保留：状态写在临时目录 chat-fake-acp/<sessionId>.json。
// 启动参数 --caps=resume|load|none（默认 resume）决定 initialize 声明的能力。
// load 模式恢复时先用 session/update 回放已存的回复正文，再返回 session/load 结果。
// 指令取 prompt 的最后一个文本块（附加指令放在前面的块里）。
// session/new 报告两个 mode、两个 model；支持 session/set_mode / session/set_model。
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const capArg = process.argv.find((a) => a.startsWith("--caps="));
const caps = capArg ? capArg.slice("--caps=".length) : "resume";
const dir = path.join(os.tmpdir(), "chat-fake-acp");

function fileOf(id) {
  if (typeof id !== "string" || !/^[A-Za-z0-9_-]+$/.test(id)) return null;
  return path.join(dir, `${id}.json`);
}

function loadState(id) {
  const f = fileOf(id);
  if (!f || !fs.existsSync(f)) return null;
  try {
    const j = JSON.parse(fs.readFileSync(f, "utf8"));
    return {
      prompts: Array.isArray(j.prompts) ? j.prompts.map(String) : [],
      replies: Array.isArray(j.replies) ? j.replies.map(String) : [],
    };
  } catch {
    return null;
  }
}

function saveState(id, st) {
  const f = fileOf(id);
  if (!f) return;
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(f, JSON.stringify(st));
}

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

let sessionId = null;
let state = { prompts: [], replies: [] };
let currentMode = "ask";
let currentModel = "fast";
let additionalDirs = [];

const MODES = [
  { id: "ask", name: "Ask" },
  { id: "code", name: "Code" },
];
const MODELS = [
  { modelId: "fast", name: "Fast" },
  { modelId: "smart", name: "Smart" },
];

function remember(text, reply) {
  state.prompts.push(text);
  if (reply != null) state.replies.push(reply);
  saveState(sessionId, state);
}

function openSaved(sid) {
  const loaded = loadState(sid);
  if (!loaded) return null;
  sessionId = sid;
  state = loaded;
  return loaded;
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
    const agentCapabilities = { sessionCapabilities: { additionalDirectories: {} } };
    if (caps === "resume") agentCapabilities.sessionCapabilities.resume = {};
    else if (caps === "load") agentCapabilities.loadSession = true;
    return result(id, { protocolVersion: 1, agentCapabilities, authMethods: [] });
  }
  if (method === "session/new") {
    sessionId = crypto.randomUUID();
    state = { prompts: [], replies: [] };
    saveState(sessionId, state);
    currentMode = "ask";
    currentModel = "fast";
    additionalDirs = Array.isArray(params?.additionalDirectories) ? params.additionalDirectories : [];
    return result(id, {
      sessionId,
      modes: { currentModeId: currentMode, availableModes: MODES },
      models: { currentModelId: currentModel, availableModels: MODELS },
    });
  }
  if (method === "session/set_mode") {
    currentMode = params?.modeId ?? currentMode;
    return result(id, {});
  }
  if (method === "session/set_model") {
    currentModel = params?.modelId ?? currentModel;
    return result(id, {});
  }
  if (method === "session/resume") {
    if (caps !== "resume") return rpcErr(id, "resume unsupported");
    if (!openSaved(params?.sessionId)) return rpcErr(id, "unknown session");
    return result(id, { sessionId });
  }
  if (method === "session/load") {
    if (caps !== "load") return rpcErr(id, "load unsupported");
    if (!openSaved(params?.sessionId)) return rpcErr(id, "unknown session");
    // 回放必须发生在 session/load 的响应之前，宿主用这一点把历史和本次回复分开
    for (const reply of state.replies) {
      if (reply) chunk(sessionId, reply);
    }
    return result(id, { sessionId });
  }
  if (method === "session/prompt") {
    const blocks = (params?.prompt ?? []).map((p) => (typeof p?.text === "string" ? p.text : ""));
    const text = blocks[blocks.length - 1] ?? "";
    if (text.trim() === "whoami") {
      const out = [
        `mode=${currentMode}`,
        `model=${currentModel}`,
        `FAKE_ECHO=${process.env.FAKE_ECHO ?? ""}`,
        `first=${blocks[0] ?? ""}`,
        `dirs=${additionalDirs.join(",")}`,
      ].join("\n");
      remember(text, out);
      chunk(sessionId, out);
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("echo:")) {
      const out = text.slice(5);
      remember(text, out);
      const mid = Math.max(1, Math.floor(out.length / 2));
      chunk(sessionId, out.slice(0, mid));
      chunk(sessionId, out.slice(mid));
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("long:")) {
      let left = parseInt(text.slice(5), 10) || 0;
      remember(text, "x".repeat(left));
      while (left > 0) {
        const k = Math.min(997, left);
        chunk(sessionId, "x".repeat(k));
        left -= k;
      }
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("sleep:")) {
      const ms = parseInt(text.slice(6), 10) || 0;
      remember(text, "done");
      setTimeout(() => {
        chunk(sessionId, "done");
        result(id, { stopReason: "end_turn" });
      }, ms);
      return;
    }
    if (text === "fail") {
      remember(text, null);
      return rpcErr(id, "fake failure");
    }
    if (text === "crash") {
      remember(text, null);
      process.exit(2);
    }
    if (text === "recall") {
      const prev = state.prompts.slice();
      const out = prev.length ? prev.join("|") : "(empty)";
      remember(text, out);
      chunk(sessionId, out);
      return result(id, { stopReason: "end_turn" });
    }
    remember(text, "ok");
    chunk(sessionId, "ok");
    return result(id, { stopReason: "end_turn" });
  }
  return rpcErr(id, `unknown method ${method}`);
});
