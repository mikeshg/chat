import { describe, expect, it } from "vitest";
import {
  applyHostStatus,
  applyInvocations,
  applyLiveMessage,
  applyPrefUpdate,
  applyRefreshed,
  applySyncMessages,
  applyUpdatedMessage,
  convTitle,
  fetchForward,
  invocationCancellable,
  invocationErrorText,
  invocationStatusLabel,
  isOwnMessage,
  mergeConversationLists,
  resolveAck,
  shouldNotify,
  sortConversations,
  type AppState,
} from "./store";
import type {
  ConversationPref,
  ConversationView,
  MessageView,
  PollView,
} from "./types";

/**
 * §8 反序交付回归：所有 mutation 帧携带完整 MessageView，客户端按
 * (id, edit_version) 整视图合并。版本更高的视图包含低版本全部变更，
 * 迟到的旧版本帧直接丢弃，正文/墓碑/reaction 互不吞掉。
 */

const CONV = "conv-1";

function msg(patch: Partial<MessageView> = {}): MessageView {
  return {
    id: "m1",
    conversation_id: CONV,
    seq: 1,
    sender_id: "u-a",
    sender_username: "ua",
    sender_name: "A",
    content: "v0",
    kind: "text",
    attachment: null,
    mentions: [],
    reply_to_id: null,
    reply_to_preview: null,
    edited_at: null,
    edit_version: 0,
    deleted_at: null,
    reactions: [],
    created_at: "2026-01-01T00:00:00Z",
    ...patch,
  };
}

function state(patch: Partial<AppState> = {}): AppState {
  return {
    me: null,
    conversations: [
      {
        id: CONV,
        kind: "dm",
        name: null,
        owner_id: null,
        peer: null,
        member_count: 2,
        last_seq: 1,
        unread_count: 0,
        has_mention: false,
        last_message: null,
        created_at: "2026-01-01T00:00:00Z",
      } satisfies ConversationView,
    ],
    messages: {},
    hasMore: {},
    historyLoaded: {},
    activeConv: null,
    online: {},
    typing: {},
    friends: [],
    requests: { incoming: [], outgoing: [] },
    wsReady: true,
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
    ...patch,
  };
}

function apply(s: AppState, m: MessageView): AppState {
  return { ...s, ...applyUpdatedMessage(s, m) };
}

describe("生命周期整视图版本合并（§8 反序安全）", () => {
  it("reaction_update(v2) 先于 message_updated(v1) 到达：正文与 reaction 都保留", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg()) };

    // v2 帧先到：完整视图（含 v1 的编辑结果 + reaction）
    const v2 = msg({ content: "edited", edited_at: "t1", edit_version: 2, reactions: [{ emoji: "👍", count: 1, reacted_by_me: false }] });
    s = apply(s, v2);
    // v1 编辑帧迟到：被版本守卫丢弃，不得覆盖正文或清空 reaction
    const v1 = msg({ content: "edited", edited_at: "t1", edit_version: 1 });
    s = apply(s, v1);

    const m = s.messages[CONV][0];
    expect(m.edit_version).toBe(2);
    expect(m.content).toBe("edited");
    expect(m.reactions).toHaveLength(1);
  });

  it("message_deleted(v3) 先于 reaction_update(v2) 到达：墓碑态不被旧帧还原", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg({ edit_version: 2 })) };

    // v3 删除帧先到：完整墓碑视图
    const v3 = msg({ content: "消息已删除", edit_version: 3, deleted_at: "t3", reactions: [] });
    s = apply(s, v3);
    // v2 reaction 帧迟到（删除前的快照）：丢弃
    const v2 = msg({ content: "edited", edit_version: 2, reactions: [{ emoji: "👍", count: 1, reacted_by_me: false }] });
    s = apply(s, v2);

    const m = s.messages[CONV][0];
    expect(m.edit_version).toBe(3);
    expect(m.deleted_at).toBe("t3");
    expect(m.content).toBe("消息已删除");
    expect(m.reactions).toHaveLength(0);
  });

  it("正序到达：v1 编辑 → v2 reaction → v3 删除逐帧合并", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg()) };
    s = apply(s, msg({ content: "edited", edit_version: 1, edited_at: "t1" }));
    s = apply(
      s,
      msg({
        content: "edited",
        edit_version: 2,
        edited_at: "t1",
        reactions: [{ emoji: "😂", count: 2, reacted_by_me: true }],
      }),
    );
    s = apply(s, msg({ content: "消息已删除", edit_version: 3, deleted_at: "t3", reactions: [] }));

    const m = s.messages[CONV][0];
    expect(m.edit_version).toBe(3);
    expect(m.deleted_at).toBe("t3");
    expect(m.reactions).toHaveLength(0);
  });

  it("同版本重复投递幂等：不抖动状态", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg()) };
    const v1 = msg({ content: "edited", edit_version: 1, edited_at: "t1" });
    s = apply(s, v1);
    const once = s.messages[CONV][0];
    s = apply(s, v1);
    expect(s.messages[CONV][0]).toEqual(once);
  });

  it("last_message 随版本换新；旧版本不回退会话卡片", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg()) };
    s = apply(s, msg({ content: "edited", edit_version: 2, deleted_at: null }));
    expect(s.conversations[0].last_message?.edit_version).toBe(2);
    s = apply(s, msg({ edit_version: 1 }));
    expect(s.conversations[0].last_message?.edit_version).toBe(2);
    expect(s.conversations[0].last_message?.content).toBe("edited");
  });
});

describe("acceptance: stale creation snapshots", () => {
  it.each([false, true])("keeps v3 in thread and same-seq previews (deleted=%s)", (deleted) => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, msg()) };
    const stale = s.conversations;
    const latest = msg({ edit_version: 3, content: deleted ? "消息已删除" : "edited-v3", deleted_at: deleted ? "t3" : null });
    s = apply(s, latest);
    expect(mergeConversationLists(s.conversations, stale)[0].last_message).toEqual(latest);
    for (const patch of [applyLiveMessage(s, msg()), applySyncMessages(s, CONV, [msg()]), applyRefreshed(s, CONV, [msg()])]) {
      expect(patch.messages?.[CONV][0]).toEqual(latest);
      expect(patch.conversations?.[0].last_message).toEqual(latest);
    }
  });
});

describe("acceptance: dependent reply previews", () => {
  it.each([false, true])("refreshes every cached reply and last_message from the latest source (deleted=%s)", (deleted) => {
    const source = msg();
    const reply = msg({ id: "reply", seq: 2, content: "reply body", reply_to_id: source.id, reply_to_preview: { message_id: source.id, seq: 1, sender_name: "A", excerpt: "v0", kind: "text", deleted: false } });
    let s = state();
    s = { ...s, ...applyLiveMessage(s, source) };
    s = { ...s, ...applyLiveMessage(s, reply) };
    const latest = msg({ edit_version: 3, content: deleted ? "消息已删除" : "𠀀".repeat(121), deleted_at: deleted ? "t3" : null });
    for (const patch of [applyUpdatedMessage(s, latest), applyRefreshed(s, CONV, [latest, reply]), applySyncMessages(s, CONV, [latest, reply])]) {
      const expected = { ...reply.reply_to_preview, excerpt: deleted ? "" : "𠀀".repeat(120), deleted };
      expect(patch.messages?.[CONV].find((m) => m.id === reply.id)?.reply_to_preview).toEqual(expected);
      expect(patch.conversations?.[0].last_message?.reply_to_preview).toEqual(expected);
      expect(patch.messages?.[CONV].map((m) => m.id)).toEqual(["m1", "reply"]);
    }
  });

  it("accepts same-version reply metadata without replacing canonical content", () => {
    const reply = msg({ id: "reply", edit_version: 3, content: "canonical", reply_to_id: "missing-source" });
    const fresh = { message_id: "missing-source", seq: 1, sender_name: "A", excerpt: "", kind: "text", deleted: true };
    const s = state({ messages: { [CONV]: [reply] }, conversations: [conv(CONV, { last_message: reply })] });
    const patch = applyRefreshed(s, CONV, [{ ...reply, content: "stale-content", reply_to_preview: fresh }]);
    expect(patch.messages?.[CONV][0].content).toBe("canonical");
    expect(patch.messages?.[CONV][0].reply_to_preview).toEqual(fresh);
    expect(patch.conversations?.[0].last_message).toEqual(patch.messages?.[CONV][0]);
  });

  it("ACK never downgrades a canonical tombstone or loses an ACK-only message", () => {
    const latest = msg({ edit_version: 3, deleted_at: "t3", content: "消息已删除" });
    expect(resolveAck([msg({ id: "client:c" }), latest], "c", msg())).toEqual([latest]);
    expect(resolveAck([], "c", msg())).toEqual([msg()]);
  });
});

// ---------- 会话偏好（§8.3） ----------

function pref(patch: Partial<ConversationPref> = {}): ConversationPref {
  return {
    pinned_at: null,
    archived_at: null,
    muted_until: null,
    mute_forever: false,
    updated_at: null,
    ...patch,
  };
}

function conv(id: string, patch: Partial<ConversationView> = {}): ConversationView {
  return {
    id,
    kind: "group",
    name: id,
    owner_id: null,
    peer: null,
    member_count: 2,
    last_seq: 0,
    unread_count: 0,
    has_mention: false,
    last_message: null,
    preference: null,
    created_at: "2026-01-01T00:00:00Z",
    ...patch,
  };
}

describe("会话偏好：排序 / 合并 / 提示抑制", () => {
  it("rejects stale preferences via events and catalog, including conversations without previews", () => {
    const latest = pref({ updated_at: "2026-09-25T10:03:00Z", mute_forever: true, archived_at: "2026-09-25T10:03:00Z" });
    const old = pref({ updated_at: "2026-09-25T10:01:00Z" });
    const s = state({ conversations: [conv(CONV, { preference: latest })] });
    expect(applyPrefUpdate(s, CONV, old).conversations?.[0].preference).toEqual(latest);
    expect(mergeConversationLists(s.conversations, [conv(CONV, { preference: old })])[0].preference).toEqual(latest);
    expect(mergeConversationLists(s.conversations, [conv(CONV)])[0].preference).toEqual(latest);
  });

  const at = (id: string, created: string, pinned_at: string | null = null) =>
    conv(id, { created_at: created, preference: pref({ pinned_at }) });

  it("置顶块在前按 pinned_at DESC；未置顶保持活动时间 DESC；id 兜底", () => {
    const list = sortConversations([
      at("a", "2026-01-03T00:00:00Z"),
      at("b", "2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z"),
      at("c", "2026-01-02T00:00:00Z", "2026-02-02T00:00:00Z"),
      at("d", "2026-01-04T00:00:00Z"),
    ]);
    // 置顶块：c(pinned 02-02) 在 b(pinned 02-01) 前；未置顶按活动：d > a
    expect(list.map((c) => c.id)).toEqual(["c", "b", "d", "a"]);
  });

  it("applyPrefUpdate：更新字段并重排；未知会话不动作", () => {
    let s = state({
      conversations: [at("a", "2026-01-03T00:00:00Z"), at("d", "2026-01-04T00:00:00Z")],
    });
    s = { ...s, ...applyPrefUpdate(s, "a", pref({ pinned_at: "2026-02-01T00:00:00Z" })) };
    expect(s.conversations.map((c) => c.id)).toEqual(["a", "d"]);
    const before = s.conversations;
    s = { ...s, ...applyPrefUpdate(s, "ghost", pref({ pinned_at: "x" })) };
    expect(s.conversations).toBe(before);
  });

  it("applyLiveMessage 后置顶仍在前（重排不丢 pin）", () => {
    let s = state({
      me: { id: "me", username: "me", display_name: "me", role: "user", last_seen_at: null },
      conversations: [
        at("pin", "2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z"),
        at("b", "2026-01-03T00:00:00Z"),
      ],
    });
    s = { ...s, ...applyLiveMessage(s, msg({ conversation_id: "b", sender_id: "u-b" })) };
    expect(s.conversations[0].id).toBe("pin");
    expect(s.conversations[1].unread_count).toBe(1);
  });

  it("shouldNotify：静音抑制、@我突破、本人/当前会话不提示", () => {
    const me = { id: "me", username: "me", display_name: "me", role: "user" as const, last_seen_at: null };
    const mutedConv = conv("m", {
      preference: pref({ muted_until: "2999-01-01T00:00:00Z" }),
    });
    const foreverConv = conv("f", { preference: pref({ mute_forever: true }) });
    const expiredConv = conv("e", {
      preference: pref({ muted_until: "2000-01-01T00:00:00Z" }),
    });
    const normal = conv("n");
    const s = state({ me, conversations: [mutedConv, foreverConv, expiredConv, normal] });
    const fromB = (cid: string, mentions: string[] = []) =>
      msg({ conversation_id: cid, sender_id: "u-b", mentions });

    expect(shouldNotify(s, fromB("n"))).toBe(true);
    expect(shouldNotify(s, fromB("m"))).toBe(false); // 定时静音
    expect(shouldNotify(s, fromB("f"))).toBe(false); // 永久静音
    expect(shouldNotify(s, fromB("e"))).toBe(true); // 过期静音不抑制
    expect(shouldNotify(s, fromB("f", ["me"]))).toBe(true); // @我突破
    expect(shouldNotify(s, fromB("n"))).toBe(true);
    // 本人消息不提示
    expect(shouldNotify(s, msg({ conversation_id: "n", sender_id: "me" }))).toBe(false);
    // 当前打开会话不提示
    const open = { ...s, activeConv: "n" };
    expect(shouldNotify(open, fromB("n"))).toBe(false);
    expect(shouldNotify(open, fromB("m"))).toBe(false);
  });
});

// ---------- 轻量投票（§8.4）：全视图版本合并 ----------

function pollView(patch: Partial<PollView> = {}): PollView {
  return {
    question: "q",
    allow_multiple: false,
    anonymous: false,
    closed_at: null,
    expires_at: null,
    expired: false,
    version: 0,
    total_voters: 0,
    options: [
      { id: "o1", index: 0, label: "x", count: 0, voters: [] },
      { id: "o2", index: 1, label: "y", count: 0, voters: [] },
    ],
    my_option_ids: [],
    ...patch,
  };
}

describe("投票消息：message_updated 整视图版本合并", () => {
  const pollMsg = (v: number, pv: Partial<PollView>) =>
    msg({ kind: "poll", content: "[投票] q", edit_version: v, poll: pollView({ version: v, ...pv }) });

  it("投票创建帧（message）携带 poll 视图；投票更新升版本替换", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, pollMsg(0, {})) };
    expect(s.messages[CONV][0].poll?.total_voters).toBe(0);

    s = apply(s, pollMsg(1, { total_voters: 1, my_option_ids: ["o1"] }));
    const m = s.messages[CONV][0];
    expect(m.edit_version).toBe(1);
    expect(m.poll?.my_option_ids).toEqual(["o1"]);
  });

  it("反序安全：关票帧(v2)先到，投票帧(v1)迟到被丢弃不回滚", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, pollMsg(0, {})) };
    // v2 关闭帧先到：完整视图（含投票结果 + closed_at）
    s = apply(
      s,
      pollMsg(2, { closed_at: "t2", total_voters: 1 }),
    );
    // v1 投票帧迟到：丢弃，closed_at 不得被还原
    s = apply(s, pollMsg(1, { total_voters: 1 }));
    const m = s.messages[CONV][0];
    expect(m.edit_version).toBe(2);
    expect(m.poll?.closed_at).toBe("t2");
    expect(m.poll?.version).toBe(2);
  });

  it("墓碑掩去 poll：删除帧后 poll 为 null 且不被旧帧还原", () => {
    let s = state();
    s = { ...s, ...applyLiveMessage(s, pollMsg(0, {})) };
    s = apply(s, msg({ kind: "poll", content: "消息已删除", edit_version: 3, deleted_at: "t3", poll: null }));
    const m = s.messages[CONV][0];
    expect(m.poll).toBeNull();
    s = apply(s, pollMsg(1, { total_voters: 1 }));
    expect(s.messages[CONV][0].poll).toBeNull();
  });
});

describe("V2 P1-1 后台时打开着的会话照常计未读", () => {
  const unread = (s: Partial<AppState>) => s.conversations?.find((c) => c.id === CONV)?.unread_count;
  it("前台：打开着的会话不计未读", () => {
    const s = state({ activeConv: CONV, me: { id: "me" } as AppState["me"] });
    expect(unread(applyLiveMessage(s, msg({ seq: 2 })))).toBe(0);
  });
  it("后台：打开着的会话计未读，标题与通知才准确", () => {
    const s = state({ activeConv: CONV, me: { id: "me" } as AppState["me"] });
    expect(unread(applyLiveMessage(s, msg({ seq: 2 }), false))).toBe(1);
  });
});

// ---------- Agent A01：归属判定 / 会话标题 / 宿主与调用状态 ----------

describe("Agent A01：isOwnMessage", () => {
  const me = "me";
  it("无 agent 字段：sender_id 是本人 → 我发的", () => {
    expect(isOwnMessage(msg({ sender_id: me }), me)).toBe(true);
    expect(isOwnMessage(msg({ sender_id: "u-b" }), me)).toBe(false);
    expect(isOwnMessage(msg({ sender_id: me }), null)).toBe(false);
    expect(isOwnMessage(msg({ sender_id: me }), undefined)).toBe(false);
  });
  it("有 agent 字段：sender_id 仍是主人，但不算我发的", () => {
    const m = msg({ sender_id: me, agent: { id: "ag1", name: "helper" } });
    expect(isOwnMessage(m, me)).toBe(false);
    // 顺带：agent 消息会给主人计未读、可触发提示
    const s = state({ me: { id: me } as AppState["me"] });
    const next = { ...s, ...applyLiveMessage(s, m, false) };
    expect(next.conversations?.[0].unread_count).toBe(1);
    expect(shouldNotify(s, m)).toBe(true);
  });
  it("agent: null 与缺省等价（旧服务端形状）", () => {
    expect(isOwnMessage(msg({ sender_id: me, agent: null }), me)).toBe(true);
  });
});

describe("Agent A01：convTitle", () => {
  it("kind='agent' → Agent 名；缺 agent 字段兜底 'Agent'", () => {
    expect(
      convTitle(
        conv("c", {
          kind: "agent",
          name: null,
          agent: { id: "a1", name: "小助手", host_id: "h1", host_online: true },
        }),
      ),
    ).toBe("小助手");
    expect(convTitle(conv("c", { kind: "agent", name: null }))).toBe("Agent");
  });
  it("既有 kind 不受影响（self/dm/group 回归）", () => {
    expect(convTitle(conv("c", { kind: "self", name: null }))).toBe("文件传输助手");
    expect(convTitle(conv("c", { kind: "group", name: "g" }))).toBe("g");
    expect(
      convTitle(
        conv("c", {
          kind: "dm",
          peer: { id: "p", username: "p", display_name: "P", role: "user", last_seen_at: null },
        }),
      ),
    ).toBe("P");
  });
});

describe("Agent A01：宿主状态与调用记录", () => {
  const agentConv = () =>
    conv("ac", {
      kind: "agent",
      name: null,
      agent: { id: "a1", name: "helper", host_id: "h1", host_online: true },
    });

  it("applyHostStatus：宿主上下线同步翻转宿主表与 agent 会话的在线点", () => {
    let s = state({
      conversations: [agentConv()],
      agentHosts: [
        { id: "h1", label: "pc", platform: "windows", online: true, last_seen_at: "t" },
      ],
    });
    s = { ...s, ...applyHostStatus(s, "h1", false) };
    expect(s.agentHosts[0].online).toBe(false);
    expect(s.conversations[0].agent?.host_online).toBe(false);
    // 别的宿主不受影响
    s = { ...s, ...applyHostStatus(s, "other", false) };
    expect(s.conversations[0].agent?.host_online).toBe(false);
  });

  it("applyInvocations：按 trigger_message_id 归档，后到的状态覆盖先到的", () => {
    const running = {
      id: "i1",
      agent_id: "a1",
      conversation_id: "ac",
      trigger_message_id: "m1",
      status: "running",
      error: null,
      created_at: "t",
      started_at: "t",
      finished_at: null,
    };
    let s = state({ invocations: { ac: { m1: running } } });
    const done = { ...running, status: "succeeded", finished_at: "t2" };
    s = { ...s, ...applyInvocations(s, [done]) };
    expect(s.invocations.ac.m1.status).toBe("succeeded");
    // 空批量不动状态
    expect(applyInvocations(s, [])).toEqual({});
  });

  it("invocationErrorText：稳定错误码 → 中文文案", () => {
    expect(invocationErrorText("host_offline")).toBe("Agent 所在电脑不在线");
    expect(invocationErrorText("host_disconnected")).toBe("Agent 所在电脑断开了");
    expect(invocationErrorText("agent_deleted")).toBe("该 Agent 已删除");
    expect(invocationErrorText("boom")).toBe("boom");
    expect(invocationErrorText(null)).toBe("未知原因");
  });

  it("invocationStatusLabel：排队 / 取消 / 超时按 status 查表", () => {
    expect(invocationStatusLabel("queued", null)).toBe("⏸ 排队中：Agent 所在电脑不在线");
    expect(invocationStatusLabel("cancelled", null)).toBe("已取消");
    expect(invocationStatusLabel("timed_out", null)).toBe("排队超时，已取消");
    expect(invocationStatusLabel("running", null)).toBe("⏳ 正在处理");
    expect(invocationStatusLabel("failed", "host_disconnected")).toBe(
      "⚠ 失败：Agent 所在电脑断开了",
    );
    expect(invocationStatusLabel("succeeded", null)).toBeNull();
    expect(invocationStatusLabel("awaiting_approval", null)).toBeNull();
  });

  it("invocationCancellable：排队、运行中、等待审批可取消", () => {
    expect(invocationCancellable("queued")).toBe(true);
    expect(invocationCancellable("running")).toBe(true);
    expect(invocationCancellable("awaiting_approval")).toBe(true);
    expect(invocationCancellable("succeeded")).toBe(false);
    expect(invocationCancellable("cancelled")).toBe(false);
    expect(invocationCancellable("timed_out")).toBe(false);
    expect(invocationCancellable("failed")).toBe(false);
  });
});

describe("跳转补齐：fetchForward 拉到缓存最早一条为止", () => {
  // 服务端 after_seq 分页：seq 1..1000，每页 200
  const page = async (after: number) => {
    const messages = Array.from({ length: Math.min(200, 1000 - after) }, (_, i) => msg({ id: "m" + (after + i + 1), seq: after + i + 1 }));
    return { messages, has_more: messages.length === 200 };
  };

  it("远处跳转：结果连续且接上缓存（无断档）", async () => {
    const got = await fetchForward(page, 75, 951);
    expect(got[0].seq).toBe(76);
    got.forEach((m, i) => expect(m.seq).toBe(76 + i));
    expect(got[got.length - 1].seq).toBeGreaterThanOrEqual(950);
  });

  it("离缓存近：一页就停", async () => {
    let calls = 0;
    await fetchForward(async (a) => (calls++, page(a)), 900, 951);
    expect(calls).toBe(1);
  });

  it("没有更多时停止（补到最新）", async () => {
    const got = await fetchForward(page, 0, Infinity);
    expect(got.length).toBe(1000);
  });
});
