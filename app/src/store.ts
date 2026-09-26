import { useSyncExternalStore } from "react";
import { applyStreamFrame, dropRun, fmtElapsed, type AgentRuns, type AgentStreamFrame } from "./agentRun";
import { writeDrafts } from "./drafts";
import type {
  AgentHostView,
  ConversationPref,
  ConversationView,
  FriendView,
  InvocationView,
  MessageView,
  RequestsView,
  UserPublic,
} from "./types";

/** 应用内新消息提示（最小通知出口；点击打开会话） */
export interface Toast {
  id: string;
  convId: string;
  title: string;
  text: string;
  ts: number;
}

export interface AppState {
  me: UserPublic | null;
  conversations: ConversationView[];
  /** conv_id -> 按 seq 升序的消息（含乐观未确认消息，pending 用 client: 前缀 id） */
  messages: Record<string, MessageView[]>;
  hasMore: Record<string, boolean>;
  /** conv_id -> 初始历史页是否已加载（P1-11：实时缓存非空也不能跳过历史拉取） */
  historyLoaded: Record<string, boolean>;
  activeConv: string | null;
  online: Record<string, boolean>;
  /** conv_id -> { user, ts } 正在输入 */
  typing: Record<string, { user: UserPublic; ts: number }>;
  friends: FriendView[];
  requests: RequestsView;
  wsReady: boolean;
  tab: "chats" | "contacts" | "me" | "admin";
  /** composer 引用卡片：正在回复的目标消息（null=普通发送） */
  replyingTo: MessageView | null;
  /** composer 编辑态：正在编辑的消息（expected_version 取其当前 edit_version） */
  editingMessage: MessageView | null;
  /** `${messageId}:${emoji}` -> 提交中（reaction 按钮 busy 态） */
  reactionPending: Record<string, boolean>;
  /** 应用内通知栈（新消息提示，自动过期） */
  toasts: Toast[];
  /** conv_id -> 输入框草稿（V2 P1-3；持久化到 localStorage，按用户隔离） */
  drafts: Record<string, string>;
  /** user_id -> 已知最新头像版本。只增不减；404 回落时把 has 记为 false。 */
  avatars: Record<string, AvatarInfo>;
  /** 全局搜索跳转目标（V2 P3-1）：打开会话后由 ChatView 消费并清空 */
  pendingJump: { convId: string; messageId: string; seq: number } | null;
  /** Agent A01：本人宿主列表（AgentsPanel 拉取 / agent_host_status 帧更新 online） */
  agentHosts: AgentHostView[];
  /** Agent A01/A11：conv_id -> trigger_message_id -> 调用记录（一条消息可以触发多个 Agent） */
  invocations: Record<string, Record<string, InvocationView[]>>;
  /** Agent A03：conv_id -> invocation_id -> 进行中调用的临时输出（不落库、不跨设备） */
  agentRuns: AgentRuns;
}

/** 头像缓存条目：版本较大的那组才有效。 */
export interface AvatarInfo {
  version: number;
  has: boolean;
}

let state: AppState = {
  me: null,
  conversations: [],
  messages: {},
  hasMore: {},
  historyLoaded: {},
  activeConv: null,
  online: {},
  typing: {},
  friends: [],
  requests: { incoming: [], outgoing: [] },
  wsReady: false,
  tab: "chats",
  replyingTo: null,
  editingMessage: null,
  reactionPending: {},
  toasts: [],
  drafts: {},
  avatars: {},
  pendingJump: null,
  agentHosts: [],
  invocations: {},
  agentRuns: {},
};

const listeners = new Set<() => void>();

export function getState() {
  return state;
}

export function setState(patch: Partial<AppState> | ((s: AppState) => Partial<AppState>)) {
  const p = typeof patch === "function" ? patch(state) : patch;
  const me = p.me === undefined ? state.me : p.me;
  const avatars = p.avatars ?? state.avatars;
  const absorbed = me === state.me ? avatars : absorbAvatarUsers(avatars, [me]);
  state = { ...state, ...p, ...(absorbed === avatars ? {} : { avatars: absorbed }) };
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
  // V2 P2-4：self 会话 name 恒 NULL、peer 恒空，标题固定
  if (c.kind === "self") return "文件传输助手";
  // Agent A01：专属会话 name 恒 NULL，标题是 Agent 名
  if (c.kind === "agent") return c.agent?.name ?? "Agent";
  return c.peer?.display_name ?? c.name ?? "会话";
}

/**
 * 「这条消息是不是我发的」唯一判定（Agent A01）：
 * Agent 消息的 sender_id 仍是主人，但带 m.agent —— 对主人它是「别人发来的」
 * （气泡在左、计未读、可触发通知、不可编辑）。各处一律走这个纯函数。
 */
export function isOwnMessage(m: MessageView, meId?: string | null): boolean {
  return !!meId && m.sender_id === meId && !m.agent;
}

/** 把一批 UserPublic 合并进头像表，只接受更大的版本。 */
export function absorbAvatarUsers(
  avatars: Record<string, AvatarInfo>,
  users: Array<UserPublic | null | undefined>,
): Record<string, AvatarInfo> {
  let next = avatars;
  for (const user of users) {
    if (!user?.id) continue;
    const merged = mergeAvatar(next[user.id], {
      version: user.avatar_version ?? 0,
      has: !!user.has_avatar,
    });
    if (next[user.id]?.version === merged.version && next[user.id]?.has === merged.has) continue;
    if (next === avatars) next = { ...avatars };
    next[user.id] = merged;
  }
  return next;
}

/** 只接受更大的版本；同版本允许更新 has（例如同一次刷新把有无写实）。 */
export function mergeAvatar(
  current: AvatarInfo | undefined,
  incoming: AvatarInfo,
): AvatarInfo {
  if (!current || incoming.version > current.version) return incoming;
  if (incoming.version === current.version) return { version: current.version, has: incoming.has };
  return current;
}

/** 消息行：消息自带版本与 store 里的值取较大的那组。 */
export function senderAvatar(
  stored: AvatarInfo | undefined,
  version: number | undefined,
  has: boolean | undefined,
): AvatarInfo {
  return mergeAvatar(stored, { version: version ?? 0, has: !!has });
}

/** 从任意 UserPublic（本人、好友、会话 peer、群成员）合并进 avatars。 */
export function noteUserAvatar(user: UserPublic | null | undefined): void {
  if (!user?.id) return;
  setState((s) => {
    const next = mergeAvatar(s.avatars[user.id], {
      version: user.avatar_version ?? 0,
      has: !!user.has_avatar,
    });
    if (s.avatars[user.id]?.version === next.version && s.avatars[user.id]?.has === next.has) return {};
    return { avatars: { ...s.avatars, [user.id]: next } };
  });
}

/** 图片 404：该版本记为无头像，避免反复请求。 */
export function noteAvatarMiss(userId: string, version: number): void {
  setState((s) => ({
    avatars: { ...s.avatars, [userId]: mergeAvatar(s.avatars[userId], { version, has: false }) },
  }));
}

export function convAvatarText(c: ConversationView): string {
  const t = convTitle(c);
  return (t[0] ?? "?").toUpperCase();
}

/**
 * 消息按 seq 插入（乐观消息 client:xxx 排最后），去重。
 * 统一 id+version 合并规则：incoming.edit_version（缺省按 0）低于缓存版本时丢弃，
 * 保证迟到的旧生命周期事件不覆盖较新的编辑/删除/reaction 状态。
 */
function selectMessage(local: MessageView, incoming: MessageView): MessageView {
  const delta = (incoming.edit_version ?? 0) - (local.edit_version ?? 0);
  if (delta < 0) return local;
  if (delta > 0) return incoming;
  return {
    ...local,
    reply_to_preview: local.deleted_at || local.reply_to_preview?.deleted
      ? local.reply_to_preview
      : incoming.reply_to_preview ?? local.reply_to_preview,
    ...(local.poll && incoming.poll?.expired ? { poll: { ...local.poll, expired: true } } : {}),
  };
}

export function replyPreview(m: MessageView) {
  return {
    message_id: m.id,
    seq: m.seq,
    sender_name: m.sender_name,
    excerpt: m.deleted_at ? "" : m.attachment
      ? m.kind === "image"
        ? "[图片]"
        : m.kind === "audio" || m.attachment.duration_ms != null
          ? "[语音]"
          : `[文件] ${m.attachment.file_name}`
      : Array.from(m.content).slice(0, 120).join(""),
    kind: m.kind,
    deleted: !!m.deleted_at,
  };
}

function selectLast(local: MessageView | null, incoming: MessageView | null): MessageView | null {
  if (!local) return incoming;
  if (!incoming) return local;
  if (local.id === incoming.id) return selectMessage(local, incoming);
  return incoming.seq > local.seq ? incoming : local;
}

export function upsertMessage(list: MessageView[], m: MessageView): MessageView[] {
  const idx = list.findIndex((x) => x.id === m.id);
  if (idx >= 0) {
    const selected = selectMessage(list[idx], m);
    if (selected === list[idx]) return list;
    const next = [...list];
    next[idx] = selected;
    return next;
  }
  const next = [...list, m];
  next.sort((a, b) => a.seq - b.seq);
  return next;
}

/**
 * 从 after 起向后逐页拉，直到拉到 stop 前一条或没有更多。
 * 跳转到缓存之外的旧消息时用它补齐到缓存最早一条：缓存必须连续（hasMore/loadMore 只看最小 seq）。
 * ponytail: 远距离跳转会逐页（200 条/页）补齐整段；历史到万级再改成独立窗口 + 向下加载。
 */
export async function fetchForward(
  page: (after: number) => Promise<{ messages: MessageView[]; has_more: boolean }>,
  after: number,
  stop: number,
): Promise<MessageView[]> {
  const out: MessageView[] = [];
  for (;;) {
    const p = await page(after);
    out.push(...p.messages);
    const last = p.messages[p.messages.length - 1];
    if (!p.has_more || !last || last.seq >= stop - 1) return out;
    after = last.seq;
  }
}

export function prependMessages(list: MessageView[], older: MessageView[]): MessageView[] {
  const seen = new Set(list.map((m) => m.id));
  return [...older.filter((m) => !seen.has(m.id)), ...list];
}

// ---------- 会话偏好 ----------

export function isPinned(c: ConversationView): boolean {
  return !!c.preference?.pinned_at;
}

export function isArchived(c: ConversationView): boolean {
  return !!c.preference?.archived_at;
}

/** 静音是否生效：永久静音或未过期的定时静音（过期时间按本地时钟判断） */
export function isMuted(c: ConversationView): boolean {
  const p = c.preference;
  if (!p) return false;
  if (p.mute_forever) return true;
  return !!p.muted_until && Date.parse(p.muted_until) > Date.now();
}

function selectPreference(
  local: ConversationView["preference"],
  incoming: ConversationView["preference"],
): ConversationView["preference"] {
  if (!local?.updated_at) return incoming ?? local;
  if (!incoming?.updated_at) return local;
  return Date.parse(incoming.updated_at) < Date.parse(local.updated_at) ? local : incoming;
}

/** 偏好变更合并入口（preference_updated 帧 / PUT 响应共用）：更新字段并重排列表 */
export function applyPrefUpdate(
  s: AppState,
  conversationId: string,
  preference: ConversationPref,
): Partial<AppState> {
  if (!s.conversations.some((c) => c.id === conversationId)) return {};
  return {
    conversations: sortConversations(
      s.conversations.map((c) =>
        c.id === conversationId ? { ...c, preference: selectPreference(c.preference, preference) } : c,
      ),
    ),
  };
}

/**
 * 新消息是否触发应用内提示：非本人所发、非当前打开的会话；
 * 静音会话抑制提示，但 @本人 可突破静音（§8.3）。
 */
export function shouldNotify(s: AppState, m: MessageView): boolean {
  if (s.me == null || isOwnMessage(m, s.me.id)) return false;
  if (s.activeConv === m.conversation_id && s.tab === "chats") return false;
  const conv = s.conversations.find((c) => c.id === m.conversation_id);
  if (conv && isMuted(conv) && !m.mentions.includes(s.me.id)) return false;
  return true;
}

export function pushToast(t: Omit<Toast, "id" | "ts">) {
  const id = crypto.randomUUID();
  setState((s) => ({
    toasts: [...s.toasts.slice(-3), { ...t, id, ts: Date.now() }],
  }));
  setTimeout(() => dismissToast(id), 5000);
}

export function dismissToast(id: string) {
  setState((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
}

// ---------- 会话草稿（V2 P1-3） ----------

let draftTimer: ReturnType<typeof setTimeout> | null = null;

/** 更新某会话草稿；空串删除该键。localStorage 写入防抖 300ms */
export function setDraft(convId: string, text: string) {
  setState((s) => {
    const drafts = { ...s.drafts };
    if (text) drafts[convId] = text;
    else delete drafts[convId];
    return { drafts };
  });
  if (draftTimer != null) clearTimeout(draftTimer);
  draftTimer = setTimeout(flushDrafts, 300);
}

/** 立即把当前草稿表落盘（pagehide 时调用，堵住防抖窗口丢草稿） */
export function flushDrafts() {
  if (draftTimer != null) {
    clearTimeout(draftTimer);
    draftTimer = null;
  }
  const { me, drafts } = getState();
  if (me && typeof localStorage !== "undefined") writeDrafts(localStorage, me.id, drafts);
}

/** 置顶优先：置顶块内按 pinned_at DESC，其余按原活动时间 DESC，id 兜底保序 */
export function sortConversations(list: ConversationView[]): ConversationView[] {
  const act = (c: ConversationView) => c.last_message?.created_at ?? c.created_at;
  return [...list].sort((a, b) => {
    const ap = a.preference?.pinned_at ?? null;
    const bp = b.preference?.pinned_at ?? null;
    if (ap == null && bp != null) return 1;
    if (ap != null && bp == null) return -1;
    if (ap != null && bp != null && ap !== bp) return bp.localeCompare(ap);
    return act(b).localeCompare(act(a)) || a.id.localeCompare(b.id);
  });
}

/** 目录刷新和实时消息可能交错。本地 seq 更新的预览留下来，远端没有的会话以远端为准（退群会消失）。 */
export function mergeConversationLists(
  local: ConversationView[],
  remote: ConversationView[],
): ConversationView[] {
  const localById = new Map(local.map((c) => [c.id, c]));
  const merged = remote.map((rc) => {
    const lc = localById.get(rc.id);
    if (!lc) return rc;
    rc = { ...rc, preference: selectPreference(lc.preference, rc.preference) };
    if (!lc.last_message) return rc;
    if (lc.last_message.seq > (rc.last_message?.seq ?? 0)) {
      return {
        ...rc,
        last_message: lc.last_message,
        last_seq: Math.max(lc.last_seq, rc.last_seq),
        unread_count: lc.unread_count,
        has_mention: lc.has_mention || rc.has_mention,
      };
    }
    return { ...rc, last_message: selectLast(lc.last_message, rc.last_message) };
  });
  return sortConversations(merged);
}

/**
 * 实时消息。同一 id 再投递一次不重复加未读。
 * foreground=false（窗口在后台）时打开着的会话也照常计未读，回到前台再补报已读。
 */
export function applyLiveMessage(s: AppState, m: MessageView, foreground = true): Partial<AppState> {
  const list = s.messages[m.conversation_id] ?? [];
  const exists = list.some((x) => x.id === m.id) || s.conversations.some((c) => c.last_message?.id === m.id);
  const refreshed = applyRefreshed(s, m.conversation_id, [m]);
  const isActive = s.activeConv === m.conversation_id && s.tab === "chats" && foreground;
  // Agent 消息对主人按「别人发来的」计未读（sender_id 仍是主人，由 m.agent 区分）
  const mine = isOwnMessage(m, s.me?.id);
  const { [m.conversation_id]: _typing, ...typing } = s.typing;
  return {
    ...refreshed,
    conversations: sortConversations(
      (refreshed.conversations ?? s.conversations).map((c) =>
        c.id === m.conversation_id
          ? {
              ...c,
              last_message: selectLast(c.last_message, refreshed.messages?.[m.conversation_id].find((x) => x.id === m.id) ?? m),
              last_seq: Math.max(c.last_seq, m.seq),
              unread_count: exists || mine || isActive ? c.unread_count : c.unread_count + 1,
              has_mention:
                c.has_mention ||
                (!exists && !mine && !isActive && s.me != null && m.mentions.includes(s.me.id)),
            }
          : c,
      ),
    ),
    typing,
  };
}

/** 断线补齐。未读以目录刷新为准，这里只补线程和更新的预览。 */
export function applySyncMessages(
  s: AppState,
  conversationId: string,
  incoming: MessageView[],
): Partial<AppState> {
  if (incoming.length === 0) return {};
  const refreshed = applyRefreshed(s, conversationId, incoming);
  const { [conversationId]: _typing, ...typing } = s.typing;
  return {
    ...refreshed,
    typing,
    conversations: sortConversations(
      (refreshed.conversations ?? s.conversations).map((c) => {
        if (c.id !== conversationId) return c;
        let last = c.last_message;
        let lastSeq = c.last_seq;
        for (const m of refreshed.messages?.[conversationId] ?? incoming) {
          if (m.id.startsWith("client:")) continue;
          last = selectLast(last, m);
          if (m.seq > lastSeq) lastSeq = m.seq;
        }
        return { ...c, last_message: last, last_seq: lastSeq };
      }),
    ),
  };
}

export function dropOptimistic(s: AppState, convId: string, clientMsgId: string): Partial<AppState> {
  const id = `client:${clientMsgId}`;
  return {
    messages: {
      ...s.messages,
      [convId]: (s.messages[convId] ?? []).filter((m) => m.id !== id),
    },
  };
}

/** 生命周期广播（编辑/删除/reaction）唯一合并入口：服务端所有 mutation 帧
 * 均携带完整 MessageView（reactions 已按本接收者个性化），按 id+edit_version
 * 整视图合并——版本更高的视图天然包含低版本的全部变更，迟到的旧帧直接丢弃，
 * 不存在部分字段帧互相吞更新的问题。会话卡片上的 last_message 同步换新版。 */
export function applyUpdatedMessage(s: AppState, m: MessageView): Partial<AppState> {
  return applyRefreshed(s, m.conversation_id, [m]);
}

/** 重连批量刷新：回包按同一 id+version 规则合并；last_message 引用同 id 时换新 */
export function applyRefreshed(
  s: AppState,
  convId: string,
  incoming: MessageView[],
): Partial<AppState> {
  if (incoming.length === 0) return {};
  const cached = s.messages[convId] ?? [];
  const byId = new Map(cached.map((m) => [m.id, m]));
  const merge = (m: MessageView) => {
    const old = byId.get(m.id);
    byId.set(m.id, old ? selectMessage(old, m) : m);
  };
  for (const c of s.conversations) {
    if (c.id === convId && c.last_message) merge(c.last_message);
  }
  incoming.filter((m) => m.conversation_id === convId).forEach(merge);
  const withReply = (m: MessageView): MessageView => {
    const source = m.reply_to_id && byId.get(m.reply_to_id);
    return source && !m.deleted_at && !(m.reply_to_preview?.deleted && !source.deleted_at)
      ? { ...m, reply_to_preview: replyPreview(source) }
      : m;
  };
  const ids = new Set([...cached.map((m) => m.id), ...incoming.filter((m) => m.conversation_id === convId).map((m) => m.id)]);
  const list = [...ids].map((id) => withReply(byId.get(id)!)).sort((a, b) => a.seq - b.seq);
  return {
    messages: { ...s.messages, [convId]: list },
    conversations: s.conversations.map((c) => c.id === convId && c.last_message
      ? { ...c, last_message: withReply(byId.get(c.last_message.id)!) }
      : c),
    replyingTo: s.replyingTo?.conversation_id === convId
      ? withReply(byId.get(s.replyingTo.id) ?? s.replyingTo)
      : s.replyingTo,
  };
}

/** 会话不再可访问（refresh 403/404）：丢弃本地消息缓存与滚动状态 */
export function dropConversationCache(s: AppState, convId: string): Partial<AppState> {
  const messages = { ...s.messages };
  delete messages[convId];
  const hasMore = { ...s.hasMore };
  delete hasMore[convId];
  const historyLoaded = { ...s.historyLoaded };
  delete historyLoaded[convId];
  const typing = { ...s.typing };
  delete typing[convId];
  return { messages, hasMore, historyLoaded, typing };
}

// ---------- Agent A01：宿主在线态 / 调用记录 ----------

/** agent_host_status 帧：更新在线态；带 label 时同时改名（上下线帧不带 label，保留原名） */
export function applyHostStatus(
  s: AppState,
  hostId: string,
  online: boolean,
  label?: string,
): Partial<AppState> {
  return {
    agentHosts: s.agentHosts.map((h) =>
      h.id === hostId ? { ...h, online, ...(label ? { label } : {}) } : h,
    ),
    conversations: s.conversations.map((c) =>
      c.agent?.host_id === hostId && c.agent.host_online !== online
        ? { ...c, agent: { ...c.agent, host_online: online } }
        : c,
    ),
  };
}

/** agent_invocation 帧 / 历史 GET 共用合并入口：同一触发消息下按调用 id 覆盖，不同调用都保留。
 *  A03：调用进终态时顺手摘除它的临时流式输出（定稿消息已含全部内容）。 */
export function applyInvocations(
  s: AppState,
  incoming: InvocationView[],
): Partial<AppState> {
  if (incoming.length === 0) return {};
  const invocations = { ...s.invocations };
  let agentRuns = s.agentRuns;
  for (const inv of incoming) {
    const cur = { ...(invocations[inv.conversation_id] ?? {}) };
    const prev = cur[inv.trigger_message_id] ?? [];
    const idx = prev.findIndex((x) => x.id === inv.id);
    cur[inv.trigger_message_id] =
      idx >= 0 ? prev.map((x, i) => (i === idx ? inv : x)) : [...prev, inv];
    invocations[inv.conversation_id] = cur;
    const convRuns = agentRuns[inv.conversation_id];
    if (inv.finished_at != null && convRuns?.[inv.id]) {
      if (agentRuns === s.agentRuns) agentRuns = { ...agentRuns };
      const next = dropRun(convRuns, inv.id) ?? {};
      if (Object.keys(next).length) {
        agentRuns[inv.conversation_id] = next;
      } else {
        delete agentRuns[inv.conversation_id];
      }
    }
  }
  return agentRuns === s.agentRuns ? { invocations } : { invocations, agentRuns };
}

/** agent_stream 帧：临时输出追加；调用已知终态后迟到的帧丢弃（防状态残留）。 */
export function applyAgentStream(
  s: AppState,
  f: AgentStreamFrame,
): Partial<AppState> {
  const invs = s.invocations[f.conversation_id];
  const inv = invs && Object.values(invs).flat().find((i) => i.id === f.invocation_id);
  if (inv?.finished_at != null) return {};
  const convRuns = s.agentRuns[f.conversation_id];
  const next = applyStreamFrame(convRuns, f);
  if (next === convRuns) return {};
  return { agentRuns: { ...s.agentRuns, [f.conversation_id]: next } };
}

/** 调用状态标签的查表：running → 进行中标签；failed → 失败原因；其余 → null 不渲染。
 *  A04 往本表加 queued / cancelled / timed_out 行。 */
export function invocationStatusView(
  inv: Pick<InvocationView, "status" | "error" | "current_step">,
  elapsedSecs: number,
): { icon: string; text: string } | null {
  switch (inv.status) {
    case "running":
      return inv.current_step
        ? {
            icon: "⏳",
            text: `正在工作 · 已用 ${fmtElapsed(elapsedSecs)} · 当前：${inv.current_step}`,
          }
        : { icon: "⏳", text: "正在处理" };
    case "failed":
      return { icon: "⚠", text: `失败：${invocationErrorText(inv.error)}` };
    case "queued":
      return { icon: "⏸", text: "排队中：Agent 所在电脑不在线" };
    case "cancelled":
      return { icon: "", text: "已取消" };
    case "timed_out":
      return { icon: "", text: "排队超时，已取消" };
    case "awaiting_approval":
      return { icon: "⏸", text: "等待主人批准" };
    default:
      return null;
  }
}

/** 状态标签的纯文本形式（不含已用时长、当前步骤），由 invocationStatusView 派生 */
export function invocationStatusLabel(
  status: string,
  error: string | null | undefined,
): string | null {
  const v = invocationStatusView({ status, error: error ?? null, current_step: null }, 0);
  return v ? `${v.icon} ${v.text}`.trim() : null;
}

/** 主人可以取消的状态：排队、运行中、等待审批 */
export function invocationCancellable(status: string): boolean {
  return status === "queued" || status === "running" || status === "awaiting_approval";
}

/** 调用失败原因的展示文案（host 侧稳定错误码 → 中文） */
export function invocationErrorText(error: string | null | undefined): string {
  switch (error) {
    case "host_offline":
      return "Agent 所在电脑不在线";
    case "host_disconnected":
      return "Agent 所在电脑断开了";
    case "agent_deleted":
      return "该 Agent 已删除";
    default:
      return error ?? "未知原因";
  }
}

/** ack 到达：用服务端消息替换乐观副本（message 回显可能先于 ack，需按 id 去重） */
export function resolveAck(
  list: MessageView[],
  clientMsgId: string | null,
  real: MessageView,
): MessageView[] {
  return upsertMessage(list.filter((m) => m.id !== `client:${clientMsgId}`), real);
}
