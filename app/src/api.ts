import type {
  AdminOverview,
  AdminSettings,
  AdminUserView,
  AttachmentView,
  AuthResponse,
  ConversationDetail,
  ConversationPref,
  ConversationPrefEntry,
  ConversationView,
  CreateInviteReq,
  CreatePollReq,
  FriendView,
  InviteView,
  MessagesPage,
  MessageView,
  PrefUpdate,
  RequestsView,
  ServerInfo,
  UserPublic,
} from "./types";

const LS_SERVER = "chat_server_url";
const LS_ACCESS = "chat_access";
const LS_REFRESH = "chat_refresh";
const LS_USER = "chat_user";

// ---------- 服务器地址 ----------
// 空 = 同源（vite dev proxy / 服务端托管静态文件时）；Tauri/LAN 场景填 http://host:8080
export function getServer(): string {
  return (localStorage.getItem(LS_SERVER) ?? "").replace(/\/+$/, "");
}
export function setServer(url: string) {
  const next = url.trim().replace(/\/+$/, "");
  if (next !== getServer()) {
    localStorage.setItem(LS_SERVER, next);
    sessionGen++; // 服务器切换：旧服务器的异步结果一律作废
  }
}
export function wsBase(): string {
  const s = getServer() || location.origin;
  return s.replace(/^http/, "ws");
}

// ---------- 会话凭据 ----------
// 会话代次：登录/登出/换服务器都会 +1；异步回调先比对代次再落地，防旧账号结果污染新账号
let sessionGen = 0;
export function session(): number {
  return sessionGen;
}

export function saveSession(r: AuthResponse) {
  localStorage.setItem(LS_ACCESS, r.access_token);
  localStorage.setItem(LS_REFRESH, r.refresh_token);
  localStorage.setItem(LS_USER, JSON.stringify(r.user));
  sessionGen++; // 新登录 = 新会话
}
export function loadSession(): { user: UserPublic } | null {
  const u = localStorage.getItem(LS_USER);
  if (!u || !localStorage.getItem(LS_REFRESH)) return null;
  try {
    return { user: JSON.parse(u) };
  } catch {
    return null;
  }
}
export function clearSession() {
  localStorage.removeItem(LS_ACCESS);
  localStorage.removeItem(LS_REFRESH);
  localStorage.removeItem(LS_USER);
  sessionGen++; // 登出 = 旧会话作废
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, msg: string) {
    super(msg);
    this.status = status;
  }
}

let refreshing: { gen: number; promise: Promise<boolean> } | null = null;

function requireSession(gen: number) {
  if (gen !== session()) throw new ApiError(0, "session changed");
}

async function tryRefresh(gen: number, srv: string): Promise<boolean> {
  requireSession(gen);
  if (refreshing?.gen === gen) return refreshing.promise;
  const rt = localStorage.getItem(LS_REFRESH);
  if (!rt) return false;
  const owner = { gen, promise: Promise.resolve(false) };
  refreshing = owner;
  owner.promise = (async () => {
    try {
      const res = await fetch(`${srv}/v1/auth/refresh`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refresh_token: rt }),
      });
      if (!res.ok) return false;
      const r: AuthResponse = await res.json();
      // 期间发生了登录/登出/换服务器：不能把旧凭据写回新会话
      if (gen !== session() || localStorage.getItem(LS_REFRESH) !== rt || getServer() !== srv) return false;
      localStorage.setItem(LS_ACCESS, r.access_token);
      localStorage.setItem(LS_REFRESH, r.refresh_token);
      localStorage.setItem(LS_USER, JSON.stringify(r.user));
      return true;
    } catch {
      return false;
    } finally {
      setTimeout(() => { if (refreshing === owner) refreshing = null; }, 0);
    }
  })();
  return owner.promise;
}

async function raw(srv: string, path: string, init?: RequestInit): Promise<Response> {
  const headers: Record<string, string> = {
    ...(init?.headers as Record<string, string>),
  };
  const at = localStorage.getItem(LS_ACCESS);
  if (at) headers["authorization"] = `Bearer ${at}`;
  if (typeof init?.body === "string") headers["Content-Type"] = "application/json";
  return fetch(`${srv}/v1${path}`, { ...init, headers });
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const gen = session();
  const srv = getServer();
  let res = await raw(srv, path, init);
  requireSession(gen);
  if (res.status === 401) {
    const refreshed = await tryRefresh(gen, srv);
    requireSession(gen);
    if (refreshed) {
      res = await raw(srv, path, init);
      requireSession(gen);
    }
    if (!refreshed || res.status === 401) {
      clearSession();
      window.dispatchEvent(new Event("chat:logout"));
      throw new ApiError(401, "unauthorized");
    }
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  requireSession(gen);
  if (!res.ok) {
    let msg = text;
    try {
      msg = JSON.parse(text).error ?? text;
    } catch {}
    throw new ApiError(res.status, msg);
  }
  return JSON.parse(text) as T;
}

const post = <T>(p: string, body?: unknown) =>
  call<T>(p, { method: "POST", body: body == null ? "{}" : JSON.stringify(body) });
const get = <T>(p: string) => call<T>(p);
const del = <T>(p: string) => call<T>(p, { method: "DELETE" });
const patch = <T>(p: string, body: unknown) =>
  call<T>(p, { method: "PATCH", body: JSON.stringify(body) });
const put = <T>(p: string, body?: unknown) =>
  call<T>(p, {
    method: "PUT",
    body: body === undefined ? undefined : JSON.stringify(body),
  });

// ---------- auth ----------
export const auth = {
  register: (username: string, password: string, display_name?: string, inviteCode?: string) =>
    post<AuthResponse>("/auth/register", {
      username,
      password,
      display_name,
      invite_code: inviteCode,
    }),
  login: (username: string, password: string) =>
    post<AuthResponse>("/auth/login", { username, password }),
  logout: () => {
    const rt = localStorage.getItem(LS_REFRESH);
    if (rt) post("/auth/logout", { refresh_token: rt }).catch(() => {});
    clearSession();
  },
  me: () => get<UserPublic>("/users/me"),
  updateMe: (display_name: string) => patch<UserPublic>("/users/me", { display_name }),
  searchUser: async (q: string) => {
    const r = await get<unknown>(`/users/search?q=${encodeURIComponent(q)}`);
    // 兼容旧服务端返回单对象的形态
    return Array.isArray(r) ? (r as UserPublic[]) : r ? [r as UserPublic] : [];
  },
};

// ---------- friends ----------
export const friends = {
  list: () => get<FriendView[]>("/friends"),
  remove: (userId: string) => del<void>(`/friends/${userId}`),
  requests: () => get<RequestsView>("/friends/requests"),
  sendRequest: (username: string, message: string) =>
    post<{ id: string; status: string }>("/friends/requests", { username, message }),
  accept: (id: string) => post(`/friends/requests/${id}/accept`),
  decline: (id: string) => post(`/friends/requests/${id}/decline`),
};

// ---------- conversations ----------
export const convs = {
  list: () => get<ConversationView[]>("/conversations"),
  detail: (id: string) => get<ConversationDetail>(`/conversations/${id}`),
  messages: (id: string, opts?: { before_seq?: number; after_seq?: number; limit?: number }) => {
    const p = new URLSearchParams();
    if (opts?.before_seq != null) p.set("before_seq", String(opts.before_seq));
    if (opts?.after_seq != null) p.set("after_seq", String(opts.after_seq));
    p.set("limit", String(opts?.limit ?? 50));
    return get<MessagesPage>(`/conversations/${id}/messages?${p}`);
  },
  search: (id: string, q: string) =>
    get<MessageView[]>(`/conversations/${id}/messages/search?q=${encodeURIComponent(q)}`),
  createGroup: (name: string, member_ids: string[]) =>
    post<ConversationDetail>("/conversations/group", { name, member_ids }),
  addMembers: (id: string, user_ids: string[]) =>
    post(`/conversations/${id}/members`, { user_ids }),
  rename: (id: string, name: string) => patch(`/conversations/${id}`, { name }),
  leave: (id: string) => del<void>(`/conversations/${id}/members/me`),
  kick: (id: string, uid: string) => del<void>(`/conversations/${id}/members/${uid}`),
  dissolve: (id: string) => del<void>(`/conversations/${id}`),
  transfer: (id: string, uid: string) =>
    post(`/conversations/${id}/transfer`, { user_id: uid }),
  /** 全量偏好（列表已带 preference，一般无需单独拉） */
  preferences: () => get<ConversationPrefEntry[]>("/conversations/preferences"),
  /** 更新本人偏好：只传要改的字段；返回最新偏好（幂等覆写） */
  setPref: (id: string, body: PrefUpdate) =>
    put<ConversationPref>(`/conversations/${id}/preferences`, body),
  ticket: () => get<{ ticket: string; expires_in: number }>("/ws/ticket"),
};

// ---------- 消息生命周期 ----------
// mutation 全部服务端确认后更新（不乐观回滚）；client_op_id 为 UUID 幂等键，
// 操作结果未知时只能用原 id 重试。reaction/删除响应带 edit_version 供版本合并。
export const msgs = {
  edit: (id: string, content: string, expected_version: number, client_op_id?: string) =>
    patch<MessageView>(`/messages/${id}`, { content, expected_version, client_op_id }),
  remove: (id: string, client_op_id?: string) =>
    del<MessageView>(
      `/messages/${id}${client_op_id ? `?client_op_id=${client_op_id}` : ""}`,
    ),
  react: (id: string, emoji: string, add: boolean, client_op_id?: string) => {
    const q = client_op_id ? `?client_op_id=${client_op_id}` : "";
    const p = `/messages/${id}/reactions/${encodeURIComponent(emoji)}${q}`;
    return add ? put<MessageView>(p) : del<MessageView>(p);
  },
  /** 重连批量回刷缓存消息（每批 ≤100 个 id） */
  refresh: (convId: string, ids: string[]) =>
    post<MessagesPage>(`/conversations/${convId}/messages/refresh`, { ids }),
};

// ---------- 轻量投票 ----------
// mutation 走 REST（服务端确认后更新）；同步靠既有 message / message_updated 广播帧。
export const polls = {
  /** 创建投票（仅群聊）；client_msg_id 为发送幂等键（重试收敛同一消息） */
  create: (convId: string, body: CreatePollReq) =>
    post<MessageView>(`/conversations/${convId}/polls`, body),
  /** 替换本人选择集合；空数组 = 撤票。返回本人视角完整 MessageView */
  vote: (msgId: string, optionIds: string[], client_op_id?: string) =>
    put<MessageView>(`/messages/${msgId}/poll/votes`, {
      option_ids: optionIds,
      client_op_id,
    }),
  /** 关闭投票（创建者或群主；幂等） */
  close: (msgId: string, client_op_id?: string) =>
    post<MessageView>(
      `/messages/${msgId}/poll/close${client_op_id ? `?client_op_id=${client_op_id}` : ""}`,
    ),
};

// ---------- files ----------
export const files = {
  /** 附件地址（能力 URL）：dl=true 强制下载，默认 inline 供预览/<img>/<a> */
  url: (id: string, dl = false) =>
    `${getServer()}/v1/files/${id}${dl ? "?dl=1" : ""}`,
  upload: async (conversationId: string, file: File): Promise<AttachmentView> => {
    const fd = new FormData();
    fd.append("conversation_id", conversationId);
    fd.append("file", file, file.name);
    return call<AttachmentView>("/files", { method: "POST", body: fd });
  },
};

// ---------- 公共服务器信息 ----------
/** 登录前探测注册模式。请求失败/404/字段缺失一律视为 open（兼容旧服务端） */
export async function serverInfo(server?: string): Promise<ServerInfo> {
  const srv = (server ?? getServer()).trim().replace(/\/+$/, "");
  try {
    const res = await fetch(`${srv}/v1/server-info`);
    if (!res.ok) return { registration_mode: "open" };
    const j = (await res.json()) as Partial<ServerInfo>;
    const m = j.registration_mode;
    return { registration_mode: m === "invite" || m === "closed" ? m : "open" };
  } catch {
    return { registration_mode: "open" };
  }
}

// ---------- admin ----------
export const admin = {
  overview: () => get<AdminOverview>("/admin/overview"),
  settings: () => get<AdminSettings>("/admin/settings"),
  /** 部分更新：只传要改的字段；返回完整设置 */
  updateSettings: (patch: Partial<AdminSettings>) =>
    put<AdminSettings>("/admin/settings", patch),
  invites: () => get<InviteView[]>("/admin/invites"),
  createInvite: (body: CreateInviteReq) => post<InviteView>("/admin/invites", body),
  /** 撤销（幂等 204） */
  revokeInvite: (id: string) => del<void>(`/admin/invites/${id}`),
  users: (q = "", offset = 0) =>
    get<AdminUserView[]>(
      `/admin/users?offset=${offset}${q ? `&q=${encodeURIComponent(q)}` : ""}`,
    ),
  disable: (id: string) => post(`/admin/users/${id}/disable`),
  enable: (id: string) => post(`/admin/users/${id}/enable`),
  setRole: (id: string, role: "admin" | "user") =>
    post(`/admin/users/${id}/role`, { role }),
  resetPassword: (id: string, password: string) =>
    post(`/admin/users/${id}/password`, { password }),
};
