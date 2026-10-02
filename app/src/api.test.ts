import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { auth, browserDeviceLabel, clearSession, convs, deviceLabel, saveSession, setServer, watchIdentity } from "./api";
import type { AuthResponse } from "./types";

const login = (name: string): AuthResponse => ({ access_token: `${name}-access`, refresh_token: `${name}-refresh`, access_expires_in: 900, user: { id: name, username: name, display_name: name, role: "user", last_seen_at: null } });
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return { promise, resolve };
};

beforeEach(() => {
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", { getItem: (k: string) => storage.get(k) ?? null, setItem: (k: string, v: string) => storage.set(k, v), removeItem: (k: string) => storage.delete(k) });
  vi.stubGlobal("window", new EventTarget());
  saveSession(login("old"));
});
afterEach(() => { clearSession(); vi.unstubAllGlobals(); vi.useRealTimers(); });

describe("session-owned HTTP requests", () => {
  it.each([200, 204, 401])("rejects an old-session %s response without refresh or retry", async (status) => {
    const response = deferred<Response>();
    const fetcher = vi.fn(() => response.promise);
    vi.stubGlobal("fetch", fetcher);
    const request = convs.leave("old-conv");
    const rejected = expect(request).rejects.toThrow();
    saveSession(login("new"));
    setServer("https://new.example");
    response.resolve(new Response(status === 204 ? null : "{}", { status }));
    await rejected;
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(localStorage.getItem("chat_access")).toBe("new-access");
  });

  it("keeps refresh promises and finalizers isolated across generations", async () => {
    vi.stubGlobal("navigator", {}); // 无 Web Locks 环境（A25 起真实环境会串行排队刷新）
    vi.useFakeTimers();
    const oldRefresh = deferred<Response>();
    const newRefresh = deferred<Response>();
    const refreshTokens: string[] = [];
    vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
      if (url.endsWith("/auth/refresh")) {
        const token = JSON.parse(String(init?.body)).refresh_token;
        refreshTokens.push(token);
        return token === "old-refresh" ? oldRefresh.promise : newRefresh.promise;
      }
      const headers = init?.headers as Record<string, string>;
      return Promise.resolve(new Response(headers.authorization === "Bearer renewed-access" ? JSON.stringify(login("new").user) : "{}", { status: headers.authorization === "Bearer renewed-access" ? 200 : 401 }));
    }));
    const old = auth.me();
    const oldRejected = expect(old).rejects.toThrow();
    await vi.waitFor(() => expect(refreshTokens).toEqual(["old-refresh"]));
    saveSession(login("new"));
    const next = auth.me();
    await vi.waitFor(() => expect(refreshTokens).toEqual(["old-refresh", "new-refresh"]));
    oldRefresh.resolve(Response.json(login("old-renewed")));
    await oldRejected;
    await vi.runOnlyPendingTimersAsync();
    const parallel = auth.me();
    await Promise.resolve();
    newRefresh.resolve(Response.json({ ...login("new"), access_token: "renewed-access" }));
    expect(await next).toEqual(login("new").user);
    expect(await parallel).toEqual(login("new").user);
    expect(refreshTokens).toEqual(["old-refresh", "new-refresh"]);
  });
});

// ===== V2 P1-5 =====
describe("device labels", () => {
  const UA = {
    chrome: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    edge: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
    safari: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15",
    firefox: "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
    android: "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
  };
  it.each([
    [UA.chrome, "Chrome · Windows"],
    [UA.edge, "Edge · Windows"],
    [UA.safari, "Safari · macOS"],
    [UA.firefox, "Firefox · Linux"],
    [UA.android, "Chrome · Android"],
    ["", "浏览器 · 未知系统"],
  ])("browserDeviceLabel(%j) → %s", (ua, want) => {
    expect(browserDeviceLabel(ua)).toBe(want);
  });

  it("uses Tauri labels inside the shell", () => {
    vi.stubGlobal("navigator", { userAgent: UA.chrome });
    vi.stubGlobal("__TAURI_INTERNALS__", {});
    expect(deviceLabel()).toBe("Chat 桌面端 (Windows)");
    vi.stubGlobal("navigator", { userAgent: UA.android });
    expect(deviceLabel()).toBe("Chat Android");
  });

  it("falls back to UA parsing outside Tauri", () => {
    vi.stubGlobal("navigator", { userAgent: UA.safari });
    expect(deviceLabel()).toBe("Safari · macOS");
  });

  it("login/register carry the generated device_label", async () => {
    vi.stubGlobal("navigator", { userAgent: UA.chrome });
    const fetcher = vi.fn(async (_url: string, _init?: RequestInit) => Response.json(login("u")));
    vi.stubGlobal("fetch", fetcher);
    await auth.login("u", "secret123");
    expect(JSON.parse(String(fetcher.mock.calls[0][1]?.body)).device_label).toBe("Chrome · Windows");
    await auth.register("u", "secret123");
    expect(JSON.parse(String(fetcher.mock.calls[1][1]?.body)).device_label).toBe("Chrome · Windows");
  });
});

// ===== Agent A25 =====
describe("cross-tab refresh", () => {
  // 刷新换的是同一账号（beforeEach 登录的 old）的令牌
  const creds = (access: string, refresh: string): AuthResponse => ({
    ...login("old"),
    access_token: access,
    refresh_token: refresh,
  });
  /** 刷新响应手动放行的假后端：非刷新接口仅认 Bearer a2，其余 401 */
  const fakeBackend = () => {
    const refreshes: { token: string; respond: (r: Response) => void }[] = [];
    const authorized: string[] = [];
    const fetcher = vi.fn((url: string, init?: RequestInit) => {
      if (url.endsWith("/v1/auth/refresh")) {
        const d = deferred<Response>();
        refreshes.push({ token: JSON.parse(String(init?.body)).refresh_token, respond: d.resolve });
        return d.promise;
      }
      const at = (init?.headers as Record<string, string>).authorization;
      authorized.push(at);
      return Promise.resolve(
        at === "Bearer a2" ? Response.json(login("u").user) : new Response("{}", { status: 401 }),
      );
    });
    return { fetcher, refreshes, authorized };
  };
  /** 两个共享同一 localStorage 的独立模块实例，模拟两个标签页 */
  const freshPair = async () => {
    vi.resetModules();
    const a = await import("./api");
    vi.resetModules();
    const b = await import("./api");
    return [a, b];
  };

  it("keeps the rotated tokens when this tab's refresh is rejected", async () => {
    vi.stubGlobal("navigator", {}); // 无 Web Locks → 走「令牌已被换掉」退路
    const { fetcher, refreshes, authorized } = fakeBackend();
    vi.stubGlobal("fetch", fetcher);
    const [A, B] = await freshPair();
    const logout = vi.fn();
    window.addEventListener("chat:logout", logout);

    const ra = A.auth.me();
    const rb = B.auth.me();
    await vi.waitFor(() => expect(refreshes.length).toBe(2));
    // 先到的标签页刷新成功并写回 r2；后到的拿旧令牌被服务端拒绝
    refreshes[0].respond(Response.json(creds("a2", "r2")));
    await vi.waitFor(() => expect(localStorage.getItem("chat_refresh")).toBe("r2"));
    refreshes[1].respond(new Response("{}", { status: 401 }));
    expect(await ra).toEqual(login("u").user);
    expect(await rb).toEqual(login("u").user);
    expect(refreshes.map((r) => r.token)).toEqual(["old-refresh", "old-refresh"]);
    expect(localStorage.getItem("chat_access")).toBe("a2");
    expect(authorized.filter((a) => a === "Bearer a2").length).toBe(2);
    expect(logout).not.toHaveBeenCalled();
  });

  it("serializes refreshes through Web Locks so the server sees one request", async () => {
    let tail: Promise<unknown> = Promise.resolve();
    const request = vi.fn((_name: string, cb: () => Promise<boolean>) => {
      const p = tail.then(cb);
      tail = p.then(() => undefined, () => undefined);
      return p;
    });
    vi.stubGlobal("navigator", { locks: { request } });
    const { fetcher, refreshes } = fakeBackend();
    vi.stubGlobal("fetch", fetcher);
    const [A, B] = await freshPair();
    const logout = vi.fn();
    window.addEventListener("chat:logout", logout);

    const ra = A.auth.me();
    const rb = B.auth.me();
    // 第二个标签页拿到锁时发现令牌已换，不再请求服务端
    await vi.waitFor(() => expect(refreshes.length).toBe(1));
    refreshes[0].respond(Response.json(creds("a2", "r2")));
    expect(await ra).toEqual(login("u").user);
    expect(await rb).toEqual(login("u").user);
    expect(request).toHaveBeenCalledTimes(2);
    expect(refreshes.length).toBe(1);
    expect(localStorage.getItem("chat_access")).toBe("a2");
    expect(logout).not.toHaveBeenCalled();
  });

  it("clears the session and emits chat:logout when the token is untouched and the server rejects", async () => {
    vi.stubGlobal("navigator", {});
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status: 401 }))));
    const logout = vi.fn();
    window.addEventListener("chat:logout", logout);

    await expect(auth.me()).rejects.toMatchObject({ status: 401 });
    expect(localStorage.getItem("chat_refresh")).toBeNull();
    expect(localStorage.getItem("chat_access")).toBeNull();
    expect(logout).toHaveBeenCalledTimes(1);
  });
});

// 另一个标签页直接改写共享的 localStorage（不经过本模块）
const otherTabLogin = (name: string) => {
  const r = login(name);
  localStorage.setItem("chat_access", r.access_token);
  localStorage.setItem("chat_refresh", r.refresh_token);
  localStorage.setItem("chat_user", JSON.stringify(r.user));
};

describe("cross-tab identity", () => {
  it("never sends another tab's account token from this tab", async () => {
    const fetcher = vi.fn(() => Promise.resolve(new Response("{}", { status: 200 })));
    vi.stubGlobal("fetch", fetcher);
    otherTabLogin("bob");
    await expect(convs.rename("g1", "x")).rejects.toThrow("session changed");
    expect(fetcher).not.toHaveBeenCalled();
    // 也不能替别的标签页登出
    auth.logout();
    expect(localStorage.getItem("chat_refresh")).toBe("bob-refresh");
  });

  it("token rotation of the same account keeps working", async () => {
    const fetcher = vi.fn((_url: string, _init?: RequestInit) => Promise.resolve(new Response("{}", { status: 200 })));
    vi.stubGlobal("fetch", fetcher);
    otherTabLogin("old"); // 同一用户 id，只是换了令牌
    localStorage.setItem("chat_access", "rotated-access");
    await convs.rename("g1", "x");
    const headers = fetcher.mock.calls[0][1]?.headers as Record<string, string>;
    expect(headers.authorization).toBe("Bearer rotated-access");
  });

  it("notifies when another tab switches account, not on rotation", () => {
    const onChange = vi.fn();
    watchIdentity(onChange);
    const storageEvent = (key: string) => Object.assign(new Event("storage"), { key });
    localStorage.setItem("chat_access", "rotated");
    window.dispatchEvent(storageEvent("chat_access"));
    window.dispatchEvent(storageEvent("chat_user"));
    expect(onChange).not.toHaveBeenCalled();
    otherTabLogin("bob");
    window.dispatchEvent(storageEvent("chat_user"));
    expect(onChange).toHaveBeenCalledTimes(1);
  });
});
