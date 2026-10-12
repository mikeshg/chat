import { removeLocalCacheForUser } from "./localCache";
import type {
  AdminOverview,
  AdminSettings,
  AdminUserView,
  AttachmentView,
  AuthResponse,
  BotView,
  BackupCreated,
  BackupList,
  BackupTicket,
  RestoreResult,
  ConversationDetail,
  ConversationPref,
  ConversationView,
  CreateInviteReq,
  CreatePollReq,
  ForwardResp,
  FriendView,
  InviteView,
  MessagesPage,
  MessageView,
  PrefUpdate,
  PushState,
  RequestsView,
  SessionView,
  AnnouncementView,
  PinEntry,
  ServerInfo,
  StorageView,
  UserPublic,
} from "./types";

const LS_SERVER = "chat_server_url";
const LS_ACCESS = "chat_access";
const LS_REFRESH = "chat_refresh";
const LS_USER = "chat_user";

// ---------- 服务器地址 ----------
// 空 = 同源（vite dev proxy / 服务端托管静态文件时）；Tauri/LAN 场景填 http://host:28080
// 桌面端没有同源服务器，未配置时用正式部署地址
const DEFAULT_DESKTOP_SERVER = "https://chat.287152.xyz";
export function getServer(): string {
  const fallback = "__TAURI_INTERNALS__" in globalThis ? DEFAULT_DESKTOP_SERVER : "";
  return (localStorage.getItem(LS_SERVER) || fallback).replace(/\/+$/, "");
}
/** 本地快照/外发队列的分键域：同源部署没有显式服务器地址，用页面 origin */
export function serverScope(): string {
  return getServer() || (typeof location !== "undefined" ? location.origin : "");
}
export function setServer(url: string) {
  const next = url.trim().replace(/\/+$/, "");
  if (next !== getServer()) {
    localStorage.setItem(LS_SERVER, next);
    sessionGen++; // 服务器切换：旧服务器的异步结果一律作废
    boundIdentity = storedIdentity();
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

// localStorage 是所有标签页共享的：别的标签页换账号 / 换服务器 / 登出后，存储里的凭据就不再属于本页。
// 本页记住自己的身份（服务器 + 用户 id），对不上时一律不发请求，等页面重载换成新身份。
// 同一账号的令牌轮换不改身份，照常使用。
function storedUserId(): string {
  try {
    return JSON.parse(localStorage.getItem(LS_USER) ?? "null")?.id ?? "";
  } catch {
    return "";
  }
}
function storedIdentity(): string {
  return `${getServer()}|${storedUserId()}`;
}
let boundIdentity: string | null = null;
function identityMatches(): boolean {
  boundIdentity ??= storedIdentity();
  return storedIdentity() === boundIdentity;
}
function requireIdentity() {
  if (!identityMatches()) throw new ApiError(0, "session changed");
}
/** 别的标签页改了账号或服务器时回调（本页应重载）。 */
export function watchIdentity(onChange: () => void) {
  window.addEventListener("storage", (e) => {
    if (e.key !== null && e.key !== LS_USER && e.key !== LS_SERVER) return;
    if (!identityMatches()) onChange();
  });
}

export function saveSession(r: AuthResponse) {
  const prev = storedUserId();
  // 换号：清掉上一个账号在本地所有服务器键下的快照，防串号
  if (prev && prev !== r.user.id) removeLocalCacheForUser(localStorage, prev);
  localStorage.setItem(LS_ACCESS, r.access_token);
  localStorage.setItem(LS_REFRESH, r.refresh_token);
  localStorage.setItem(LS_USER, JSON.stringify(r.user));
  sessionGen++; // 新登录 = 新会话
  boundIdentity = storedIdentity();
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
  // 登出/换号/被踢：清掉本账号在本地所有服务器键下的快照
  const uid = storedUserId();
  if (uid) removeLocalCacheForUser(localStorage, uid);
  localStorage.removeItem(LS_ACCESS);
  localStorage.removeItem(LS_REFRESH);
  localStorage.removeItem(LS_USER);
  sessionGen++; // 登出 = 旧会话作废
  boundIdentity = storedIdentity();
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, msg: string) {
    super(msg);
    this.status = status;
  }
}

// 刷新结果：凭据确实失效才登出；断网、5xx、维护中只是暂时不可用，保留凭据等下次
type Refreshed = "ok" | "invalid" | "transient";
// 明确拒绝时的登出提示（invalid 才可能有）
type RefreshOutcome = { r: Refreshed; reason?: string };

let refreshing: { gen: number; promise: Promise<RefreshOutcome> } | null = null;

function requireSession(gen: number) {
  if (gen !== session()) throw new ApiError(0, "session changed");
}

// Web Locks 锁名：跨标签页串行化刷新，避免并发轮换互相消费掉对方的刷新令牌
const REFRESH_LOCK = "chat_token_refresh";

// refresh 的明确拒绝（核对响应体，代理/WAF 拦下的 4xx 不得误判成凭据失效）：
// 401 invalid refresh token / session expired…（无效、吊销、被挤下线）；
// 403 账号已被禁用。其余响应一律按暂时失败处理。
const REFRESH_DENIED_401 = new Set([
  "invalid refresh token",
  "session expired, please login again",
]);
const REFRESH_DENIED_403 = "账号已被禁用";
const LOGOUT_EXPIRED = "登录已失效，请重新登录";
const LOGOUT_DISABLED = "账号已被禁用，请联系管理员"; // 与 ws.ts kickReasonText("disabled") 同文案

/** 服务端明确拒收 refresh token 时的登出文案；非明确拒绝返回 null（按暂时失败） */
async function refreshDeniedReason(res: Response): Promise<string | null> {
  if (res.status !== 401 && res.status !== 403) return null;
  let err: unknown;
  try {
    err = (await res.json())?.error;
  } catch {
    return null; // 非 JSON 响应（代理/网关拦下）：只是连不上，不动凭据
  }
  if (res.status === 403) return err === REFRESH_DENIED_403 ? LOGOUT_DISABLED : null;
  return REFRESH_DENIED_401.has(String(err)) ? LOGOUT_EXPIRED : null;
}

/** 业务接口 401（已换新 access token 仍被拒）：本服务的 JSON 报文才算会话级拒绝，代理/WAF 的不算 */
async function serverDenied(res: Response): Promise<boolean> {
  try {
    const j = await res.clone().json();
    return !!j && typeof j.error === "string";
  } catch {
    return false;
  }
}

/** 服务端明确踢下线：清会话、记登录页提示、广播登出。别的标签页已换账号时不替它登出 */
function denySession(status: number, reason: string): never {
  requireIdentity();
  clearSession();
  try {
    sessionStorage.setItem("chat_kick_reason", reason);
  } catch {}
  window.dispatchEvent(new Event("chat:logout"));
  throw new ApiError(status, reason);
}

// 单次请求超时：服务端不可达（IP 不通/防火墙丢包）时不能无限挂起，否则启动页一直停在「加载中…」
const REQUEST_TIMEOUT_MS = 10_000;

function timedFetch(url: string, init?: RequestInit, timeoutMs = REQUEST_TIMEOUT_MS): Promise<Response> {
  if (timeoutMs <= 0 || typeof AbortController === "undefined") return fetch(url, init);
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new ApiError(0, "连接超时，请稍后再试")), timeoutMs);
  const outer = init?.signal ?? null;
  const onOuter = () => ctrl.abort(outer!.reason);
  if (outer) {
    if (outer.aborted) ctrl.abort(outer.reason);
    else outer.addEventListener("abort", onOuter, { once: true });
  }
  return fetch(url, { ...init, signal: ctrl.signal }).finally(() => {
    clearTimeout(timer);
    outer?.removeEventListener("abort", onOuter);
  });
}

// 单次刷新（在 Web Locks 内执行）：localStorage 是所有标签页共享的，
// 刷新令牌已被换成别的值 = 别的标签页刚刷新成功（或新登录），直接当作成功、
// 用存储里的新访问令牌重试即可，不再请求服务端也不清空会话
async function refreshOnce(gen: number, srv: string, rt: string): Promise<RefreshOutcome> {
  const swapped = () => {
    const cur = localStorage.getItem(LS_REFRESH);
    return cur !== null && cur !== rt;
  };
  if (swapped()) return { r: "ok" };
  let r: AuthResponse;
  try {
    const res = await timedFetch(`${srv}/v1/auth/refresh`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ refresh_token: rt }),
    });
    if (!res.ok) {
      if (swapped()) return { r: "ok" };
      const reason = await refreshDeniedReason(res);
      // 只有服务端明确拒绝（核对过响应体）才判 invalid，其余全部按暂时失败
      return reason ? { r: "invalid", reason } : { r: "transient" };
    }
    r = await res.json();
  } catch {
    return { r: swapped() ? "ok" : "transient" };
  }
  // 期间发生了登录/登出/换服务器：不能把旧凭据写回新会话
  if (gen !== session() || getServer() !== srv) return { r: "invalid" };
  if (localStorage.getItem(LS_REFRESH) !== rt) return { r: swapped() ? "ok" : "invalid" };
  localStorage.setItem(LS_ACCESS, r.access_token);
  localStorage.setItem(LS_REFRESH, r.refresh_token);
  localStorage.setItem(LS_USER, JSON.stringify(r.user));
  return { r: "ok" };
}

async function tryRefresh(gen: number, srv: string): Promise<RefreshOutcome> {
  requireSession(gen);
  requireIdentity();
  // 单飞：同一代会话的并发 401 共用一次刷新，避免旧令牌被重复提交触发盗用判定
  if (refreshing?.gen === gen) return refreshing.promise;
  const rt = localStorage.getItem(LS_REFRESH);
  if (!rt) return { r: "invalid" };
  const owner = { gen, promise: Promise.resolve<RefreshOutcome>({ r: "invalid" }) };
  refreshing = owner;
  const run = () => refreshOnce(gen, srv, rt);
  owner.promise = (async () => {
    try {
      const locks = typeof navigator !== "undefined" ? navigator.locks : undefined;
      if (locks) {
        try {
          return await locks.request(REFRESH_LOCK, run);
        } catch {
          // 锁服务本身异常：退化为无锁刷新
        }
      }
      return await run();
    } catch {
      return { r: "transient" } as RefreshOutcome;
    } finally {
      setTimeout(() => { if (refreshing === owner) refreshing = null; }, 0);
    }
  })();
  return owner.promise;
}

async function raw(srv: string, path: string, init?: RequestInit, timeoutMs?: number): Promise<Response> {
  requireIdentity();
  const headers: Record<string, string> = {
    ...(init?.headers as Record<string, string>),
  };
  const at = localStorage.getItem(LS_ACCESS);
  if (at) headers["authorization"] = `Bearer ${at}`;
  if (typeof init?.body === "string") headers["Content-Type"] = "application/json";
  return timedFetch(`${srv}/v1${path}`, { ...init, headers }, timeoutMs);
}

// 登录/注册的 401 是业务错误（密码错/邀请码无效），与会话凭据无关：不触发刷新、不登出
const NO_REFRESH_PATHS = new Set(["/auth/login", "/auth/register"]);

async function call<T>(path: string, init?: RequestInit, opts?: { timeoutMs?: number }): Promise<T> {
  const gen = session();
  const srv = getServer();
  let res = await raw(srv, path, init, opts?.timeoutMs);
  requireSession(gen);
  if (res.status === 401 && !NO_REFRESH_PATHS.has(path)) {
    const refreshed = await tryRefresh(gen, srv);
    requireSession(gen);
    if (refreshed.r === "transient") throw new ApiError(503, "暂时连不上服务器，请稍后再试");
    if (refreshed.r === "ok") {
      res = await raw(srv, path, init, opts?.timeoutMs);
      requireSession(gen);
    }
    // 只有服务端明确拒绝 refresh 或新 access token 才登出；
    // 代理/网关拦下的 401（非本服务报文）按暂时失败保留会话
    const reason =
      refreshed.r === "invalid"
        ? refreshed.reason ?? LOGOUT_EXPIRED
        : res.status === 401 && (await serverDenied(res))
          ? LOGOUT_EXPIRED
          : null;
    if (reason) denySession(401, reason);
    if (res.status === 401) throw new ApiError(503, "暂时连不上服务器，请稍后再试");
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  requireSession(gen);
  if (!res.ok) {
    let msg = text;
    try {
      msg = JSON.parse(text).error ?? text;
    } catch {}
    // 账号被禁用：服务端对本会话一切 API 一律 403——与 refresh 被拒同等对待，踢回登录页
    if (res.status === 403 && msg === REFRESH_DENIED_403 && !NO_REFRESH_PATHS.has(path)) {
      denySession(403, LOGOUT_DISABLED);
    }
    // 登录失败统一口语文案（服务端报文是英文代号 invalid credentials）
    if (path === "/auth/login" && res.status === 401) msg = "用户名或密码错误";
    // 服务端没有这个接口（客户端更新了、服务端还没更新）
    if (res.status === 404 && msg === "unknown_api") msg = "服务端版本较旧，不支持这个操作，请更新服务端";
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

// ---------- 设备名（V2 P1-5） ----------
// 登录/注册带给服务端的 device_label：Tauri 壳固定文案，Web 端按 UA 拼「浏览器 · 系统」。
export function osOfUa(ua: string): string {
  if (/Windows NT/i.test(ua)) return "Windows";
  if (/Android/i.test(ua)) return "Android";
  if (/iPhone|iPad|iPod/i.test(ua)) return "iOS";
  if (/Mac OS X/i.test(ua)) return "macOS";
  if (/Linux/i.test(ua)) return "Linux";
  return "未知系统";
}
function browserOfUa(ua: string): string {
  // Edge/Opera 的 UA 也含 Chrome/，必须先判
  if (/Edg(e|A|iOS)?\//i.test(ua)) return "Edge";
  if (/OPR\/|Opera/i.test(ua)) return "Opera";
  if (/Firefox\//i.test(ua)) return "Firefox";
  if (/Chrome\//i.test(ua)) return "Chrome";
  if (/Safari\//i.test(ua)) return "Safari";
  return "浏览器";
}
/** 纯函数：按 UA 生成 Web 端设备名（便于测试） */
export function browserDeviceLabel(ua: string): string {
  return `${browserOfUa(ua)} · ${osOfUa(ua)}`;
}
/** 当前设备的 device_label：Tauri Android → Chat Android；Tauri 桌面 → Chat 桌面端 (OS)；其余按 UA */
export function deviceLabel(): string {
  const w = globalThis as { __TAURI_INTERNALS__?: unknown; __TAURI__?: unknown; navigator?: { userAgent?: string } };
  const ua = w.navigator?.userAgent ?? "";
  if (w.__TAURI_INTERNALS__ || w.__TAURI__) {
    return /Android/i.test(ua) ? "Chat Android" : `Chat 桌面端 (${osOfUa(ua)})`;
  }
  return browserDeviceLabel(ua);
}

/** Tauri 桌面壳（Windows / macOS / Linux）；网页与 Android 为 false */
export function isDesktopShell(): boolean {
  const w = globalThis as { __TAURI_INTERNALS__?: unknown; __TAURI__?: unknown; navigator?: { userAgent?: string } };
  if (!w.__TAURI_INTERNALS__ && !w.__TAURI__) return false;
  return !/Android/i.test(w.navigator?.userAgent ?? "");
}

// ---------- auth ----------
export const auth = {
  register: (username: string, password: string, display_name?: string, inviteCode?: string) =>
    post<AuthResponse>("/auth/register", {
      username,
      password,
      display_name,
      device_label: deviceLabel(),
      invite_code: inviteCode,
    }),
  login: (username: string, password: string) =>
    post<AuthResponse>("/auth/login", { username, password, device_label: deviceLabel() }),
  logout: () => {
    if (!identityMatches()) return; // 存储里已是别的标签页的账号
    const rt = localStorage.getItem(LS_REFRESH);
    if (rt) post("/auth/logout", { refresh_token: rt }).catch(() => {});
    clearSession();
  },
  me: () => get<UserPublic>("/users/me"),
  updateMe: (display_name: string) => patch<UserPublic>("/users/me", { display_name }),
  /** 上传已裁好的头像；字段名 file，返回更新后的本人资料 */
  uploadAvatar: (file: Blob) => {
    const fd = new FormData();
    const ext = file.type === "image/png" ? "png" : file.type === "image/webp" ? "webp" : "jpg";
    fd.append("file", file, `avatar.${ext}`);
    return call<UserPublic>("/users/me/avatar", { method: "PUT", body: fd }, { timeoutMs: 0 });
  },
  removeAvatar: () => call<UserPublic>("/users/me/avatar", { method: "DELETE" }),
  /** 自助改密：返回为当前 session 新签发的凭据（其他 session 已被服务端撤销） */
  changePassword: (old_password: string, new_password: string) =>
    post<AuthResponse>("/users/me/password", { old_password, new_password }),
  /** 登录设备列表 */
  sessions: () => get<SessionView[]>("/users/me/sessions"),
  /** 移出一台设备（不能是当前会话） */
  revokeSession: (id: string) => del<void>(`/users/me/sessions/${id}`),
  /** 退出其他所有设备 */
  revokeOtherSessions: () => post<void>("/users/me/sessions/revoke-others"),
  /** 本人附件用量与配额（V2 P2-1） */
  storage: () => get<StorageView>("/users/me/storage"),
  /** ntfy 推送设置（V2 P3-3） */
  push: () => get<PushState>("/users/me/push"),
  /** 部分更新：只传要改的字段；服务器未配置时 enabled=true 返回 400 */
  updatePush: (patch: { enabled?: boolean; preview?: boolean }) =>
    put<PushState>("/users/me/push", patch),
  /** 登记一台手机，服务端给它生成订阅主题 */
  addPushDevice: (name: string) => post<PushState>("/users/me/push/devices", { name }),
  /** 移除一台手机，它的主题立即不再收到推送 */
  removePushDevice: (id: string) => del<PushState>(`/users/me/push/devices/${id}`),
  /** 给这台手机立即发一条测试推送；失败 502 */
  testPushDevice: (id: string) => post<PushState>(`/users/me/push/devices/${id}/test`),
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
  /** 更新本人偏好：只传要改的字段；返回最新偏好（幂等覆写） */
  setPref: (id: string, body: PrefUpdate) =>
    put<ConversationPref>(`/conversations/${id}/preferences`, body),
  /** 群主设置公告；null / 空字符串清空 */
  setAnnouncement: (id: string, text: string | null) =>
    put<AnnouncementView>(`/conversations/${id}/announcement`, { text }),
  pins: (id: string) => get<PinEntry[]>(`/conversations/${id}/pins`),
  pin: (id: string, messageId: string) => put<void>(`/conversations/${id}/pins/${messageId}`),
  unpin: (id: string, messageId: string) => del<void>(`/conversations/${id}/pins/${messageId}`),
  ticket: () => get<{ ticket: string; expires_in: number }>("/ws/ticket"),
};

// ---------- webhook bot ----------
export const bots = {
  list: () => get<BotView[]>("/bots"),
  /** 409 用户名已被占用；400 用户名格式 / 地址不对 / 数量上限 */
  create: (body: { username: string; display_name?: string; webhook_url?: string }) =>
    post<BotView>("/bots", body),
  patch: (id: string, body: { display_name?: string; webhook_url?: string }) =>
    patch<BotView>(`/bots/${id}`, body),
  resetSecret: (id: string) => post<BotView>(`/bots/${id}/secret`),
  addToken: (id: string, name: string) => post<BotView>(`/bots/${id}/tokens`, { name }),
  removeToken: (id: string, tokenId: string) =>
    del<BotView>(`/bots/${id}/tokens/${tokenId}`),
  remove: (id: string) => del<void>(`/bots/${id}`),
};

/** bot 的入站地址：带 Authorization: Bearer <令牌> POST JSON {"content": "..."} 即以 bot 身份发消息 */
export function hookUrl(): string {
  return `${getServer() || location.origin}/v1/hooks`;
}

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
  /** 转发/收藏（V2 P2-4）：targets ≤10（服务端去重）；client_op_id 幂等——
   * 结果未确认时只能用原值重试，否则会产生第二条转发消息 */
  forward: (id: string, conversation_ids: string[], client_op_id: string) =>
    post<ForwardResp>(`/messages/${id}/forward`, { conversation_ids, client_op_id }),
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

// ---------- 全局搜索（V2 P3-1） ----------
/** GET /v1/search 的查询参数：全部可选，但 q 或至少一个筛选必填（服务端强制 400） */
export interface SearchParams {
  q?: string;
  conversation_id?: string;
  sender_id?: string;
  kind?: "text" | "image" | "file" | "audio";
  /** RFC3339；含 from、不含 to */
  from?: string;
  to?: string;
  /** 翻页游标：上一页末条的 created_at + id */
  before?: string;
  before_id?: string;
  limit?: number;
}
export const searchApi = {
  messages: (p: SearchParams) => {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(p)) {
      if (v !== undefined && v !== "") qs.set(k, String(v));
    }
    return get<MessagesPage>(`/search?${qs}`);
  },
};

// ---------- files ----------
export const files = {
  /** 附件地址（能力 URL）：dl=true 强制下载；thumb=true 取 JPEG 缩略图（有则返回），默认 inline */
  url: (id: string, opts?: { dl?: boolean; thumb?: boolean }) => {
    const p = new URLSearchParams();
    if (opts?.dl) p.set("dl", "1");
    else if (opts?.thumb) p.set("thumb", "1");
    const q = p.toString();
    return `${getServer()}/v1/files/${id}${q ? `?${q}` : ""}`;
  },
  upload: async (
    conversationId: string,
    file: File,
    opts?: { durationMs?: number },
  ): Promise<AttachmentView> => {
    const fd = new FormData();
    fd.append("conversation_id", conversationId);
    fd.append("file", file, file.name);
    if (opts?.durationMs != null) fd.append("duration_ms", String(opts.durationMs));
    return call<AttachmentView>("/files", { method: "POST", body: fd }, { timeoutMs: 0 });
  },
};

// ---------- 公共服务器信息 ----------
/** 登录前探测注册模式。请求失败/404/字段缺失一律视为 open（兼容旧服务端） */
export async function serverInfo(server?: string): Promise<ServerInfo> {
  const srv = (server ?? getServer()).trim().replace(/\/+$/, "");
  try {
    const res = await timedFetch(`${srv}/v1/server-info`);
    if (!res.ok) return { registration_mode: "open" };
    const j = (await res.json()) as Partial<ServerInfo>;
    const m = j.registration_mode;
    // push_available 缺省（旧服务端）按未配置处理
    return {
      registration_mode: m === "invite" || m === "closed" ? m : "open",
      push_available: j.push_available === true,
      meeting_base_url: typeof j.meeting_base_url === "string" ? j.meeting_base_url : undefined,
    };
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
  // ---------- V2 P4-1 备份/恢复 ----------
  backups: () => get<BackupList>("/admin/backups"),
  createBackup: () => post<BackupCreated>("/admin/backups"),
  deleteBackup: (name: string) =>
    del<void>(`/admin/backups/${encodeURIComponent(name)}`),
  /** 浏览器 <a href> 不能带 Authorization：换一次性下载票据 */
  backupDownloadTicket: (name: string) =>
    post<BackupTicket>(
      `/admin/backups/${encodeURIComponent(name)}/download-ticket`,
    ),
  /**
   * 上传备份包恢复（confirm=RESTORE 由面板收口）。
   * 用 XMLHttpRequest 是为了 upload.onprogress 的进度回调。
   */
  restore: (file: File, onProgress?: (pct: number) => void) =>
    new Promise<RestoreResult>((resolve, reject) => {
      const fd = new FormData();
      fd.append("file", file, "backup.tar.gz");
      fd.append("confirm", "RESTORE");
      if (!identityMatches()) return reject(new ApiError(0, "session changed"));
      const xhr = new XMLHttpRequest();
      xhr.open("POST", `${getServer()}/v1/admin/restore`);
      const at = localStorage.getItem(LS_ACCESS);
      if (at) xhr.setRequestHeader("authorization", `Bearer ${at}`);
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && onProgress)
          onProgress(Math.round((e.loaded / e.total) * 100));
      };
      xhr.onload = () => {
        if (xhr.status >= 200 && xhr.status < 300) {
          try {
            resolve(JSON.parse(xhr.responseText) as RestoreResult);
          } catch {
            resolve({ tables: [], files: 0 });
          }
          return;
        }
        let msg = xhr.responseText;
        try {
          msg = (JSON.parse(xhr.responseText).error as string) ?? msg;
        } catch {}
        reject(new ApiError(xhr.status, msg || "恢复失败"));
      };
      xhr.onerror = () => reject(new ApiError(0, "网络错误"));
      xhr.send(fd);
    }),
};
