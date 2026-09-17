import { useSyncExternalStore } from "react";
import type {
  ConversationView,
  FriendView,
  MessageView,
  RequestsView,
  UserPublic,
} from "./types";

export interface AppState {
  me: UserPublic | null;
  conversations: ConversationView[];
  /** conv_id -> 按 seq 升序的消息（含乐观未确认消息，pending 用 client: 前缀 id） */
  messages: Record<string, MessageView[]>;
  hasMore: Record<string, boolean>;
  activeConv: string | null;
  online: Record<string, boolean>;
  /** conv_id -> { user, ts } 正在输入 */
  typing: Record<string, { user: UserPublic; ts: number }>;
  friends: FriendView[];
  requests: RequestsView;
  wsReady: boolean;
  tab: "chats" | "contacts" | "me";
}

let state: AppState = {
  me: null,
  conversations: [],
  messages: {},
  hasMore: {},
  activeConv: null,
  online: {},
  typing: {},
  friends: [],
  requests: { incoming: [], outgoing: [] },
  wsReady: false,
  tab: "chats",
};

const listeners = new Set<() => void>();

export function getState() {
  return state;
}

export function setState(patch: Partial<AppState> | ((s: AppState) => Partial<AppState>)) {
  const p = typeof patch === "function" ? patch(state) : patch;
  state = { ...state, ...p };
  listeners.forEach((l) => l());
}

export function useApp(): AppState {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => state,
    () => state,
  );
}

// ---------- 领域辅助 ----------

export function convTitle(c: ConversationView, meId?: string): string {
  if (c.kind === "group") return c.name ?? `群聊(${c.member_count})`;
  return c.peer?.display_name ?? c.name ?? "会话";
}

export function convAvatarText(c: ConversationView): string {
  const t = convTitle(c);
  return (t[0] ?? "?").toUpperCase();
}

/** 消息按 seq 插入（乐观消息 client:xxx 排最后），去重 */
export function upsertMessage(list: MessageView[], m: MessageView): MessageView[] {
  const idx = list.findIndex((x) => x.id === m.id);
  if (idx >= 0) {
    const next = [...list];
    next[idx] = m;
    return next;
  }
  const next = [...list, m];
  next.sort((a, b) => a.seq - b.seq);
  return next;
}

export function prependMessages(list: MessageView[], older: MessageView[]): MessageView[] {
  const seen = new Set(list.map((m) => m.id));
  return [...older.filter((m) => !seen.has(m.id)), ...list];
}

/** ack 到达：用服务端消息替换乐观副本（message 回显可能先于 ack，需按 id 去重） */
export function resolveAck(
  list: MessageView[],
  clientMsgId: string | null,
  real: MessageView,
): MessageView[] {
  const mapped = clientMsgId
    ? list.map((m) => (m.id === `client:${clientMsgId}` ? real : m))
    : list;
  const seen = new Set<string>();
  return mapped
    .filter((m) => !seen.has(m.id) && (seen.add(m.id), true))
    .sort((a, b) => a.seq - b.seq);
}
