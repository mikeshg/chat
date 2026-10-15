import { describe, expect, it } from "vitest";
import type { ConversationView, MessageView } from "../../types";
import { convPreview } from "./convPreview";

// ===== V2 P1-3 =====
function msg(patch: Partial<MessageView> = {}): MessageView {
  return {
    id: "m1",
    conversation_id: "c1",
    seq: 1,
    sender_id: "u-b",
    sender_username: "ub",
    sender_name: "B",
    content: "最后一条消息",
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

describe("会话列表副标题预览", () => {
  const base = { active: false, typing: false, meId: "me" };

  it("优先级：typing > 草稿 > 最后一条消息 > 空", () => {
    const c = conv({ last_message: msg() });
    expect(convPreview(c, { ...base, typing: true, draft: "d" })).toEqual({
      kind: "typing",
      text: "对方正在输入…",
    });
    expect(convPreview(c, { ...base, draft: "d" })).toEqual({ kind: "draft", text: "d" });
    expect(convPreview(c, { ...base })).toEqual({ kind: "message", text: "最后一条消息" });
    expect(convPreview(conv(), { ...base })).toEqual({ kind: "empty", text: "暂无消息" });
  });

  it("当前打开的会话不显示草稿标记", () => {
    const c = conv({ last_message: msg() });
    expect(convPreview(c, { ...base, active: true, draft: "d" })).toEqual({
      kind: "message",
      text: "最后一条消息",
    });
  });

  it("空白草稿不显示；多行草稿只取首行（截断交给 CSS）", () => {
    const c = conv({ last_message: msg() });
    expect(convPreview(c, { ...base, draft: "   " })).toEqual({ kind: "message", text: "最后一条消息" });
    expect(convPreview(c, { ...base, draft: "第一行\n第二行" })).toEqual({
      kind: "draft",
      text: "第一行",
    });
  });

  it("群聊预览保留他人 sender 前缀；本人消息与私聊无前缀", () => {
    const g = conv({ kind: "group", last_message: msg() });
    expect(convPreview(g, { ...base })).toEqual({ kind: "message", text: "B：最后一条消息" });
    const mine = conv({ kind: "group", last_message: msg({ sender_id: "me" }) });
    expect(convPreview(mine, { ...base })).toEqual({ kind: "message", text: "最后一条消息" });
    const dm = conv({ last_message: msg() });
    expect(convPreview(dm, { ...base })).toEqual({ kind: "message", text: "最后一条消息" });
  });

  it("语音消息预览为 [语音]，不露出文件名", () => {
    const c = conv({
      last_message: msg({
        kind: "audio",
        content: "[语音]",
        attachment: { id: "a", file_name: "voice.webm", mime: "audio/webm", size: 1, duration_ms: 3000 },
      }),
    });
    expect(convPreview(c, { ...base })).toEqual({ kind: "message", text: "[语音]" });
  });

  // ===== MM2 =====
  it("撤回的最后一条消息按墓碑文案预览（含群主撤回与旧删除）", () => {
    const recalled = msg({
      deleted_at: "t",
      recalled_by: "u-b",
      recalled_by_name: "B",
      content: "消息已撤回",
    });
    expect(convPreview(conv({ last_message: recalled }), { ...base })).toEqual({
      kind: "message",
      text: "对方撤回了一条消息",
    });
    const mine = msg({ sender_id: "me", deleted_at: "t", recalled_by: "me", recalled_by_name: "我" });
    expect(convPreview(conv({ last_message: mine }), { ...base })).toEqual({
      kind: "message",
      text: "你撤回了一条消息",
    });
    const owner = msg({
      deleted_at: "t",
      recalled_by: "owner",
      recalled_by_name: "群主A",
      sender_name: "B",
      content: "消息已撤回",
    });
    expect(convPreview(conv({ kind: "group", last_message: owner }), { ...base })).toEqual({
      kind: "message",
      text: "群主撤回了 B 的一条消息",
    });
    // MM1 之前的历史墓碑保持旧文案
    const legacy = msg({ deleted_at: "t", content: "消息已删除" });
    expect(convPreview(conv({ last_message: legacy }), { ...base })).toEqual({
      kind: "message",
      text: "消息已删除",
    });
  });

  it("结构化草稿：DraftEntry.text 参与预览；只有 chip 时占位文件名", () => {
    const c = conv({ last_message: msg() });
    expect(convPreview(c, { ...base, draft: { text: "草稿字" } })).toEqual({
      kind: "draft",
      text: "草稿字",
    });
    expect(
      convPreview(c, {
        ...base,
        draft: {
          text: "",
          reuse: {
            attachment_id: "a1",
            file_name: "p.png",
            mime: "image/png",
            size: 1,
            kind: "image",
            conversation_id: "c1",
          },
        },
      }),
    ).toEqual({ kind: "draft", text: "[文件] p.png" });
  });
});
