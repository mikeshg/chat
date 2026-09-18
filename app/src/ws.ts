import { convs, friends as friendsApi } from "./api";
import { getState, setState, upsertMessage, resolveAck } from "./store";
import type { AttachmentView, MessageView, WsOut } from "./types";
import { wsBase } from "./api";

let ws: WebSocket | null = null;
let reconnectTimer: number | null = null;
let attempts = 0;
let stopped = false;

export function stopWs() {
  stopped = true;
  if (reconnectTimer) clearTimeout(reconnectTimer);
  ws?.close();
  ws = null;
  setState({ wsReady: false });
}

export async function startWs() {
  stopped = false;
  await connect();
}

async function connect() {
  if (stopped) return;
  let ticket: string;
  try {
    ticket = (await convs.ticket()).ticket;
  } catch {
    scheduleReconnect();
    return;
  }
  const sock = new WebSocket(`${wsBase()}/v1/ws?ticket=${ticket}`);
  ws = sock;

  sock.onopen = () => {
    attempts = 0;
    setState({ wsReady: true });
    // 重连后补齐：本地每个会话按 last_seq 拉增量
    for (const [conv, msgs] of Object.entries(getState().messages)) {
      const last = msgs.filter((m) => !m.id.startsWith("client:")).pop();
      sock.send(
        JSON.stringify({ type: "sync", conversation_id: conv, last_seq: last?.seq ?? 0 }),
      );
    }
  };
  sock.onmessage = (ev) => {
    try {
      dispatch(JSON.parse(ev.data) as WsOut);
    } catch (e) {
      console.warn("bad ws frame", e);
    }
  };
  sock.onclose = () => {
    setState({ wsReady: false });
    if (!stopped) scheduleReconnect();
  };
  sock.onerror = () => sock.close();
}

function scheduleReconnect() {
  if (stopped || reconnectTimer) return;
  const delay = Math.min(15000, 1000 * 2 ** attempts++);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
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
      break;
    case "message_ack":
      setState({
        messages: {
          ...s.messages,
          [f.message.conversation_id]: resolveAck(
            s.messages[f.message.conversation_id] ?? [],
            f.client_msg_id,
            f.message,
          ),
        },
      });
      break;
    case "sync_messages":
      setState({
        messages: {
          ...s.messages,
          [f.conversation_id]: f.messages.reduce(
            (l, m) => upsertMessage(l, m),
            s.messages[f.conversation_id] ?? [],
          ),
        },
      });
      break;
    case "typing":
      setState({
        typing: {
          ...s.typing,
          [f.conversation_id]: { user: f.user, ts: Date.now() },
        },
      });
      break;
    case "read_sync":
      setState({
        conversations: s.conversations.map((c) =>
          c.id === f.conversation_id ? { ...c, unread_count: 0, has_mention: false } : c,
        ),
      });
      break;
    case "invalidate":
      void refreshByWhat(f.what);
      break;
    case "error":
      console.warn("ws error", f.code, f.message);
      break;
  }
}

function onMessage(m: MessageView) {
  const s = getState();
  const list = s.messages[m.conversation_id] ?? [];
  const isActive = s.activeConv === m.conversation_id && s.tab === "chats";
  const mine = m.sender_id === s.me?.id;
  const { [m.conversation_id]: _t, ...typing } = s.typing;

  setState({
    messages: { ...s.messages, [m.conversation_id]: upsertMessage(list, m) },
    conversations: s.conversations
      .map((c) =>
        c.id === m.conversation_id
          ? {
              ...c,
              last_message: m,
              last_seq: Math.max(c.last_seq, m.seq),
              unread_count: mine || isActive ? c.unread_count : c.unread_count + 1,
              has_mention:
                c.has_mention ||
                (!mine && !isActive && s.me != null && m.mentions.includes(s.me.id)),
            }
          : c,
      )
      .sort((a, b) =>
        (b.last_message?.created_at ?? b.created_at).localeCompare(
          a.last_message?.created_at ?? a.created_at,
        ),
      ),
    // 新消息到达即清掉该会话的 typing 状态
    typing,
  });
  // 当前打开的会话收到别人消息 → 上报已读
  if (isActive && !mine) sendRead(m.conversation_id, m.seq);
}

async function refreshByWhat(what: string) {
  try {
    if (what === "conversations") {
      setState({ conversations: await convs.list() });
    } else if (what === "friends") {
      setState({
        friends: await friendsApi.list(),
        conversations: await convs.list(),
      });
    } else if (what === "requests") {
      setState({ requests: await friendsApi.requests() });
    }
  } catch (e) {
    console.warn("refresh failed", e);
  }
}

// ---------- 发送 ----------
export function wsSend(obj: unknown) {
  if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
}

export function sendChat(convId: string, content: string, attachment?: AttachmentView) {
  const clientMsgId = crypto.randomUUID();
  const s = getState();
  const optimistic: MessageView = {
    id: `client:${clientMsgId}`,
    conversation_id: convId,
    seq: Number.MAX_SAFE_INTEGER - Math.floor(Math.random() * 1e6),
    sender_id: s.me!.id,
    sender_username: s.me!.username,
    sender_name: s.me!.display_name,
    content: content || (attachment ? (attachment.mime.startsWith("image/") ? "[图片]" : `[文件] ${attachment.file_name}`) : ""),
    kind: attachment ? (attachment.mime.startsWith("image/") ? "image" : "file") : "text",
    attachment: attachment ?? null,
    mentions: [],
    created_at: new Date().toISOString(),
  };
  setState({
    messages: {
      ...s.messages,
      [convId]: [...(s.messages[convId] ?? []), optimistic],
    },
  });
  wsSend({
    type: "send_message",
    conversation_id: convId,
    client_msg_id: clientMsgId,
    content,
    attachment_id: attachment?.id,
  });
}

export function sendTyping(convId: string) {
  wsSend({ type: "typing", conversation_id: convId });
}

export function sendRead(convId: string, seq: number) {
  wsSend({ type: "read", conversation_id: convId, seq });
}
