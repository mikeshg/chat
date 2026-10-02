import type { ConversationView } from "../../types";

// ===== V2 P1-3 =====
// 会话列表副标题预览：typing > 草稿 > 最后一条消息 > 空。
// （[有人@我] 标记在组件里单独渲染，始终最前。）

export interface ConvPreviewInput {
  /** 该会话当前草稿文本（无则 undefined/""） */
  draft?: string;
  /** 是否当前打开的会话（打开中的会话不显示草稿标记） */
  active: boolean;
  /** 对方正在输入（未过期） */
  typing: boolean;
  /** 本人 id（群聊预览的 sender 前缀判定用） */
  meId?: string;
}

export interface ConvPreview {
  kind: "typing" | "draft" | "message" | "empty";
  /** 副标题正文；draft 时仅首行（截断由 CSS 负责） */
  text: string;
}

export function convPreview(c: ConversationView, o: ConvPreviewInput): ConvPreview {
  if (o.typing) return { kind: "typing", text: "对方正在输入…" };
  const draft = o.draft?.trim();
  if (!o.active && draft) return { kind: "draft", text: draft.split("\n", 1)[0] };
  const m = c.last_message;
  if (!m) return { kind: "empty", text: "暂无消息" };
  const who = c.kind === "group" && m.sender_id !== o.meId ? `${m.sender_name}：` : "";
  return { kind: "message", text: `${who}${m.content}` };
}
