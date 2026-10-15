import { describe, expect, it } from "vitest";
import type { MessageView } from "../../types";
import {
  GHOST_CLICK_MS,
  RECALL_WINDOW_MS,
  canRecall,
  copyTextFor,
  isGhostClick,
  isTombstone,
  menuItems,
  msgDeletedText,
  quoteDeletedText,
  shouldOpenMenuOnClick,
  shouldOpenMenuOnContextMenu,
  shouldOpenMenuOnTap,
  tombstoneText,
  type MenuCtx,
} from "./msgMenu";

// ===== MM2 =====
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
    created_at: new Date(NOW - 60_000).toISOString(), // 1 分钟前
    ...patch,
  };
}

function ctx(patch: Partial<MenuCtx> = {}): MenuCtx {
  return {
    meId: "me",
    myRole: undefined,
    convType: "dm",
    now: NOW,
    readOnly: false,
    serverSupportsRecall: true,
    ...patch,
  };
}

const actions = (m: MessageView, c: MenuCtx) => menuItems(m, c).map((i) => i.action);
const labels = (m: MessageView, c: MenuCtx) => menuItems(m, c).map((i) => i.label);

describe("menuItems：统一菜单项", () => {
  it("文本消息（他人，DM）：回复/复制/转发/多选；撤回在末位仅自己消息有", () => {
    const items = menuItems(msg(), ctx());
    expect(items.map((i) => i.action)).toEqual(["reply", "copy", "forward", "multi"]);
    expect(menuItems(msg({ sender_id: "me" }), ctx()).map((i) => i.action)).toEqual([
      "reply",
      "copy",
      "forward",
      "multi",
      "recall",
    ]);
    // 撤回末位且标红
    const its = menuItems(msg({ sender_id: "me" }), ctx());
    expect(its[its.length - 1]).toMatchObject({ action: "recall", label: "撤回", danger: true });
  });

  it("图片附件：查看大图 + 下载在最前", () => {
    const m = msg({
      kind: "image",
      attachment: { id: "a", file_name: "p.png", mime: "image/png", size: 1 },
    });
    expect(actions(m, ctx()).slice(0, 2)).toEqual(["preview", "reply"]);
    expect(labels(m, ctx())).toContain("查看大图");
    expect(labels(m, ctx())).toContain("下载");
    expect(labels(m, ctx())).not.toContain("复制图片");
  });

  it("文件附件：第一项是「预览」", () => {
    const m = msg({
      kind: "file",
      attachment: { id: "a", file_name: "d.pdf", mime: "application/pdf", size: 1 },
    });
    expect(labels(m, ctx())[0]).toBe("预览");
  });

  it("墓碑与乐观消息没有任何菜单项", () => {
    expect(menuItems(msg({ deleted_at: "t" }), ctx())).toEqual([]);
    expect(menuItems(msg({ id: "client:x" }), ctx())).toEqual([]);
  });

  it("只读会话：回复隐藏，复制保留", () => {
    expect(actions(msg(), ctx({ readOnly: true }))).toEqual(["copy", "forward", "multi"]);
  });

  it("投票消息没有转发（不可转），复制拿标题", () => {
    const m = msg({
      kind: "poll",
      content: "",
      poll: {
        question: "午饭吃什么",
        allow_multiple: false,
        anonymous: false,
        closed_at: null,
        expires_at: null,
        expired: false,
        version: 1,
        total_voters: 0,
        options: [],
        my_option_ids: [],
      },
    });
    const a = actions(m, ctx());
    expect(a).not.toContain("forward");
    expect(a).toContain("copy");
  });
});

describe("canRecall：撤回显隐", () => {
  it("自己的消息 24h 内可撤；超过 24h 不可撤（严格小于）", () => {
    const within = msg({ sender_id: "me", created_at: new Date(NOW - RECALL_WINDOW_MS + 1).toISOString() });
    const past = msg({ sender_id: "me", created_at: new Date(NOW - RECALL_WINDOW_MS).toISOString() });
    expect(canRecall(within, ctx())).toBe(true);
    expect(canRecall(past, ctx())).toBe(false);
  });

  it("群主可撤他人消息（无时限），普通成员不行；admin 无额外权限", () => {
    const old = msg({ created_at: new Date(NOW - 8 * RECALL_WINDOW_MS).toISOString() });
    const groupOwner = ctx({ convType: "group", myRole: "owner" });
    const member = ctx({ convType: "group", myRole: "member" });
    const admin = ctx({ myRole: "admin" });
    expect(canRecall(old, groupOwner)).toBe(true);
    expect(canRecall(old, member)).toBe(false);
    expect(canRecall(old, admin)).toBe(false);
  });

  it("群主撤自己的消息也受 24h 限制", () => {
    const old = msg({ sender_id: "me", created_at: new Date(NOW - 2 * RECALL_WINDOW_MS).toISOString() });
    expect(canRecall(old, ctx({ convType: "group", myRole: "owner" }))).toBe(false);
  });

  it("旧服务端（无撤回语义）：自己的消息无时限", () => {
    const old = msg({ sender_id: "me", created_at: new Date(NOW - 2 * RECALL_WINDOW_MS).toISOString() });
    expect(canRecall(old, ctx({ serverSupportsRecall: false }))).toBe(true);
  });
});

describe("shouldOpenMenuOnTap：触屏点按规则", () => {
  const el = (hit: boolean) => ({ closest: () => (hit ? {} : null) });

  it("普通气泡点按开菜单", () => {
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 0, pressMs: 100, hasSelection: false })).toBe(true);
  });

  it("排除目标（链接/按钮/回复引用/自有动作）不开", () => {
    expect(shouldOpenMenuOnTap({ target: el(true), movedPx: 0, pressMs: 100, hasSelection: false })).toBe(false);
  });

  it("有文本选区 / 位移 >10px / 按压 >500ms 都不开", () => {
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 0, pressMs: 100, hasSelection: true })).toBe(false);
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 11, pressMs: 100, hasSelection: false })).toBe(false);
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 10, pressMs: 100, hasSelection: false })).toBe(true);
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 0, pressMs: 501, hasSelection: false })).toBe(false);
    expect(shouldOpenMenuOnTap({ target: el(false), movedPx: 0, pressMs: 500, hasSelection: false })).toBe(true);
  });

  it("target 无 closest（非元素）也按可开处理", () => {
    expect(shouldOpenMenuOnTap({ target: null, movedPx: 0, pressMs: 0, hasSelection: false })).toBe(true);
  });
});

describe("shouldOpenMenuOnContextMenu：右键来源判定", () => {
  it("touch 长按压制的 contextmenu 不开；鼠标/笔/键盘开", () => {
    expect(shouldOpenMenuOnContextMenu("touch")).toBe(false);
    expect(shouldOpenMenuOnContextMenu("mouse")).toBe(true);
    expect(shouldOpenMenuOnContextMenu("pen")).toBe(true);
    expect(shouldOpenMenuOnContextMenu("")).toBe(true);
    expect(shouldOpenMenuOnContextMenu(null)).toBe(true);
  });
});

describe("shouldOpenMenuOnClick：click 路径的点按判定（幽灵点击修复）", () => {
  const el = (hit: boolean) => ({ closest: () => (hit ? {} : null) });
  const tap = { movedPx: 0, pressMs: 100, hasSelection: false, target: el(false) };

  it("触屏点按开面板", () => {
    expect(shouldOpenMenuOnClick({ pointerType: "touch", ...tap })).toBe(true);
  });

  it("鼠标/笔单击不开（桌面行为照旧）", () => {
    expect(shouldOpenMenuOnClick({ pointerType: "mouse", ...tap })).toBe(false);
    expect(shouldOpenMenuOnClick({ pointerType: "pen", ...tap })).toBe(false);
    expect(shouldOpenMenuOnClick({ pointerType: "", ...tap })).toBe(false);
  });

  it("手势不合格照样不开：位移/按压/选区/排除目标沿用 tap 规则", () => {
    expect(shouldOpenMenuOnClick({ pointerType: "touch", ...tap, movedPx: 11 })).toBe(false);
    expect(shouldOpenMenuOnClick({ pointerType: "touch", ...tap, pressMs: 501 })).toBe(false);
    expect(shouldOpenMenuOnClick({ pointerType: "touch", ...tap, hasSelection: true })).toBe(false);
    expect(shouldOpenMenuOnClick({ pointerType: "touch", ...tap, target: el(true) })).toBe(false);
  });
});

describe("isGhostClick：浮层打开后的幽灵点击豁免窗", () => {
  it("350ms 内的点击忽略，窗外放行（严格小于）", () => {
    expect(isGhostClick(NOW, NOW)).toBe(true);
    expect(isGhostClick(NOW, NOW + GHOST_CLICK_MS - 1)).toBe(true);
    expect(isGhostClick(NOW, NOW + GHOST_CLICK_MS)).toBe(false);
    expect(isGhostClick(NOW, NOW + 60_000)).toBe(false);
  });

  it("未打开（openedAt=0）不算幽灵点击", () => {
    expect(isGhostClick(0, NOW)).toBe(false);
  });
});

describe("tombstoneText：墓碑文案", () => {
  const c = { meId: "me" };

  it("非墓碑 → null", () => {
    expect(tombstoneText(msg(), { ...c, convType: "dm" })).toBeNull();
    expect(isTombstone(msg())).toBe(false);
  });

  it("我撤回自己 → 你撤回了一条消息", () => {
    const m = msg({ sender_id: "me", deleted_at: "t", recalled_by: "me", recalled_by_name: "我" });
    expect(tombstoneText(m, { ...c, convType: "dm" })).toBe("你撤回了一条消息");
  });

  it("DM 对方撤回 → 对方撤回了一条消息", () => {
    const m = msg({ deleted_at: "t", recalled_by: "u-b", recalled_by_name: "B" });
    expect(tombstoneText(m, { ...c, convType: "dm" })).toBe("对方撤回了一条消息");
  });

  it("群成员各撤各的 → X 撤回了一条消息", () => {
    const m = msg({ deleted_at: "t", recalled_by: "u-b", recalled_by_name: "B" });
    expect(tombstoneText(m, { ...c, convType: "group" })).toBe("B 撤回了一条消息");
  });

  it("群主撤他人 → 群主撤回了 X 的一条消息；我是群主 → 你撤回了 X 的一条消息", () => {
    const m = msg({
      deleted_at: "t",
      recalled_by: "owner",
      recalled_by_name: "群主A",
      sender_name: "B",
    });
    expect(tombstoneText(m, { ...c, convType: "group" })).toBe("群主撤回了 B 的一条消息");
    const mine = msg({
      sender_id: "u-b",
      sender_name: "B",
      deleted_at: "t",
      recalled_by: "me",
      recalled_by_name: "我",
    });
    expect(tombstoneText(mine, { ...c, convType: "group" })).toBe("你撤回了 B 的一条消息");
  });

  it("我的消息被群主撤 → 群主撤回了你的一条消息", () => {
    const m = msg({ sender_id: "me", deleted_at: "t", recalled_by: "owner", recalled_by_name: "群主A" });
    expect(tombstoneText(m, { ...c, convType: "group" })).toBe("群主撤回了你的一条消息");
  });

  it("旧墓碑（无 recalled_by）保持「消息已删除」", () => {
    const m = msg({ deleted_at: "t", content: "消息已删除" });
    expect(tombstoneText(m, { ...c, convType: "dm" })).toBe("消息已删除");
    expect(tombstoneText(m, { ...c, convType: "group" })).toBe("消息已删除");
  });
});

describe("引用/横幅文案与复制文本", () => {
  it("撤回引用 → 原消息已撤回；旧删除 → 原消息已删除", () => {
    expect(msgDeletedText(msg({ deleted_at: "t", recalled_by: "u" }))).toBe("原消息已撤回");
    expect(msgDeletedText(msg({ deleted_at: "t" }))).toBe("原消息已删除");
    expect(
      quoteDeletedText({ message_id: "m", seq: 1, sender_name: "B", excerpt: "", kind: "text", deleted: true, recalled_by_name: "B" }),
    ).toBe("原消息已撤回");
    expect(
      quoteDeletedText({ message_id: "m", seq: 1, sender_name: "B", excerpt: "", kind: "text", deleted: true }),
    ).toBe("原消息已删除");
  });

  it("copyTextFor：文本→正文；附件→文件名；投票→标题", () => {
    expect(copyTextFor(msg({ content: "hi @u" }))).toBe("hi @u");
    expect(
      copyTextFor(
        msg({ kind: "image", attachment: { id: "a", file_name: "p.png", mime: "image/png", size: 1 } }),
      ),
    ).toBe("p.png");
    expect(
      copyTextFor(
        msg({
          kind: "poll",
          content: "",
          poll: {
            question: "问题",
            allow_multiple: false,
            anonymous: false,
            closed_at: null,
            expires_at: null,
            expired: false,
            version: 1,
            total_voters: 0,
            options: [],
            my_option_ids: [],
          },
        }),
      ),
    ).toBe("问题");
  });
});
