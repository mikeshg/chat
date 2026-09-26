import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { clearSession, saveSession } from "./api";
import { getState, setState } from "./store";
import type { ConversationView, MessageView } from "./types";
import {
  clientActive,
  kickReasonText,
  reportClientState,
  sendChat,
  startWs,
  stopWs,
} from "./ws";

const me = { id: "me", username: "me", display_name: "Me", role: "user" as const, last_seen_at: null };
const message = (seq: number, mentions: string[] = []): MessageView => ({ id: `m${seq}`, seq, conversation_id: "c", sender_id: "other", sender_username: "other", sender_name: "Other", content: "hello", kind: "text", attachment: null, mentions, created_at: "2026-01-01T00:00:00Z" });
const conversation = (): ConversationView => ({ id: "c", kind: "group", name: "C", owner_id: "me", member_count: 2, peer: null, last_seq: 0, unread_count: 0, has_mention: false, last_message: null, created_at: "2026-01-01T00:00:00Z" });
class Socket {
  static OPEN = 1;
  static all: Socket[] = [];
  readyState = 1;
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  sent: Record<string, unknown>[] = [];
  constructor() { Socket.all.push(this); }
  send(data: string) { this.sent.push(JSON.parse(data)); }
  close() { this.readyState = 3; this.onclose?.(); }
  frame(data: unknown) { this.onmessage?.({ data: JSON.stringify(data) }); }
}
let refreshStatus = 200;
beforeEach(() => {
  vi.useFakeTimers();
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", { getItem: (k: string) => storage.get(k) ?? null, setItem: (k: string, v: string) => storage.set(k, v), removeItem: (k: string) => storage.delete(k) });
  vi.stubGlobal("window", { setTimeout, dispatchEvent: vi.fn() });
  vi.stubGlobal("location", { origin: "http://localhost" });
  vi.stubGlobal("WebSocket", Socket);
  Socket.all = [];
  refreshStatus = 200;
  vi.stubGlobal("fetch", vi.fn(async (url: string) => {
    if (url.endsWith("/ws/ticket")) return Response.json({ ticket: "t" });
    if (url.endsWith("/messages/refresh")) return new Response(refreshStatus === 200 ? JSON.stringify({ messages: [], has_more: false }) : "", { status: refreshStatus });
    if (url.endsWith("/conversations/c")) return Response.json({ id: "c" });
    if (url.endsWith("/conversations")) return Response.json(getState().conversations);
    if (url.endsWith("/friends/requests")) return Response.json({ incoming: [], outgoing: [] });
    return Response.json([]);
  }));
  saveSession({ user: me, access_token: "a", refresh_token: "r", access_expires_in: 900 });
  setState({ me, conversations: [conversation()], messages: {}, historyLoaded: {}, hasMore: {}, activeConv: null, tab: "chats", toasts: [], typing: {} });
});
afterEach(() => { stopWs(); clearSession(); vi.clearAllTimers(); vi.useRealTimers(); vi.unstubAllGlobals(); });
const open = async () => {
  await startWs();
  const socket = Socket.all[Socket.all.length - 1];
  socket.onopen?.();
  await vi.waitFor(() => expect(getState().wsReady).toBe(true));
  await vi.advanceTimersByTimeAsync(0);
  return socket;
};

describe("WebSocket delivery", () => {
  it("sends each outstanding item once per connection and replays only on reconnect", async () => {
    const first = await open();
    sendChat("c", "one"); sendChat("c", "two"); sendChat("c", "three");
    const sent = first.sent.filter((f) => f.type === "send_message");
    expect(sent).toHaveLength(3);
    first.frame({ type: "message_ack", client_msg_id: sent[0].client_msg_id, message: message(1) });
    expect(getState().messages.c.filter((m) => m.id.startsWith("client:"))).toHaveLength(2);
    first.close();
    await vi.advanceTimersByTimeAsync(1000);
    const second = Socket.all[Socket.all.length - 1];
    second.onopen?.();
    await vi.advanceTimersByTimeAsync(0);
    expect(second.sent.filter((f) => f.type === "send_message").map((f) => f.client_msg_id)).toEqual(sent.slice(1).map((f) => f.client_msg_id));
    sendChat("c", "four");
    expect(second.sent.filter((f) => f.type === "send_message")).toHaveLength(3);
  });

  it("does not toast or count a duplicate message twice", async () => {
    const sock = await open();
    sock.frame({ type: "message", message: message(1) });
    sock.frame({ type: "message", message: message(1) });
    expect(getState().toasts).toHaveLength(1);
    expect(getState().conversations[0].unread_count).toBe(1);
  });

  it("read_sync clears only the covered range and preserves later mentions", async () => {
    const sock = await open();
    sock.frame({ type: "message", message: message(1) });
    sock.frame({ type: "message", message: message(2, ["me"]) });
    sock.frame({ type: "read_sync", conversation_id: "c", seq: 1 });
    expect(getState().conversations[0]).toMatchObject({ unread_count: 1, has_mention: true });
    sock.frame({ type: "read_sync", conversation_id: "c", seq: 2 });
    expect(getState().conversations[0]).toMatchObject({ unread_count: 0, has_mention: false });
  });

  it("preserves history when a legacy server lacks refresh but still grants conversation access", async () => {
    refreshStatus = 404;
    setState({ messages: { c: [message(1)] }, historyLoaded: { c: true } });
    await open();
    await vi.advanceTimersByTimeAsync(0);
    expect(getState().messages.c).toEqual([message(1)]);
    expect(getState().historyLoaded.c).toBe(true);
  });
});

// ===== V2 P3-3 =====
describe("client_state reporting", () => {
  const stubDoc = (hidden: boolean, focus: boolean) =>
    vi.stubGlobal("document", {
      hidden,
      hasFocus: () => focus,
      addEventListener: vi.fn(),
    });
  const states = (sock: Socket) => sock.sent.filter((f) => f.type === "client_state");

  it("clientActive = 页面可见且窗口有焦点", () => {
    stubDoc(false, true);
    expect(clientActive()).toBe(true);
    stubDoc(true, true);
    expect(clientActive()).toBe(false);
    stubDoc(false, false);
    expect(clientActive()).toBe(false);
  });

  it("reports current state once immediately on connect", async () => {
    stubDoc(false, true);
    const sock = await open();
    expect(states(sock)).toEqual([{ type: "client_state", active: true }]);
  });

  it("debounces rapid changes into one frame with the latest value", async () => {
    stubDoc(false, true);
    const sock = await open();
    stubDoc(true, false); // 切到后台
    reportClientState();
    reportClientState();
    reportClientState();
    await vi.advanceTimersByTimeAsync(499);
    expect(states(sock)).toHaveLength(1); // 去抖窗口内不重复发
    await vi.advanceTimersByTimeAsync(1);
    expect(states(sock)).toHaveLength(2);
    expect(states(sock)[1]).toEqual({ type: "client_state", active: false });
  });

  it("does not resend an unchanged value", async () => {
    stubDoc(false, true);
    const sock = await open();
    reportClientState();
    reportClientState();
    await vi.advanceTimersByTimeAsync(600);
    expect(states(sock)).toHaveLength(1); // 只有连接建立时的那一帧
  });

  it("reports again on a new connection", async () => {
    stubDoc(true, false);
    const first = await open();
    expect(states(first)).toEqual([{ type: "client_state", active: false }]);
    first.close();
    await vi.advanceTimersByTimeAsync(1000);
    const second = Socket.all[Socket.all.length - 1];
    stubDoc(false, true); // 重连时已回前台
    second.onopen?.();
    await vi.advanceTimersByTimeAsync(0);
    expect(states(second)).toEqual([{ type: "client_state", active: true }]);
  });
});

// ===== V2 P1-5 =====
describe("force_logout reason copy", () => {
  it.each([
    ["disabled", "账号已被禁用，请联系管理员"],
    ["password_reset", "密码已被管理员重置，请重新登录"],
    ["password_changed", "密码已在其他设备修改，请重新登录"],
    ["session_revoked", "该设备已被移出登录，请重新登录"],
    ["unknown_reason", "登录已失效，请重新登录"],
  ])("kickReasonText(%j) → %s", (reason, want) => {
    expect(kickReasonText(reason)).toBe(want);
  });
});
