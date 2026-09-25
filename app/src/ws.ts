import {
  ApiError,
  clearSession,
  convs,
  friends as friendsApi,
  msgs,
  session,
  wsBase,
} from "./api";
import {
  applyLiveMessage,
  applyPrefUpdate,
  applyRefreshed,
  applySyncMessages,
  applyUpdatedMessage,
  convTitle,
  dropConversationCache,
  dropOptimistic,
  getState,
  mergeConversationLists,
  pushToast,
  replyPreview,
  resolveAck,
  setState,
  shouldNotify,
} from "./store";
import type { AttachmentView, MessageView, WsOut } from "./types";

let ws: WebSocket | null = null;
let reconnectTimer: number | null = null;
let attempts = 0;
let stopped = false;
let catalogGen = 0;
let wsGen = -1; // 建立连接时的会话代次快照；换代后旧 socket/异步回调一律作废

// ---- 同步水位（P1-9）：以「最高连续 seq」为准，缺口靠 sync 补齐 ----
const syncedSeq = new Map<string, number>(); // conv_id → 已确认连续的最大 seq
const syncInflight = new Set<string>(); // 每会话只允许一个未完成 sync
const readSeq = new Map<string, number>(); // conv_id → 已应用的最大 read_sync seq（P2-4 单调）

type Outgoing = {
  convId: string;
  clientMsgId: string;
  content: string;
  attachment?: AttachmentView;
  replyToId?: string;
  sentOn?: WebSocket;
};

/** 未收到 ack 的发送。重连后按原 client_msg_id 重放，服务端幂等。 */
const outbox: Outgoing[] = [];

const PERMANENT_SEND_ERRORS = new Set([
  "bad_message",
  "forbidden",
  "not_friends",
  "bad_attachment",
  "not_found",
  "bad_frame",
]);

export function stopWs() {
  stopped = true;
  wsGen = -1; // 使在途连接/回调全部失效（P1-10）
  catalogGen++; // 使在途目录刷新失效
  outbox.length = 0;
  // P2-7：清理重连定时器并复位计数/水位，避免新会话继承旧状态
  if (reconnectTimer != null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  attempts = 0;
  syncedSeq.clear();
  syncInflight.clear();
  readSeq.clear();
  if (ws) {
    ws.onclose = null;
    ws.close();
    ws = null;
  }
  setState({ wsReady: false });
}

export async function startWs() {
  stopped = false;
  wsGen = session(); // P1-10：绑定当前登录会话
  await connect();
}

export async function refreshCatalogs(): Promise<void> {
  const gen = ++catalogGen;
  const sess = session();
  const [conversations, friends, requests] = await Promise.all([
    convs.list(),
    friendsApi.list(),
    friendsApi.requests(),
  ]);
  if (gen !== catalogGen || sess !== session()) return;
  setState((s) => {
    let next = { ...s, conversations: mergeConversationLists(s.conversations, conversations), friends, requests };
    for (const c of conversations) {
      if (c.last_message && s.messages[c.id]) next = { ...next, ...applyRefreshed(next, c.id, [c.last_message]) };
    }
    return next;
  });
}

async function connect() {
  if (stopped) return;
  const gen = wsGen;
  let ticket: string;
  try {
    ticket = (await convs.ticket()).ticket;
  } catch {
    if (gen === wsGen) scheduleReconnect(gen);
    return;
  }
  if (stopped || gen !== wsGen || gen !== session()) return; // ticket 落地前会话已换代 → 丢弃
  const sock = new WebSocket(`${wsBase()}/v1/ws?ticket=${ticket}`);
  ws = sock;

  sock.onopen = () => {
    if (gen !== wsGen || gen !== session() || ws !== sock) return;
    attempts = 0;
    setState({ wsReady: true });
    void afterOpen(sock, gen);
  };
  sock.onmessage = (ev) => {
    if (gen !== wsGen || gen !== session() || ws !== sock) return;
    try {
      dispatch(JSON.parse(ev.data) as WsOut);
    } catch (e) {
      console.warn("bad ws frame", e);
    }
  };
  sock.onclose = () => {
    if (gen !== wsGen || gen !== session() || ws !== sock) return;
    setState({ wsReady: false });
    if (!stopped) scheduleReconnect(gen);
  };
  sock.onerror = () => sock.close();
}

async function afterOpen(sock: WebSocket, gen: number) {
  try {
    await refreshCatalogs();
  } catch (e) {
    console.warn("refresh failed", e);
  }
  if (stopped || gen !== wsGen || gen !== session() || ws !== sock || sock.readyState !== WebSocket.OPEN) return;
  const s = getState();
  syncInflight.clear(); // 旧 socket 上的未完成 sync 随连接作废
  for (const conv of new Set([...Object.keys(s.messages), ...syncedSeq.keys()])) {
    const last = syncedSeq.get(conv) ?? contiguousSeq(s.messages[conv] ?? []);
    syncedSeq.set(conv, last);
    sock.send(
      JSON.stringify({
        type: "sync",
        conversation_id: conv,
        last_seq: last,
      }),
    );
    // seq 同步只补新消息；离线期间的编辑/删除/reaction 要靠按 id 回刷缓存补齐
    void refreshCachedMessages(conv, gen);
  }
  flushOutbox();
}

/**
 * 断线恢复第二步：把本地已缓存的 message id 分批（≤100）回读服务端最新状态，
 * 合并正文/墓碑/reaction/回复预览。会话已无权限时清空本地缓存。
 */
async function refreshCachedMessages(convId: string, gen: number) {
  const ids = (getState().messages[convId] ?? [])
    .filter((m) => !m.id.startsWith("client:"))
    .map((m) => m.id);
  for (let i = 0; i < ids.length; i += 100) {
    if (stopped || gen !== wsGen || gen !== session()) return;
    try {
      const p = await msgs.refresh(convId, ids.slice(i, i + 100));
      if (stopped || gen !== wsGen || gen !== session()) return;
      setState((cur) => applyRefreshed(cur, convId, p.messages));
    } catch (e) {
      if (stopped || gen !== wsGen || gen !== session()) return;
      let inaccessible = e instanceof ApiError && e.status === 403;
      if (e instanceof ApiError && e.status === 404) {
        try {
          await convs.detail(convId);
        } catch (detailError) {
          inaccessible = detailError instanceof ApiError && (detailError.status === 403 || detailError.status === 404);
        }
      }
      if (stopped || gen !== wsGen || gen !== session()) return;
      if (inaccessible) setState((cur) => dropConversationCache(cur, convId));
      else if (!(e instanceof ApiError && e.status === 404)) console.warn("message refresh failed", convId, e);
      return;
    }
  }
}

/** 缓存中已确认连续的最高 seq：从最小观测 seq 向上推进，遇到缺口即停（不含乐观消息）。 */
function contiguousSeq(msgs: MessageView[]): number {
  const seqs = msgs
    .filter((m) => !m.id.startsWith("client:"))
    .map((m) => m.seq)
    .sort((a, b) => a - b);
  if (!seqs.length) return 0;
  let w = seqs[0];
  for (const seq of seqs) {
    if (seq > w + 1) break;
    if (seq > w) w = seq;
  }
  return w;
}

/**
 * 实时消息计入连续水位（P1-9）：
 * - seq <= 水位：已覆盖，忽略；
 * - seq == 水位+1 或无基线（水位 0）：推进水位；
 * - seq 跳过水位：有缺口 → 发 sync 从水位处补齐。
 */
function trackIncomingSeq(conv: string, seq: number) {
  let wm = syncedSeq.get(conv);
  if (wm === undefined) {
    wm = contiguousSeq(getState().messages[conv] ?? []);
    syncedSeq.set(conv, wm);
  }
  if (seq <= wm) return;
  if (seq === wm + 1 || wm === 0) {
    syncedSeq.set(conv, seq);
  } else {
    requestSync(conv, wm);
  }
}

/** 从连续水位发起 sync（防抖：同会话只允许一个未完成请求） */
function requestSync(conv: string, lastSeq: number) {
  if (syncInflight.has(conv)) return;
  if (ws?.readyState !== WebSocket.OPEN) return;
  syncInflight.add(conv);
  wsSend({ type: "sync", conversation_id: conv, last_seq: lastSeq });
}

function scheduleReconnect(gen: number) {
  if (stopped || reconnectTimer != null) return;
  const delay = Math.min(15000, 1000 * 2 ** attempts++);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    if (stopped || gen !== wsGen || gen !== session()) return; // P2-7/P1-10：换代后的定时器不动作
    void connect();
  }, delay);
}

function dispatch(f: WsOut) {
  const s = getState();
  switch (f.type) {
    case "presence_init": {
      const online: Record<string, boolean> = {};
      f.online.forEach((u) => (online[u] = true));
      setState({ online });
      break;
    }
    case "presence":
      setState({
        online: { ...s.online, [f.user_id]: f.online },
        // 同步 DM 会话里 peer 的 last_seen_at（聊天头「最后在线」）
        conversations: f.last_seen_at
          ? s.conversations.map((c) =>
              c.peer?.id === f.user_id
                ? { ...c, peer: { ...c.peer, last_seen_at: f.last_seen_at } }
                : c,
            )
          : s.conversations,
      });
      break;
    case "message":
      onMessage(f.message);
      trackIncomingSeq(f.message.conversation_id, f.message.seq);
      break;
    case "message_ack":
      takeOutbox(f.client_msg_id);
      setState((cur) => {
        const merged = { ...cur, ...applySyncMessages(cur, f.message.conversation_id, [f.message]) };
        return { ...merged, messages: { ...merged.messages,
          [f.message.conversation_id]: resolveAck(merged.messages[f.message.conversation_id] ?? [], f.client_msg_id, f.message),
        } };
      });
      trackIncomingSeq(f.message.conversation_id, f.message.seq);
      break;
    case "sync_messages": {
      setState((cur) => applySyncMessages(cur, f.conversation_id, f.messages));
      // 服务端按 last_seq 之后连续有序返回 → 本页末尾即新水位
      const lastMsg = f.messages[f.messages.length - 1];
      if (lastMsg) {
        syncedSeq.set(
          f.conversation_id,
          Math.max(syncedSeq.get(f.conversation_id) ?? 0, lastMsg.seq),
        );
      }
      // P2-3：服务端分页（>500 条）→ 按新水位续拉；无 has_more 字段的旧服务端视为完结
      if (f.has_more && lastMsg) {
        syncInflight.delete(f.conversation_id);
        requestSync(f.conversation_id, lastMsg.seq);
      } else {
        syncInflight.delete(f.conversation_id);
      }
      break;
    }
    case "typing":
      setState({
        typing: {
          ...s.typing,
          [f.conversation_id]: { user: f.user, ts: Date.now() },
        },
      });
      break;
    case "read_sync": {
      // P2-4：单调推进——迟到的旧事件不得回退已读位置、清掉新未读
      if (f.seq <= (readSeq.get(f.conversation_id) ?? -1)) break;
      readSeq.set(f.conversation_id, f.seq);
      let incomplete = false;
      setState((cur) => ({
        conversations: cur.conversations.map((c) => {
          if (c.id !== f.conversation_id) return c;
          const unread = Math.min(c.unread_count, Math.max(0, c.last_seq - f.seq));
          const remaining = (cur.messages[c.id] ?? []).filter((m) =>
            !m.id.startsWith("client:") && m.seq > c.last_seq - unread && m.seq <= c.last_seq);
          const complete = remaining.length === unread;
          if (!complete) incomplete = true;
          return { ...c, unread_count: unread, has_mention: unread > 0 &&
            (remaining.some((m) => !m.deleted_at && m.mentions.includes(cur.me?.id ?? "")) || (!complete && c.has_mention)) };
        }),
      }));
      if (incomplete) void refreshCatalogs().catch((e) => console.warn("read refresh failed", e));
      break;
    }
    // 所有生命周期帧都携带完整视图（reactions 已个性化），统一整视图版本合并
    case "message_updated":
    case "message_deleted":
    case "reaction_update":
      setState((cur) => applyUpdatedMessage(cur, f.message));
      break;
    case "op_ack":
      // mutation 经 REST 提交（UI 用 await 拿错误）；WS op 回执此处无 outbox 可清
      break;
    case "preference_updated": {
      // 偏好只发本人连接：直接按 conversation_id 合并；本地不认识该会话时拉目录兜底
      const knownConv = s.conversations.some((c) => c.id === f.conversation_id);
      setState((cur) => applyPrefUpdate(cur, f.conversation_id, f.preference));
      if (!knownConv)
        void refreshCatalogs().catch((e) => console.warn("refresh failed", e));
      break;
    }
    case "invalidate":
      void refreshByWhat(f.what);
      break;
    case "force_logout": {
      // 被管理员禁用/重置密码：记录原因 → 停连接 → 回登录页
      sessionStorage.setItem(
        "chat_kick_reason",
        f.reason === "disabled" ? "账号已被禁用，请联系管理员" : "密码已被管理员重置，请重新登录",
      );
      stopWs();
      clearSession();
      window.dispatchEvent(new Event("chat:logout"));
      break;
    }
    case "error":
      // client_op_id 只关联非发送类 mutation，绝不能消费发送 outbox
      if (f.client_msg_id && !f.client_op_id && PERMANENT_SEND_ERRORS.has(f.code)) {
        const item = takeOutbox(f.client_msg_id);
        if (item) setState((cur) => dropOptimistic(cur, item.convId, item.clientMsgId));
      }
      console.warn("ws error", f.code, f.message);
      break;
  }
}

function onMessage(m: MessageView) {
  const known = getState().conversations.some((c) => c.id === m.conversation_id);
  const s = getState();
  const isActive = s.activeConv === m.conversation_id && s.tab === "chats";
  const mine = m.sender_id === s.me?.id;
  setState((cur) => applyLiveMessage(cur, m));
  if (!known) void refreshCatalogs().catch((e) => console.warn("refresh failed", e));
  // 当前打开的会话收到别人消息 → 上报已读
  if (isActive && !mine) sendRead(m.conversation_id, m.seq);
  // 最小应用内提示：静音会话被抑制（@我 除外）；点击打开会话
  const cur = getState();
  const existed = (s.messages[m.conversation_id] ?? []).some((x) => x.id === m.id)
    || s.conversations.some((c) => c.last_message?.id === m.id);
  if (!existed && shouldNotify(cur, m)) {
    const c = cur.conversations.find((x) => x.id === m.conversation_id);
    const excerpt = m.attachment
      ? m.kind === "image"
        ? "[图片]"
        : `[文件] ${m.attachment.file_name}`
      : m.content.slice(0, 80);
    const tag = cur.me && m.mentions.includes(cur.me.id) ? "[有人@我] " : "";
    pushToast({
      convId: m.conversation_id,
      title: c ? convTitle(c) : "新消息",
      text: `${tag}${m.sender_name}: ${excerpt}`,
    });
  }
}

async function refreshByWhat(what: string) {
  try {
    if (what === "conversations" || what === "friends" || what === "requests") {
      await refreshCatalogs();
    }
  } catch (e) {
    console.warn("refresh failed", e);
  }
}

function takeOutbox(clientMsgId: string | null): Outgoing | undefined {
  if (!clientMsgId) return;
  const i = outbox.findIndex((item) => item.clientMsgId === clientMsgId);
  if (i < 0) return;
  return outbox.splice(i, 1)[0];
}

function flushOutbox() {
  if (wsGen !== session() || ws?.readyState !== WebSocket.OPEN) return;
  for (const item of outbox) {
    if (item.sentOn === ws) continue;
    ws.send(
      JSON.stringify({
        type: "send_message",
        conversation_id: item.convId,
        client_msg_id: item.clientMsgId,
        content: item.content,
        attachment_id: item.attachment?.id,
        reply_to_id: item.replyToId,
      }),
    );
    item.sentOn = ws;
  }
}

// ---------- 发送 ----------
export function wsSend(obj: unknown) {
  if (wsGen === session() && ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
}

export function sendChat(
  convId: string,
  content: string,
  attachment?: AttachmentView,
  replyTo?: MessageView,
) {
  const clientMsgId = crypto.randomUUID();
  const me = getState().me;
  if (!me) return;
  const optimistic: MessageView = {
    id: `client:${clientMsgId}`,
    conversation_id: convId,
    seq: Number.MAX_SAFE_INTEGER - Math.floor(Math.random() * 1e6),
    sender_id: me.id,
    sender_username: me.username,
    sender_name: me.display_name,
    content:
      content ||
      (attachment
        ? attachment.mime.startsWith("image/")
          ? "[图片]"
          : `[文件] ${attachment.file_name}`
        : ""),
    kind: attachment ? (attachment.mime.startsWith("image/") ? "image" : "file") : "text",
    attachment: attachment ?? null,
    mentions: [],
    reply_to_id: replyTo?.id ?? null,
    reply_to_preview: replyTo ? replyPreview(replyTo) : null,
    edited_at: null,
    edit_version: 0,
    deleted_at: null,
    reactions: [],
    created_at: new Date().toISOString(),
  };
  outbox.push({ convId, clientMsgId, content, attachment, replyToId: replyTo?.id });
  setState((s) => ({
    messages: {
      ...s.messages,
      [convId]: [...(s.messages[convId] ?? []), optimistic],
    },
  }));
  flushOutbox();
}

export function sendTyping(convId: string) {
  wsSend({ type: "typing", conversation_id: convId });
}

export function sendRead(convId: string, seq: number) {
  wsSend({ type: "read", conversation_id: convId, seq });
}

/**
 * 打开会话：切到聊天页、上报已读、本地清未读；
 * 归档会话被主动打开时即取消本人归档（§8.3，新普通消息不自动恢复）。
 */
export function openConversation(id: string) {
  const s = getState();
  const c = s.conversations.find((x) => x.id === id);
  setState({ activeConv: id, tab: "chats" });
  if (c && c.unread_count > 0 && c.last_seq > 0) {
    sendRead(id, c.last_seq);
    setState((cur) => ({
      conversations: cur.conversations.map((x) =>
        x.id === id ? { ...x, unread_count: 0, has_mention: false } : x,
      ),
    }));
  }
  if (c?.preference?.archived_at) {
    const gen = session();
    void convs
      .setPref(id, { archived: false })
      .then((pref) => { if (gen === session()) setState((cur) => applyPrefUpdate(cur, id, pref)); })
      .catch((e) => console.warn("unarchive failed", e));
  }
}
