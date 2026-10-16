import { describe, expect, it, vi } from "vitest";
import type { MessageView } from "../../types";

// ===== MM2-fix2 =====
// 本文件以 FEATURE_MULTI_SELECT=true 跑 menuItems（多选「开」一侧；
// 「关」一侧由 msgMenu.test.ts 覆盖）。MM3 暂缓：合并转发设计定稿后
// 把 features.ts 的开关拨回 true 即恢复菜单项。
vi.mock("./features", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./features")>()),
  FEATURE_MULTI_SELECT: true,
}));

import { menuItems, type MenuCtx } from "./msgMenu";

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

describe("menuItems：FEATURE_MULTI_SELECT=true（多选开启）", () => {
  it("文本消息：多选在转发之后、撤回之前", () => {
    expect(actions(msg(), ctx())).toEqual(["reply", "copy", "forward", "multi"]);
    expect(actions(msg({ sender_id: "me" }), ctx())).toEqual([
      "reply",
      "copy",
      "forward",
      "multi",
      "recall",
    ]);
  });

  it("附件消息：多选在下载之后", () => {
    const m = msg({
      kind: "image",
      attachment: { id: "a", file_name: "p.png", mime: "image/png", size: 1 },
    });
    expect(actions(m, ctx())).toEqual([
      "preview",
      "reply",
      "copy",
      "forward",
      "download",
      "multi",
    ]);
    expect(menuItems(m, ctx()).find((i) => i.action === "multi")?.label).toBe("多选");
  });

  it("只读会话仍有多选；墓碑/乐观消息仍无菜单项", () => {
    expect(actions(msg(), ctx({ readOnly: true }))).toContain("multi");
    expect(menuItems(msg({ deleted_at: "t" }), ctx())).toEqual([]);
    expect(menuItems(msg({ id: "client:x" }), ctx())).toEqual([]);
  });

  it("其余下线开关仍关闭：表情/编辑/置顶/收藏不出现", () => {
    const a = actions(msg({ sender_id: "me" }), ctx());
    expect(a).not.toContain("reaction");
    expect(a).not.toContain("edit");
    expect(a).not.toContain("pin");
    expect(a).not.toContain("favorite");
  });
});
