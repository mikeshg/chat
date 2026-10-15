import { describe, expect, it } from "vitest";
import { ApiError } from "../../api";
import { tokenize } from "../../richText";
import { pinExcerpt } from "./PinnedBar";
import { replyPreview } from "../../store";
import type { ForwardBatchResp, ForwardResp, MessageView, RecordItem, RecordPreview } from "../../types";
import {
  FALLBACK_MERGED_TOAST,
  FALLBACK_SEPARATE_CAP,
  batchResultToast,
  fallbackSeparateToast,
  forwardRespToast,
  isOldServerUnsupported,
  prepareBatchSubmission,
} from "./forwardBatch";
import {
  RECORD_TAG,
  RECORD_TITLE_MAX,
  recordCardLines,
  recordItemText,
  recordLabel,
  recordSections,
  recordTitle,
  recordViewerActions,
  viewerToolbarIds,
} from "./chatRecord";
import {
  MULTI_SELECT_MAX,
  MS_CAP_TOAST,
  enterMultiSelect,
  orderedSelectedIds,
  pruneSelection,
  selectable,
  toggleSelect,
} from "./multiSelect";
import { menuItems, type MenuCtx } from "./msgMenu";
import { backLayerDepth, consumeBack, pushBackLayer, resetBackLayers } from "./backStack";
import { convPreview } from "./convPreview";
import { notificationBody } from "../../notify";
import { snippetOf } from "./searchSnippets";

// ===== MF2：多选 + 逐条/合并转发 + 聊天记录（纯逻辑测试） =====

const NOW = Date.parse("2026-06-01T12:00:00Z");

function msg(patch: Partial<MessageView> = {}): MessageView {
  return {
    id: "m1",
    conversation_id: "c1",
    seq: 1,
    sender_id: "u-b",
    sender_username: "ub",
    sender_name: "B",
    content: "你好",
    kind: "text",
    attachment: null,
    mentions: [],
    created_at: new Date(NOW - 60_000).toISOString(),
    ...patch,
  };
}

const recordMsg = (patch: Partial<MessageView> = {}): MessageView =>
  msg({
    kind: "chat_record",
    content: "[聊天记录] 周末团建\nA：hi",
    record_preview: {
      title: "周末团建",
      count: 5,
      items: [
        { sender_name: "A", kind: "text", text: "hi" },
        { sender_name: "B", kind: "image", text: "photo.png" },
        { sender_name: "C", kind: "file", text: "plan.pdf" },
        { sender_name: "D", kind: "audio", text: "" },
      ],
    },
    ...patch,
  });

// ---------- multiSelect reducer（MF2-C2/C3） ----------

describe("multiSelect 纯状态机", () => {
  it("从菜单进入：可选消息预选；不可选（投票）只进模式不预选", () => {
    const ms = enterMultiSelect("c1", msg({ id: "a" }));
    expect(ms.convId).toBe("c1");
    expect([...ms.ids]).toEqual(["a"]);
    const msPoll = enterMultiSelect("c1", msg({ id: "p", kind: "poll" }));
    expect(msPoll.ids.size).toBe(0);
  });

  it("toggle：可选行加选/取消；不可选行静默忽略", () => {
    let ms = enterMultiSelect("c1", msg({ id: "a" }));
    const r1 = toggleSelect(ms, msg({ id: "b" }));
    expect(r1.toast).toBeUndefined();
    ms = r1.next;
    expect([...ms.ids].sort()).toEqual(["a", "b"]);
    ms = toggleSelect(ms, msg({ id: "a" })).next;
    expect([...ms.ids]).toEqual(["b"]);
    // 墓碑 / client: / poll / system 都不可选：不变更、不提示
    for (const bad of [
      msg({ id: "x1", deleted_at: "t" }),
      msg({ id: "client:x" }),
      msg({ id: "x2", kind: "poll" }),
      msg({ id: "x3", kind: "system" }),
    ]) {
      expect(selectable(bad)).toBe(false);
      const r = toggleSelect(ms, bad);
      expect(r.next).toBe(ms);
      expect(r.toast).toBeUndefined();
    }
    expect([...ms.ids]).toEqual(["b"]);
  });

  it("cap 100：选第 101 条集合不变并提示「最多选择 100 条」", () => {
    let ms = enterMultiSelect("c1", msg({ id: "s0" }));
    for (let i = 1; i < MULTI_SELECT_MAX; i++) {
      ms = toggleSelect(ms, msg({ id: `s${i}` })).next;
    }
    expect(ms.ids.size).toBe(100);
    const r = toggleSelect(ms, msg({ id: "s100" }));
    expect(r.next.ids.size).toBe(100);
    expect(r.toast).toBe(MS_CAP_TOAST);
    expect(r.toast).toBe("最多选择 100 条");
  });

  it("收编：变墓碑的选中 id 剔除；不在已加载窗口的 id 保留（翻页未拉到不算消失）", () => {
    let ms = enterMultiSelect("c1", msg({ id: "a" }));
    ms = toggleSelect(ms, msg({ id: "b" })).next;
    ms = toggleSelect(ms, msg({ id: "ghost-not-loaded" })).next;
    expect(ms.ids.size).toBe(3);
    const pruned = pruneSelection(ms, [
      msg({ id: "a", deleted_at: "t" }), // 撤回 → 剔除
      msg({ id: "b" }),
      // ghost-not-loaded 不在窗口 → 保留
    ]);
    expect([...pruned.ids].sort()).toEqual(["b", "ghost-not-loaded"]);
    // 全剔除后仍返回 MultiSelect（模式留在 0，由 UI 禁用转发按钮）
    const empty = pruneSelection(ms, [msg({ id: "a", deleted_at: "t" }), msg({ id: "b", deleted_at: "t" }), msg({ id: "ghost-not-loaded", deleted_at: "t" })]);
    expect(empty.ids.size).toBe(0);
  });

  it("选中 id 按消息 seq（气泡序）输出，与点击顺序无关", () => {
    let ms = enterMultiSelect("c1", msg({ id: "late", seq: 9 }));
    ms = toggleSelect(ms, msg({ id: "early", seq: 2 })).next;
    ms = toggleSelect(ms, msg({ id: "mid", seq: 5 })).next;
    const list = [msg({ id: "early", seq: 2 }), msg({ id: "mid", seq: 5 }), msg({ id: "late", seq: 9 })];
    expect(orderedSelectedIds(ms, list)).toEqual(["early", "mid", "late"]);
  });

  it("乐观消息确认换新 id：不会自动带入选中", () => {
    // client: 不可选，所以列表里换名后自然不在集合里
    const ms = enterMultiSelect("c1", msg({ id: "a" }));
    const afterAck = pruneSelection(ms, [msg({ id: "a" }), msg({ id: "srv-1" })]);
    expect(afterAck.ids.has("srv-1")).toBe(false);
  });
});

// ---------- forwardBatch 载荷 / 结果文案 / 兜底判定（MF2-C4/C5/C6） ----------

describe("forwardBatch", () => {
  it("prepareBatchSubmission：同参数复用 op，任一变化换新 op", () => {
    const p1 = prepareBatchSubmission(null, "c1", ["m1", "m2"], ["t2", "t1"], "merged");
    expect(p1.targets).toEqual(["t1", "t2"]); // 去重排序
    expect(p1.client_op_id).toBeTruthy();
    // 完全相同 → 复用
    expect(prepareBatchSubmission(p1, "c1", ["m1", "m2"], ["t1", "t2"], "merged")).toBe(p1);
    // 任一变化 → 新 op
    for (const next of [
      prepareBatchSubmission(p1, "c2", ["m1", "m2"], ["t1", "t2"], "merged"),
      prepareBatchSubmission(p1, "c1", ["m1"], ["t1", "t2"], "merged"),
      prepareBatchSubmission(p1, "c1", ["m1", "m2"], ["t1"], "merged"),
      prepareBatchSubmission(p1, "c1", ["m1", "m2"], ["t1", "t2"], "separate"),
    ]) {
      expect(next.client_op_id).not.toBe(p1.client_op_id);
    }
  });

  const resp = (patch: Partial<ForwardBatchResp>): ForwardBatchResp => ({
    results: [],
    skipped: {},
    ...patch,
  });

  it("C5 结果文案：全成 / 部分败 / 全败 / 跳过计数", () => {
    expect(
      batchResultToast(
        resp({
          results: [
            { conversation_id: "t1", ok: true, forwarded: 2, skipped: {} },
            { conversation_id: "t2", ok: true, forwarded: 2, skipped: {} },
          ],
        }),
      ).text,
    ).toBe("已转发到 2 个会话");

    const partial = batchResultToast(
      resp({
        results: [
          { conversation_id: "t1", ok: true, forwarded: 1, skipped: {} },
          { conversation_id: "t2", ok: false, forwarded: 0, error: { code: "x", message: "不是成员" }, skipped: {} },
          { conversation_id: "t3", ok: false, forwarded: 0, error: { code: "y", message: "会话归档" }, skipped: {} },
        ],
      }),
    );
    expect(partial.text).toBe("已转发到 1 个会话，2 个失败：不是成员 等");
    expect(partial.ok).toBe(1);
    expect(partial.fail).toBe(2);

    expect(
      batchResultToast(
        resp({
          results: [
            { conversation_id: "t1", ok: false, forwarded: 0, error: { code: "x", message: "理由A" }, skipped: {} },
          ],
        }),
      ).text,
    ).toBe("转发失败：理由A");

    // skipped>0 追加；K 只取顶层一次（不按目标数乘）
    const skipped = batchResultToast(
      resp({
        results: [
          { conversation_id: "t1", ok: true, forwarded: 1, skipped: { too_deep: 1 } },
          { conversation_id: "t2", ok: true, forwarded: 1, skipped: { too_deep: 1 } },
        ],
        skipped: { too_deep: 1 },
      }),
    );
    expect(skipped.text).toBe("已转发到 2 个会话，已跳过 1 条不支持的消息");
    expect(skipped.skipped).toBe(1);
  });

  it("C5：顶层无 skipped 字段时退回第一目标的计数", () => {
    const t = batchResultToast(
      resp({
        results: [
          { conversation_id: "t1", ok: true, forwarded: 1, skipped: { recalled: 2 } },
        ],
        skipped: {},
      }),
    );
    expect(t.skipped).toBe(2);
    expect(t.text).toBe("已转发到 1 个会话，已跳过 2 条不支持的消息");
  });

  it("C6 旧服务端判定：405 / 404无error字段 → 兜底；404+JSON / 400 / 500 → 不兜底", () => {
    expect(isOldServerUnsupported(new ApiError(405, "method not allowed"))).toBe(true);
    expect(isOldServerUnsupported(new ApiError(404, "not found", { hasErrorField: false }))).toBe(true);
    expect(isOldServerUnsupported(new ApiError(404, "unknown_api", { hasErrorField: true }))).toBe(false);
    expect(isOldServerUnsupported(new ApiError(400, "没有可转发的消息", { hasErrorField: true }))).toBe(false);
    expect(isOldServerUnsupported(new ApiError(500, "boom"))).toBe(false);
    expect(isOldServerUnsupported(new Error("network"))).toBe(false);
  });

  it("C6 兜底文案：逐条 X/N；合并固定提示；上限 20", () => {
    expect(fallbackSeparateToast(3, 25)).toBe("服务端版本过低，已逐条转发 3/25 条");
    expect(FALLBACK_MERGED_TOAST).toBe("服务端版本过低，不支持合并转发");
    expect(FALLBACK_SEPARATE_CAP).toBe(20);
  });

  it("单条 ForwardResp 复用同一套 C5 文案（记录内附件转发也用它）", () => {
    const r: ForwardResp = {
      results: [
        { conversation_id: "t1", ok: true, message: msg() },
        { conversation_id: "t2", ok: false, error: "forbidden", message_text: "无权发送" },
      ],
    };
    expect(forwardRespToast(r).text).toBe("已转发到 1 个会话，1 个失败：无权发送");
  });
});

// ---------- chatRecord 标题/标签/卡片/分组/工具栏（REQUIRE 4/7、C8/C9） ----------

describe("chatRecord 标签与卡片", () => {
  it("recordTitle：预览 title 优先；缺预览取 content 首行并剥一个前导标签（不双重前缀）", () => {
    expect(recordTitle(recordMsg())).toBe("周末团建");
    expect(recordTitle(msg({ kind: "chat_record", content: "[聊天记录] 项目讨论\nA：x" }))).toBe("项目讨论");
    expect(recordTitle(msg({ kind: "chat_record", content: "纯首行\n其他" }))).toBe("纯首行");
  });

  it("recordLabel：「[聊天记录] {title}」；>30 字列表上下文截断", () => {
    expect(recordLabel(recordMsg())).toBe("[聊天记录] 周末团建");
    const long = recordMsg({
      record_preview: {
        title: "这是一个特别特别长的群聊记录标题它远远超过了三十个字符的长度限制哦",
        count: 1,
        items: [],
      },
    });
    const cut = recordLabel(long, RECORD_TITLE_MAX);
    expect(cut.startsWith("[聊天记录] ")).toBe(true);
    expect(cut.endsWith("…")).toBe(true);
    // 前缀之外标题 ≤30 字
    expect(Array.from(cut.slice(RECORD_TAG.length + 1)).length).toBe(31); // 30 + …
  });

  it("recordItemText：服务端已带 [ 前缀照用；否则按 kind 补 [图片]/[文件] 名/[语音]/[聊天记录]", () => {
    expect(recordItemText({ kind: "text", text: "hello" })).toBe("hello");
    expect(recordItemText({ kind: "image", text: "a.png" })).toBe("[图片] a.png");
    expect(recordItemText({ kind: "image", text: "[图片] 已带" })).toBe("[图片] 已带");
    expect(recordItemText({ kind: "file", text: "报表.pdf" })).toBe("[文件] 报表.pdf");
    expect(recordItemText({ kind: "audio", text: "" })).toBe("[语音]");
    expect(recordItemText({ kind: "chat_record", text: "嵌套记录" })).toBe("[聊天记录] 嵌套记录");
  });

  it("recordCardLines：最多 4 行「发送者：摘要」；count>4 不再补行", () => {
    const lines = recordCardLines(recordMsg().record_preview);
    expect(lines).toHaveLength(4);
    expect(lines[0]).toEqual({ sender: "A", text: "hi" });
    expect(lines[1].text).toBe("[图片] photo.png");
    expect(lines[2].text).toBe("[文件] plan.pdf");
    expect(lines[3].text).toBe("[语音]");
    const six: RecordPreview = {
      title: "t",
      count: 6,
      items: Array.from({ length: 6 }, (_, i) => ({ sender_name: `u${i}`, kind: "text", text: `x${i}` })),
    };
    expect(recordCardLines(six)).toHaveLength(4); // 无「等 6 条」行
  });

  const item = (created_at: string, kind = "text"): RecordItem => ({
    sender_id: "u",
    sender_name: "U",
    has_avatar: false,
    avatar_version: 0,
    created_at,
    kind,
    content: "x",
    attachment: null,
    record: null,
  });

  it("recordSections：按本地自然日分组并出日期分隔", () => {
    // 用本地时间构造（任何时区下同一天/跨天关系稳定）
    const secs = recordSections([
      item(new Date(2026, 4, 30, 9).toISOString()),
      item(new Date(2026, 4, 30, 22).toISOString()), // 本地同一天
      item(new Date(2026, 5, 1, 8).toISOString()), // 跨天
    ]);
    expect(secs.length).toBe(2);
    expect(secs[0].items).toHaveLength(2);
    expect(secs[1].items).toHaveLength(1);
    expect(secs[0].dayLabel).toBeTruthy();
  });

  it("C8 工具栏：记录上下文恰为 download→forward→close；普通上下文不含 multi", () => {
    expect(recordViewerActions().map((a) => a.testid)).toEqual([
      "viewer-download",
      "viewer-forward",
      "viewer-close",
    ]);
    expect(viewerToolbarIds("record")).toEqual(["viewer-download", "viewer-forward", "viewer-close"]);
    const normal = viewerToolbarIds("normal", ["reply", "forward", "recall", "multi", "download"]);
    expect(normal).toEqual(["viewer-reply", "viewer-forward", "viewer-recall", "viewer-download", "viewer-close"]);
    expect(normal).not.toContain("viewer-multi");
  });
});

// ---------- 聊天记录菜单（REQUIRE 4、C1） ----------

describe("chat_record 菜单", () => {
  const ctx = (patch: Partial<MenuCtx> = {}): MenuCtx => ({
    meId: "me",
    convType: "dm",
    now: NOW,
    readOnly: false,
    serverSupportsRecall: true,
    ...patch,
  });

  it("恰为 回复/转发/多选（+撤回规则）；永不出现 复制", () => {
    const other = menuItems(recordMsg(), ctx());
    expect(other.map((i) => i.action)).toEqual(["reply", "forward", "multi"]);
    const own = menuItems(recordMsg({ sender_id: "me" }), ctx());
    expect(own.map((i) => i.action)).toEqual(["reply", "forward", "multi", "recall"]);
    expect(own.every((i) => i.action !== "copy")).toBe(true);
    // 只读会话：回复消失，仍无复制
    const ro = menuItems(recordMsg(), ctx({ readOnly: true }));
    expect(ro.map((i) => i.action)).toEqual(["forward", "multi"]);
  });

  it("墓碑记录只剩撤回占位文案（无重新编辑/无菜单项）", () => {
    expect(menuItems(recordMsg({ deleted_at: "t" }), ctx())).toEqual([]);
  });
});

// ---------- 各上下文标签（REQUIRE 7、C9） ----------

describe("chat_record 各上下文标签", () => {
  it("convPreview：「[聊天记录] {title}」；群聊保留发送者前缀；缺预览用 content 首行", () => {
    const conv = {
      id: "c1",
      kind: "group",
      name: "g",
      owner_id: null,
      peer: null,
      member_count: 3,
      last_seq: 1,
      unread_count: 0,
      has_mention: false,
      last_message: recordMsg({ sender_id: "u-b", sender_name: "B" }),
      created_at: "2026-01-01T00:00:00Z",
    };
    expect(convPreview(conv, { active: false, typing: false, meId: "me" }).text).toBe(
      "B：[聊天记录] 周末团建",
    );
    const fallback = {
      ...conv,
      kind: "dm",
      last_message: msg({ kind: "chat_record", content: "[聊天记录] 兜底标题\nA：x" }),
    };
    expect(convPreview(fallback, { active: false, typing: false, meId: "me" }).text).toBe(
      "[聊天记录] 兜底标题",
    );
  });

  it("notificationBody：chat_record → 「发送者: [聊天记录] {title}」", () => {
    expect(notificationBody(recordMsg(), { preview: true, mentioned: false })).toBe(
      "B: [聊天记录] 周末团建",
    );
  });

  it("pinExcerpt / snippetOf / replyPreview 一致带标签", () => {
    expect(pinExcerpt(recordMsg())).toBe("[聊天记录] 周末团建");
    expect(snippetOf(recordMsg())).toBe("[聊天记录] 周末团建");
    const rp = replyPreview(recordMsg());
    expect(rp.excerpt).toBe("[聊天记录] 周末团建");
    expect(rp.kind).toBe("chat_record");
  });
});

// ---------- 提及关闭 / 返回栈 ----------

describe("记录/转发文本的 RichText", () => {
  it("mentions=false：@bob 是纯文本不产生 mention token；链接仍识别", () => {
    const toks = tokenize("@bob 看 https://a.com", { mentions: false });
    expect(toks.every((t) => t.t !== "mention")).toBe(true);
    expect(toks.some((t) => t.t === "link")).toBe(true);
    expect(toks.map((t) => (t.t === "text" ? t.text : "")).join("")).toContain("@bob");
    // 默认仍开提及
    expect(tokenize("@bob x").some((t) => t.t === "mention")).toBe(true);
  });
});

describe("backStack 覆盖层栈", () => {
  it("后进先出逐层关闭；close 后可重注册", () => {
    resetBackLayers();
    const closed: string[] = [];
    pushBackLayer(() => closed.push("ms"));
    const unregFwd = pushBackLayer(() => closed.push("fwd"));
    expect(backLayerDepth()).toBe(2);
    expect(consumeBack()).toBe(true); // fwd 先关
    expect(closed).toEqual(["fwd"]);
    expect(consumeBack()).toBe(true);
    expect(closed).toEqual(["fwd", "ms"]);
    expect(consumeBack()).toBe(false);
    // UI 路径关闭 → unregister 防尸体
    pushBackLayer(() => closed.push("a"));
    const u = pushBackLayer(() => closed.push("b"));
    u();
    expect(consumeBack()).toBe(true);
    expect(closed).toEqual(["fwd", "ms", "a"]);
    resetBackLayers();
    void unregFwd;
  });
});
