import { describe, expect, it } from "vitest";
import { ApiError } from "../../api";
import type { ConversationView, MessageView } from "../../types";
import {
  editRejected,
  ownsEditResult,
  pendingAgentAttachment,
  prepareEditSubmission,
} from "./ChatView";

describe("edit submission callback ownership", () => {
  const token = {};
  const submitted = { token, rev: 2, messageId: "m1", convId: "c1", gen: 7 };

  it("accepts the result only for the same owner token, edit revision, message, conv and session", () => {
    const live = { owner: token, rev: 2, editingId: "m1", activeConv: "c1", gen: 7 };
    expect(ownsEditResult(submitted, live)).toBe(true);
  });

  it("rejects the result after cancel, switching to another message, or account/conv change", () => {
    // 取消编辑：editingId 清空且代次递增
    expect(ownsEditResult(submitted, { owner: token, rev: 3, editingId: null, activeConv: "c1", gen: 7 })).toBe(false);
    // 改编辑另一条消息
    expect(ownsEditResult(submitted, { owner: token, rev: 3, editingId: "m2", activeConv: "c1", gen: 7 })).toBe(false);
    // 会话切换/组件重挂：owner token 被替换
    expect(ownsEditResult(submitted, { owner: {}, rev: 2, editingId: "m1", activeConv: "c1", gen: 7 })).toBe(false);
    expect(ownsEditResult(submitted, { owner: null, rev: 2, editingId: "m1", activeConv: "c1", gen: 7 })).toBe(false);
    // 已切走会话或换账号
    expect(ownsEditResult(submitted, { owner: token, rev: 2, editingId: "m1", activeConv: "c2", gen: 7 })).toBe(false);
    expect(ownsEditResult(submitted, { owner: token, rev: 2, editingId: "m1", activeConv: "c1", gen: 8 })).toBe(false);
    // token 为空时永不相等
    expect(ownsEditResult({ ...submitted, token: null }, { owner: null, rev: 2, editingId: "m1", activeConv: "c1", gen: 7 })).toBe(false);
  });
});

describe("edit submission retention by outcome", () => {
  it("keeps the same op only when the outcome is uncertain", () => {
    // 结果未确认（网络错误/会话代次中断/5xx）：必须保留 clientOpId 供幂等重试
    expect(editRejected(new Error("network down"))).toBe(false);
    expect(editRejected(new ApiError(0, "session changed"))).toBe(false);
    expect(editRejected(new ApiError(500, "boom"))).toBe(false);
    // 服务端明确拒绝：内容或版本需变化，下一次提交必须是新 op
    for (const st of [400, 403, 404, 409, 422]) {
      expect(editRejected(new ApiError(st, "rejected"))).toBe(true);
    }
  });
});

describe("edit submission payload", () => {
  it("retains operation ID, body and expected version for an uncertain retry", () => {
    const first = prepareEditSubmission(null, "message", " edited ", 2);
    expect(prepareEditSubmission(first, "message", "edited", 3)).toBe(first);
    expect(first.expectedVersion).toBe(2);
    expect(first.content).toBe("edited");
    expect(prepareEditSubmission(first, "message", "new draft", 3).clientOpId).not.toBe(first.clientOpId);
    expect(prepareEditSubmission(null, "message", "edited", 3).clientOpId).not.toBe(first.clientOpId);
  });

  it("validates the Rust codepoint limit rather than UTF-16 units", () => {
    expect(prepareEditSubmission(null, "m", "𠀀".repeat(4000), 0).content).toHaveLength(8000);
    expect(() => prepareEditSubmission(null, "m", "𠀀".repeat(4001), 0)).toThrow();
  });
});

// ===== Agent A05 =====
// 专属会话里主人的附件消息显示「待交付」提示，被调用带走（attachment_ids）后消失。
describe("pending agent attachment hint", () => {
  const me = "u-me";
  const agentConv = { kind: "agent" } as ConversationView;
  const msg = (over: Partial<MessageView> = {}): MessageView => ({
    id: "m1",
    conversation_id: "c1",
    seq: 1,
    sender_id: me,
    sender_username: "u",
    sender_name: "u",
    content: "",
    kind: "file",
    attachment: { id: "a1", file_name: "f.txt", mime: "text/plain", size: 1 },
    mentions: [],
    created_at: "t",
    ...over,
  });

  it("own file/image messages in an agent conv are pending until an invocation takes them", () => {
    expect(pendingAgentAttachment(msg(), agentConv, me, new Set())).toBe(true);
    expect(pendingAgentAttachment(msg({ kind: "image" }), agentConv, me, new Set())).toBe(true);
    // 已被某次调用带走 → 提示消失
    expect(pendingAgentAttachment(msg(), agentConv, me, new Set(["a1"]))).toBe(false);
  });

  it("agent messages, text, audio, deleted and non-agent convs never show it", () => {
    expect(
      pendingAgentAttachment(msg({ agent: { id: "g", name: "g" } }), agentConv, me, new Set()),
    ).toBe(false);
    expect(
      pendingAgentAttachment(msg({ kind: "text", attachment: null }), agentConv, me, new Set()),
    ).toBe(false);
    expect(pendingAgentAttachment(msg({ kind: "audio" }), agentConv, me, new Set())).toBe(false);
    expect(pendingAgentAttachment(msg({ deleted_at: "x" }), agentConv, me, new Set())).toBe(false);
    expect(
      pendingAgentAttachment(msg(), { kind: "dm" } as ConversationView, me, new Set()),
    ).toBe(false);
    // 别人的消息（非主人所发）不显示
    expect(pendingAgentAttachment(msg({ sender_id: "other" }), agentConv, me, new Set())).toBe(
      false,
    );
  });
});
