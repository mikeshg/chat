import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { auth, clearSession, convs, saveSession, setServer } from "./api";
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
