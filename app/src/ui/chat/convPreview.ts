import type { ConversationView } from "../../types";
import type { DraftEntry } from "../../drafts";
import { tombstoneText } from "./msgMenu";

// ===== V2 P1-3 =====
// 会话列表副标题预览：typing > 草稿 > 最后一条消息 > 空。
// （[有人@我] 标记在组件里单独渲染，始终最前。）
// MM2：草稿为结构化 DraftEntry（可含「复用附件」chip）；
// 最后一条消息是墓碑时按撤回文案显示（你/对方/名字）。

export interface ConvPreviewInput {
  /** 该会话当前草稿（旧调用方可传纯文本） */
  draft?: DraftEntry | string;
  /** 是否当前打开的会话（打开中的会话不显示草稿标记） */
  active: boolean;
  /** 对方正在输入（未过期） */
  typing: boolean;
  /** 本人 id（群聊预览的 sender 前缀与撤回文案判定用） */
  meId?: string;
}

export interface ConvPreview {
  kind: "typing" | "draft" | "message" | "empty";
  /** 副标题正文；draft 时仅首行（截断由 CSS 负责） */
  text: string;
}

export function convPreview(c: ConversationView, o: ConvPreviewInput): ConvPreview {
  if (o.typing) return { kind: "typing", text: "对方正在输入…" };
  const entry = typeof o.draft === "string" ? { text: o.draft } : o.draft;
  // 草稿只有 chip 没有文字时，用文件名占位提示还有未发内容
  const draftText =
    entry?.text.trim() || (entry?.reuse ? `[文件] ${entry.reuse.file_name}` : "");
  if (!o.active && draftText) return { kind: "draft", text: draftText.split("\n", 1)[0] };
  const m = c.last_message;
  if (!m) return { kind: "empty", text: "暂无消息" };
  const tomb = tombstoneText(m, { meId: o.meId, convType: c.kind });
  if (tomb != null) return { kind: "message", text: tomb };
  const who = c.kind === "group" && m.sender_id !== o.meId ? `${m.sender_name}：` : "";
  return { kind: "message", text: `${who}${m.content}` };
}
