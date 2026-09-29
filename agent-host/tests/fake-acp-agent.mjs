// 假 ACP agent（NDJSON over stdio），供 agent-host 与 server 集成测试使用。
// 启动参数：
//   --mcp=http   initialize 里声明 agentCapabilities.mcpCapabilities.http
//   --mcp=stdio  不声明 http 能力（宿主改用 stdio 桥接子进程注入）
// prompt 行为：
//   echo:<text>  → 至少 2 个 agent_message_chunk 分块，合计恰为 <text>
//   long:<n>     → 总分块长度 = n（单块 ≤ 997 字符，强迫多块）
//   sleep:<ms>   → 等 ms 毫秒后回 "done"
//   steps:<n>    → 依次上报 n 个 tool_call 步骤（每个 tool_call + 文本 chunk +
//                  tool_call_update completed 穿插），最后回出 "done"
//   stepfail     → 一个步骤以 status=failed 结束，然后正常回复 "done"
//   drip:<ms>    → 每 ~30ms 回一个小 chunk，持续 ms 毫秒（服务端限频验证用），最后回 "done"
//   fail         → JSON-RPC error
//   crash        → 直接退出（进程码 2）
//   recall       → 本会话之前收到过的全部指令，用 | 连接；没有则 (empty)
//   whoami       → 当前 mode / model、环境变量 FAKE_ECHO、第一个文本块、额外目录
//   tool:read_history  → 经注入的 MCP 服务 tools/call read_conversation_history，原样回出文本
//   call:<名字>:<指令> → 同步调用主人的该 Agent，回出结果正文
//   callasync:<名1>,<名2>:<指令> → 并行派出后逐个等待，结果用 | 连接
//   callctx:<名字>:<上下文号|new>:<指令> → 带 / 不带上下文号调用，回出 ctx=<号> result=<正文>
//   tool:list / tool:search:<关键词> / tool:read:<会话id> /
//   tool:send:<会话id>:<正文> / tool:sendfile:<会话id>:<文件名>:<内容>
//     → 调 list_conversations / search_messages / read_conversation / send_message，回出工具文本
//       （sendfile 先把内容写进临时文件，再经 file_paths 发送）
//   perm:<kind>:<路径> → session/request_permission（kind + locations，选项 allow/always/reject）。
//                        选定 allow 类回 allowed:<optionId>，拒绝或取消回 rejected
//   inspect       → 回出收到的每个非文本块（image:<mime>:<字节数> / link:<name>:<存在与否>），
//                   无非文本块则 (no attachments)
//   makefile:<名>:<内容> → 写进 CHAT_AGENT_OUTBOX 后回 "ok"；makebig:<名>:<字节数> 写零字节文件
//   其他          → 回 "ok"
//   --no-image    → initialize 不声明 promptCapabilities.image（图片退化为 resource_link）
//
// 会话跨进程保留：状态写在临时目录 chat-fake-acp/<sessionId>.json。
// 启动参数 --caps=resume|load|none（默认 resume）决定 initialize 声明的能力。
// load 模式恢复时先用 session/update 回放已存的回复正文，再返回 session/load 结果。
// 指令取 prompt 的最后一个文本块（附加指令放在前面的块里）。
// session/new 报告两个 mode、两个 model；支持 session/set_mode / session/set_model。
// session/cancel（通知）：进行中的 sleep 立刻以 stopReason "cancelled" 返回。
// 启动参数 --banner=before：session/new 响应之前先发一段 BANNER 消息（启动信息不算回复）。
// 启动参数 --banner=pi：像 pi-acp 那样把启动信息放进 _meta.fakeAcp.startupInfo，
//   并在下一个 session/prompt 开头才补发同一段文字（模拟定时器晚于 prompt 到达）。
// 启动参数 --ignore-cancel：忽略取消，用来测宿主 5 秒后强制结束子进程。
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { spawn } from "node:child_process";

const capArg = process.argv.find((a) => a.startsWith("--caps="));
const caps = capArg ? capArg.slice("--caps=".length) : "resume";
const dir = path.join(os.tmpdir(), "chat-fake-acp");
const ignoreCancel = process.argv.includes("--ignore-cancel");
const noImage = process.argv.includes("--no-image");
const bannerArg = process.argv.find((a) => a.startsWith("--banner="));
const banner = bannerArg ? bannerArg.slice("--banner=".length) : "";
const STARTUP = "## Extensions\n- [object Object]\n- npm:pi-btw\n  - index.ts\n";
let startupPending = false;
// 宿主在 prompt 末尾追加的 outbox 提示块（固定前缀），不是用户指令
const HINT = "如需把文件发给用户";

/// file:///… → 本地路径（Windows 的 /C:/x 去掉前导斜杠）
function uriToPath(u) {
  if (typeof u !== "string" || !u.startsWith("file://")) return null;
  let p = decodeURIComponent(u.slice(7));
  if (/^\/[A-Za-z]:\//.test(p)) p = p.slice(1);
  return p;
}

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
// A03：步骤上报 = tool_call（开始）+ tool_call_update（终态）
const toolCall = (sessionId, toolCallId, title, kind) =>
  send({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId,
      update: { sessionUpdate: "tool_call", toolCallId, title, kind },
    },
  });
const toolCallUpdate = (sessionId, toolCallId, status) =>
  send({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId,
      update: { sessionUpdate: "tool_call_update", toolCallId, status },
    },
  });

let sessionId = null;
let state = { prompts: [], replies: [] };
let currentMode = "ask";
let currentModel = "fast";
let additionalDirs = [];
let sessionCwd = "";

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
let pendingSleep = null;
// A12：session/request_permission 的应答还没回来时，prompt 先挂起。
let permSeq = 0;
let pendingPerm = null;
const permOptions = () => [
  { optionId: "allow", name: "Allow once", kind: "allow_once" },
  { optionId: "always", name: "Always allow", kind: "allow_always" },
  { optionId: "reject", name: "Reject", kind: "reject_once" },
];
const mcpMode = (process.argv.find((a) => a.startsWith("--mcp=")) || "").slice(6);
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

async function runMcpTool(name, args) {
  await mcpCall("initialize", {
    protocolVersion: "2025-03-26",
    capabilities: {},
    clientInfo: { name: "fake-acp-agent", version: "0" },
  });
  await mcpCall("notifications/initialized", {}, false);
  const r = await mcpCall("tools/call", { name, arguments: args });
  const content = r?.result?.content;
  const text = Array.isArray(content) ? content.map((c) => c.text ?? "").join("\n") : JSON.stringify(r);
  if (r?.result?.isError) throw new Error(text);
  return text;
}

function invocationIdOf(text) {
  const m = /^invocation_id=([0-9a-fA-F-]+)/m.exec(text || "");
  return m ? m[1] : "";
}
function contextIdOf(text) {
  const m = /^context_id=([0-9a-fA-F-]+)/m.exec(text || "");
  return m ? m[1] : "";
}
function resultBody(text) {
  const mark = "result:\n";
  const i = (text || "").indexOf(mark);
  if (i < 0) return (text || "").trim();
  return text.slice(i + mark.length).replace(/\n$/, "");
}
function replyTool(promptId, promise) {
  promise
    .then((out) => {
      chunk(sessionId, out || "(empty)");
      result(promptId, { stopReason: "end_turn" });
    })
    .catch((e) => {
      chunk(sessionId, `tool error: ${e?.message || e}`);
      result(promptId, { stopReason: "end_turn" });
    });
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
  // 权限请求的响应（id 是我们发出去的，没有 method）
  if (pendingPerm && id === pendingPerm.reqId) {
    const { promptId, options, text } = pendingPerm;
    pendingPerm = null;
    const outcome = m.result?.outcome;
    let reply = "rejected";
    if (outcome && outcome.outcome === "selected") {
      const opt = options.find((o) => o.optionId === outcome.optionId);
      const kind = opt?.kind || "";
      reply = String(kind).includes("reject") ? "rejected" : `allowed:${outcome.optionId}`;
    }
    remember(text, reply);
    chunk(sessionId, reply);
    return result(promptId, { stopReason: "end_turn" });
  }
  if (method === "session/cancel") {
    if (ignoreCancel || !pendingSleep) return;
    clearTimeout(pendingSleep.timer);
    const promptId = pendingSleep.id;
    pendingSleep = null;
    return result(promptId, { stopReason: "cancelled" });
  }
  if (id === undefined) return; // 其余通知不回

  if (method === "initialize") {
    const agentCapabilities = { sessionCapabilities: { additionalDirectories: {} } };
    if (!noImage) agentCapabilities.promptCapabilities = { image: true };
    if (caps === "resume") agentCapabilities.sessionCapabilities.resume = {};
    else if (caps === "load") agentCapabilities.loadSession = true;
    // --mcp=http 声明支持 http 注入；否则只支持 stdio
    if (mcpMode === "http") agentCapabilities.mcpCapabilities = { http: true };
    return result(id, { protocolVersion: 1, agentCapabilities, authMethods: [] });
  }
  if (method === "session/new") {
    sessionId = crypto.randomUUID();
    mcpServers = params?.mcpServers ?? [];
    state = { prompts: [], replies: [] };
    saveState(sessionId, state);
    currentMode = "ask";
    currentModel = "fast";
    additionalDirs = Array.isArray(params?.additionalDirectories) ? params.additionalDirectories : [];
    sessionCwd = params?.cwd ?? "";
    if (banner === "before") chunk(sessionId, "BANNER-BEFORE\n");
    startupPending = banner === "pi";
    return result(id, {
      sessionId,
      modes: { currentModeId: currentMode, availableModes: MODES },
      models: { currentModelId: currentModel, availableModels: MODELS },
      ...(banner === "pi" ? { _meta: { fakeAcp: { startupInfo: STARTUP } } } : {}),
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
    sessionCwd = params?.cwd ?? "";
    mcpServers = params?.mcpServers ?? [];
    return result(id, { sessionId });
  }
  if (method === "session/load") {
    if (caps !== "load") return rpcErr(id, "load unsupported");
    if (!openSaved(params?.sessionId)) return rpcErr(id, "unknown session");
    sessionCwd = params?.cwd ?? "";
    mcpServers = params?.mcpServers ?? [];
    // 回放必须发生在 session/load 的响应之前，宿主用这一点把历史和本次回复分开
    for (const reply of state.replies) {
      if (reply) chunk(sessionId, reply);
    }
    return result(id, { sessionId });
  }
  if (method === "session/prompt") {
    if (startupPending) {
      startupPending = false;
      chunk(sessionId, STARTUP);
    }
    const raw = params?.prompt ?? [];
    const blocks = raw.map((p) => (typeof p?.text === "string" ? p.text : ""));
    // 最后一个文本块可能是宿主的 outbox 提示，不是指令——去掉再取
    const cmds = blocks.filter((t) => !t.startsWith(HINT));
    const text = cmds[cmds.length - 1] ?? "";
    if (text.trim() === "whoami") {
      const out = [
        `mode=${currentMode}`,
        `model=${currentModel}`,
        `FAKE_ECHO=${process.env.FAKE_ECHO ?? ""}`,
        `first=${blocks[0] ?? ""}`,
        `dirs=${additionalDirs.join(",")}`,
        `cwd=${sessionCwd}`,
      ].join("\n");
      remember(text, out);
      chunk(sessionId, out);
      return result(id, { stopReason: "end_turn" });
    }
    const perm = /^perm:([^:]+):(.*)$/.exec(text);
    if (perm) {
      const kind = perm[1];
      const path = perm[2];
      const options = permOptions();
      const reqId = ++permSeq;
      pendingPerm = { reqId, promptId: id, options, text };
      return send({
        jsonrpc: "2.0",
        id: reqId,
        method: "session/request_permission",
        params: {
          sessionId,
          toolCall: {
            toolCallId: `perm-${reqId}`,
            title: path,
            kind,
            locations: path ? [{ path }] : [],
          },
          options,
        },
      });
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
      const timer = setTimeout(() => {
        pendingSleep = null;
        chunk(sessionId, "done");
        result(id, { stopReason: "end_turn" });
      }, ms);
      pendingSleep = { id, timer };
      return;
    }
    // A03：N 个步骤，穿插文本 chunk，最后回出 done
    if (text.startsWith("steps:")) {
      const n = parseInt(text.slice(6), 10) || 0;
      for (let i = 1; i <= n; i++) {
        const tid = `step-${i}`;
        toolCall(sessionId, tid, `步骤 ${i}`, "other");
        chunk(sessionId, `第 ${i} 步。\n`);
        toolCallUpdate(sessionId, tid, "completed");
      }
      chunk(sessionId, "done");
      return result(id, { stopReason: "end_turn" });
    }
    // A03：一个步骤以 failed 结束，调用本身仍正常结束
    if (text === "stepfail") {
      toolCall(sessionId, "step-x", "读取配置", "read");
      chunk(sessionId, "步骤失败但继续。\n");
      toolCallUpdate(sessionId, "step-x", "failed");
      chunk(sessionId, "done");
      return result(id, { stopReason: "end_turn" });
    }
    // A03 限频验证：持续 ms 毫秒、每 ~30ms 一个小 chunk，最后回 done
    if (text.startsWith("drip:")) {
      const ms = parseInt(text.slice(5), 10) || 0;
      const started = Date.now();
      const timer = setInterval(() => {
        chunk(sessionId, ".");
        if (Date.now() - started >= ms) {
          clearInterval(timer);
          chunk(sessionId, "done");
          result(id, { stopReason: "end_turn" });
        }
      }, 30);
      return;
    }
    // A16：搜索 / 读取其他会话、以 Agent 身份发消息（含本地文件）
    if (text === "tool:list") {
      replyTool(id, runMcpTool("list_conversations", {}));
      return;
    }
    if (text.startsWith("tool:search:")) {
      replyTool(id, runMcpTool("search_messages", { query: text.slice("tool:search:".length) }));
      return;
    }
    if (text.startsWith("tool:read:")) {
      replyTool(
        id,
        runMcpTool("read_conversation", { conversation_id: text.slice("tool:read:".length) }),
      );
      return;
    }
    if (text.startsWith("tool:sendfile:")) {
      const m = /^([^:]+):([^:]+):([\s\S]*)$/.exec(text.slice("tool:sendfile:".length));
      if (!m) {
        chunk(sessionId, "tool error: bad sendfile");
        return result(id, { stopReason: "end_turn" });
      }
      const dir = fs.mkdtempSync(path.join(os.tmpdir(), "a16-"));
      const fp = path.join(dir, path.basename(m[2]));
      fs.writeFileSync(fp, m[3]);
      replyTool(
        id,
        runMcpTool("send_message", {
          conversation_id: m[1],
          text: "",
          file_paths: [fp],
        }).finally(() => {
          try {
            fs.rmSync(dir, { recursive: true, force: true });
          } catch {
            /* 临时目录清不掉不影响工具结果 */
          }
        }),
      );
      return;
    }
    if (text.startsWith("tool:send:")) {
      const rest = text.slice("tool:send:".length);
      const i = rest.indexOf(":");
      if (i <= 0) {
        chunk(sessionId, "tool error: bad send");
        return result(id, { stopReason: "end_turn" });
      }
      replyTool(
        id,
        runMcpTool("send_message", {
          conversation_id: rest.slice(0, i),
          text: rest.slice(i + 1),
        }),
      );
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
    if (text === "tool:create_schedule") {
      runMcpTool("create_schedule", {
        name: "from-agent",
        prompt: "echo:from-schedule",
        spec: { type: "interval", minutes: 30 },
        timezone: "UTC",
      })
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
    if (text === "inspect") {
      // 回报每个非文本块：image:<mime>:<解码字节数> / link:<name>:<目标存在与否>
      const parts = [];
      for (const p of raw) {
        if (p?.type === "image") {
          const n = Buffer.from(p.data ?? "", "base64").length;
          parts.push(`image:${p.mimeType ?? ""}:${n}`);
        } else if (p?.type === "resource_link") {
          const fp = uriToPath(p.uri);
          parts.push(`link:${p.name ?? ""}:${fp ? fs.existsSync(fp) : false}`);
        }
      }
      const out = parts.length ? parts.join("|") : "(no attachments)";
      remember(text, out);
      chunk(sessionId, out);
      return result(id, { stopReason: "end_turn" });
    }
    if (text.startsWith("makefile:") || text.startsWith("makebig:")) {
      // makefile:<名>:<内容> / makebig:<名>:<字节数> → 写进 CHAT_AGENT_OUTBOX
      const isBig = text.startsWith("makebig:");
      const rest = text.slice(isBig ? 8 : 9);
      const sep = rest.indexOf(":");
      const name = path.basename(rest.slice(0, sep >= 0 ? sep : rest.length));
      const body = sep >= 0 ? rest.slice(sep + 1) : "";
      const outDir = process.env.CHAT_AGENT_OUTBOX;
      if (!outDir) {
        remember(text, "no outbox");
        chunk(sessionId, "no outbox");
        return result(id, { stopReason: "end_turn" });
      }
      try {
        fs.mkdirSync(outDir, { recursive: true });
        const data = isBig ? Buffer.alloc(parseInt(body, 10) || 0) : Buffer.from(body);
        fs.writeFileSync(path.join(outDir, name), data);
        remember(text, "ok");
        chunk(sessionId, "ok");
      } catch (e) {
        remember(text, `write failed: ${e?.message || e}`);
        chunk(sessionId, `write failed: ${e?.message || e}`);
      }
      return result(id, { stopReason: "end_turn" });
    }
    // A14：互调。指令里可以再带冒号（echo:hi、call:c:…），只按前两段切开。
    if (text.startsWith("callasync:") || text.startsWith("callctx:") || text.startsWith("call:")) {
      const promptId = id;
      const promptText = text;
      (async () => {
        if (promptText.startsWith("callasync:")) {
          const rest = promptText.slice("callasync:".length);
          const cut = rest.indexOf(":");
          const names = rest.slice(0, cut).split(",").map((s) => s.trim()).filter(Boolean);
          const instruction = rest.slice(cut + 1);
          const created = await Promise.all(
            names.map((name) => runMcpTool("call_agent", { name, prompt: instruction, wait: false })),
          );
          const parts = [];
          for (const c of created) {
            const waited = await runMcpTool("wait_invocation", {
              invocation_id: invocationIdOf(c),
              timeout_secs: 600,
            });
            parts.push(resultBody(waited));
          }
          return parts.join("|");
        }
        if (promptText.startsWith("callctx:")) {
          const rest = promptText.slice("callctx:".length);
          const c1 = rest.indexOf(":");
          const name = rest.slice(0, c1);
          const rest2 = rest.slice(c1 + 1);
          const c2 = rest2.indexOf(":");
          const ctxTok = rest2.slice(0, c2);
          const instruction = rest2.slice(c2 + 1);
          const args = { name, prompt: instruction, wait: true };
          if (ctxTok !== "new") args.context_id = ctxTok;
          const out = await runMcpTool("call_agent", args);
          return `ctx=${contextIdOf(out)} result=${resultBody(out)}`;
        }
        const rest = promptText.slice("call:".length);
        const cut = rest.indexOf(":");
        const name = rest.slice(0, cut);
        const instruction = rest.slice(cut + 1);
        const out = await runMcpTool("call_agent", { name, prompt: instruction, wait: true });
        return resultBody(out);
      })()
        .then((out) => {
          remember(promptText, out);
          chunk(sessionId, out);
          result(promptId, { stopReason: "end_turn" });
        })
        .catch((e) => {
          const out = `tool error: ${e?.message || e}`;
          chunk(sessionId, out);
          result(promptId, { stopReason: "end_turn" });
        });
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
