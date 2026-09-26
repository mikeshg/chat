import { convTitle } from "../store";
import type { ConversationView, FriendView, MessageView, UserPublic } from "../types";

// ===== V2 P3-1 =====
// 全局搜索面板的纯逻辑：关键词高亮切分、附件占位摘要、会话标题对照、本地会话过滤、发送者候选。

export interface HiPart {
  text: string;
  hit: boolean;
}

/** 把摘要按关键词切成片段：大小写不敏感、关键词按字面量（正则元字符无特殊含义）、多处命中全切。 */
export function highlightParts(text: string, q: string): HiPart[] {
  const needle = q.trim().toLowerCase();
  if (!text) return [];
  if (!needle) return [{ text, hit: false }];
  const lower = text.toLowerCase();
  const parts: HiPart[] = [];
  let i = 0;
  for (;;) {
    const at = lower.indexOf(needle, i);
    if (at < 0) {
      if (i < text.length) parts.push({ text: text.slice(i), hit: false });
      break;
    }
    if (at > i) parts.push({ text: text.slice(i, at), hit: false });
    parts.push({ text: text.slice(at, at + needle.length), hit: true });
    i = at + needle.length;
  }
  return parts;
}

/** 搜索结果行摘要：附件消息显示占位文字（与气泡口径一致），不暴露文件元数据之外的原文 */
export function snippetOf(m: MessageView): string {
  if (!m.attachment) return m.content;
  if (m.kind === "image") return "[图片]";
  if (m.kind === "audio" || m.attachment.duration_ms != null) return "[语音]";
  if (m.kind === "file") return `[文件] ${m.attachment.file_name}`;
  return m.content;
}

/** conversation_id → 会话显示名（对照本地会话目录；已退出/不在目录的会话兜底「会话」） */
export function convTitleOf(
  convs: ConversationView[],
  id: string,
  meId?: string,
): string {
  const c = convs.find((x) => x.id === id);
  return c ? convTitle(c, meId) : "会话";
}

/** 「会话」分组：按标题本地过滤（不区分会话/归档页签，与目录全量一致） */
export function matchConvsByTitle(
  convs: ConversationView[],
  q: string,
  meId?: string,
): ConversationView[] {
  const kw = q.trim().toLowerCase();
  if (!kw) return [];
  return convs.filter((c) => convTitle(c, meId).toLowerCase().includes(kw));
}

/** 发送者筛选候选：好友 + 会话 peer + 本地已缓存消息里出现过的发送者，去重按显示名排序 */
export function senderOptions(
  friends: FriendView[],
  convs: ConversationView[],
  messages: Record<string, MessageView[]>,
): UserPublic[] {
  const map = new Map<string, UserPublic>();
  for (const f of friends) map.set(f.user.id, f.user);
  for (const c of convs) {
    if (c.peer) map.set(c.peer.id, c.peer);
  }
  for (const list of Object.values(messages)) {
    for (const m of list) {
      // 乐观消息（client: 前缀）不进候选
      if (m.id.startsWith("client:") || map.has(m.sender_id)) continue;
      map.set(m.sender_id, {
        id: m.sender_id,
        username: m.sender_username,
        display_name: m.sender_name,
        role: "user",
        last_seen_at: null,
      });
    }
  }
  return [...map.values()].sort((a, b) =>
    a.display_name.localeCompare(b.display_name, "zh"),
  );
}
