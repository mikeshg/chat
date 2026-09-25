import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { auth, browserDeviceLabel, clearSession, convs, deviceLabel, saveSession, setServer } from "./api";
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
