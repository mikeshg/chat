import { describe, expect, it } from "vitest";
import type { ConversationView, FriendView, MessageView } from "../../types";
import {
  convTitleOf,
  highlightParts,
  matchConvsByTitle,
  senderOptions,
  snippetOf,
} from "./searchSnippets";

// ===== V2 P3-1 =====
function msg(patch: Partial<MessageView> = {}): MessageView {
  return {
    id: "m1",
    conversation_id: "c1",
    seq: 1,
    sender_id: "u-b",
    sender_username: "ub",
    sender_name: "B",
    content: "正文",
    kind: "text",
    attachment: null,
    mentions: [],
    created_at: "2026-01-01T00:00:00Z",
    ...patch,
  };
}

function conv(patch: Partial<ConversationView> = {}): ConversationView {
  return {
    id: "c1",
    kind: "dm",
    name: null,
    owner_id: null,
    peer: null,
    member_count: 2,
    last_seq: 0,
    unread_count: 0,
    has_mention: false,
    last_message: null,
    created_at: "2026-01-01T00:00:00Z",
    ...patch,
  };
}

function friend(id: string, name: string): FriendView {
  return {
    user: { id, username: id, display_name: name, role: "user", last_seen_at: null },
    since: "2026-01-01T00:00:00Z",
  };
}

describe("搜索摘要高亮切分", () => {
  it("大小写不敏感，多处命中全切成片段", () => {
    expect(highlightParts("Foo 打 foo 包 FOO", "foo")).toEqual([
      { text: "Foo", hit: true },
      { text: " 打 ", hit: false },
      { text: "foo", hit: true },
      { text: " 包 ", hit: false },
      { text: "FOO", hit: true },
    ]);
  });

  it("正则元字符按字面量处理，不当通配符", () => {
    // "a.c" 只能命中字面的 "a.c"，不能命中 "axc"
    expect(highlightParts("a.c 与 axc", "a.c")).toEqual([
      { text: "a.c", hit: true },
      { text: " 与 axc", hit: false },
    ]);
    // 100% / a_b 这类字符同样按字面量
    expect(highlightParts("进度 100% 完成", "100%")).toEqual([
      { text: "进度 ", hit: false },
      { text: "100%", hit: true },
      { text: " 完成", hit: false },
    ]);
  });

  it("空关键词不切；空文本返回空数组", () => {
    expect(highlightParts("abc", "  ")).toEqual([{ text: "abc", hit: false }]);
    expect(highlightParts("", "q")).toEqual([]);
  });

  it("中文关键词命中", () => {
    expect(highlightParts("上周的周报发了吗", "周报")).toEqual([
      { text: "上周的", hit: false },
      { text: "周报", hit: true },
      { text: "发了吗", hit: false },
    ]);
  });
});

describe("搜索结果摘要", () => {
  it("附件消息显示占位文字而不是占位 content 原文", () => {
    expect(snippetOf(msg({ kind: "image", content: "[图片]", attachment: { id: "a", file_name: "p.png", mime: "image/png", size: 1 } }))).toBe("[图片]");
    expect(
      snippetOf(
        msg({ kind: "audio", content: "[语音]", attachment: { id: "a", file_name: "v.webm", mime: "audio/webm", size: 1, duration_ms: 3000 } }),
      ),
    ).toBe("[语音]");
    expect(
      snippetOf(
        msg({ kind: "file", content: "[文件] doc.pdf", attachment: { id: "a", file_name: "doc.pdf", mime: "application/pdf", size: 1 } }),
      ),
    ).toBe("[文件] doc.pdf");
  });

  it("普通文本/投票消息用 content", () => {
    expect(snippetOf(msg({ content: "你好" }))).toBe("你好");
    expect(snippetOf(msg({ kind: "poll", content: "[投票] 吃什么" }))).toBe("[投票] 吃什么");
  });
});

describe("会话标题对照与本地过滤", () => {
  const convs = [
    conv({ id: "c1", kind: "dm", peer: { id: "u1", username: "u1", display_name: "小明", role: "user", last_seen_at: null } }),
    conv({ id: "c2", kind: "group", name: "项目组", member_count: 3 }),
    conv({ id: "c3", kind: "self" }),
  ];

  it("convTitleOf：dm 用 peer 显示名、群用群名、self 固定文案、未知会话兜底", () => {
    expect(convTitleOf(convs, "c1")).toBe("小明");
    expect(convTitleOf(convs, "c2")).toBe("项目组");
    expect(convTitleOf(convs, "c3")).toBe("文件传输助手");
    expect(convTitleOf(convs, "gone")).toBe("会话");
  });

  it("matchConvsByTitle：大小写不敏感子串匹配；空关键词返回空", () => {
    expect(matchConvsByTitle(convs, "项目").map((c) => c.id)).toEqual(["c2"]);
    expect(matchConvsByTitle(convs, "小明").map((c) => c.id)).toEqual(["c1"]);
    expect(matchConvsByTitle(convs, "  ")).toEqual([]);
  });
});

describe("发送者筛选候选", () => {
  it("好友 + 会话 peer + 已缓存消息发送者，去重并按显示名排序", () => {
    const convs = [
      conv({ id: "c1", kind: "dm", peer: { id: "u1", username: "u1", display_name: "小明", role: "user", last_seen_at: null } }),
    ];
    const friends = [friend("u2", "阿红"), friend("u1", "小明")];
    const messages = {
      c1: [
        msg({ sender_id: "u1", sender_name: "小明", sender_username: "u1" }),
        msg({ id: "m2", sender_id: "u3", sender_name: "张三", sender_username: "u3" }),
        // 乐观消息不进候选（id 前缀 client:）
        msg({ id: "client:x", sender_id: "me", sender_name: "我", sender_username: "me" }),
      ],
    };
    const opts = senderOptions(friends, convs, messages);
    expect(opts.map((u) => u.id)).toEqual(["u2", "u1", "u3"]);
    expect(opts.find((u) => u.id === "u3")?.display_name).toBe("张三");
  });
});
