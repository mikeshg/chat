import type {
  AttachmentView,
  AuthResponse,
  ConversationDetail,
  ConversationView,
  FriendView,
  MessagesPage,
  MessageView,
  RequestsView,
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
  localStorage.setItem(LS_SERVER, url.trim().replace(/\/+$/, ""));
}
export function wsBase(): string {
  const s = getServer() || location.origin;
  return s.replace(/^http/, "ws");
}

// ---------- 会话凭据 ----------
export function saveSession(r: AuthResponse) {
  localStorage.setItem(LS_ACCESS, r.access_token);
  localStorage.setItem(LS_REFRESH, r.refresh_token);
  localStorage.setItem(LS_USER, JSON.stringify(r.user));
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
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, msg: string) {
    super(msg);
    this.status = status;
  }
}

let refreshing: Promise<boolean> | null = null;

async function tryRefresh(): Promise<boolean> {
  const rt = localStorage.getItem(LS_REFRESH);
  if (!rt) return false;
  refreshing ??= (async () => {
    try {
      const res = await fetch(`${getServer()}/v1/auth/refresh`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refresh_token: rt }),
      });
      if (!res.ok) return false;
      const r: AuthResponse = await res.json();
      saveSession(r);
      return true;
    } catch {
      return false;
    } finally {
      setTimeout(() => (refreshing = null), 0);
    }
  })();
  return refreshing;
}

async function raw(path: string, init?: RequestInit): Promise<Response> {
  const headers: Record<string, string> = {
    ...(init?.headers as Record<string, string>),
  };
  const at = localStorage.getItem(LS_ACCESS);
  if (at) headers["authorization"] = `Bearer ${at}`;
  if (init?.body) headers["Content-Type"] = "application/json";
  return fetch(`${getServer()}/v1${path}`, { ...init, headers });
}

async function call<T>(path: string, init?: RequestInit, retried = false): Promise<T> {
  const res = await raw(path, init);
  if (res.status === 401 && !retried) {
    if (await tryRefresh()) return call<T>(path, init, true);
    clearSession();
    window.dispatchEvent(new Event("chat:logout"));
    throw new ApiError(401, "unauthorized");
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
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

// ---------- auth ----------
export const auth = {
  register: (username: string, password: string, display_name?: string) =>
    post<AuthResponse>("/auth/register", { username, password, display_name }),
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
    if (opts?.before_seq) p.set("before_seq", String(opts.before_seq));
    if (opts?.after_seq) p.set("after_seq", String(opts.after_seq));
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
  ticket: () => get<{ ticket: string; expires_in: number }>("/ws/ticket"),
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
    const at = localStorage.getItem(LS_ACCESS);
    const res = await fetch(`${getServer()}/v1/files`, {
      method: "POST",
      headers: at ? { authorization: `Bearer ${at}` } : {},
      body: fd,
    });
    if (res.status === 401 && (await tryRefresh())) return files.upload(conversationId, file);
    if (!res.ok) {
      const text = await res.text();
      let msg = text;
      try {
        msg = JSON.parse(text).error ?? text;
      } catch {}
      throw new ApiError(res.status, msg);
    }
    return res.json() as Promise<AttachmentView>;
  },
};
