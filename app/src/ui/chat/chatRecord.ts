// ===== MF2 聊天记录（chat_record）：标题/标签/卡片行/日期分组（纯函数，便于单测） =====
// 快照只在组件内存里活；本模块只做文本/结构变换，不含任何 IO。

import type { ChatRecord, MessageView, RecordItem, RecordPreview, RecordPreviewItem } from "../../types";

export const RECORD_TAG = "[聊天记录]";
/** 列表/置顶/通知里标题超过 30 字截断 */
export const RECORD_TITLE_MAX = 30;

const cut = (s: string, max: number) => {
  const a = Array.from(s);
  return a.length > max ? a.slice(0, max).join("") + "…" : s;
};

/**
 * 记录标题：优先 record_preview.title；
 * 缺预览时取 content 首行并剥掉一个前导「[聊天记录] 」（兜底文本首行就是它，避免双重前缀）。
 */
export function recordTitle(
  m: Pick<MessageView, "kind" | "content"> & { record_preview?: RecordPreview | null },
): string {
  const t = m.record_preview?.title?.trim();
  if (t) return t;
  const first = (m.content || "").split("\n")[0].trim();
  return first.startsWith(`${RECORD_TAG} `) ? first.slice(RECORD_TAG.length).trim() : first;
}

/** 通用标签「[聊天记录] {title}」；maxTitle>0 时标题截断（会话列表/置顶/通知用 30） */
export function recordLabel(
  m: Pick<MessageView, "kind" | "content"> & { record_preview?: RecordPreview | null },
  maxTitle = 0,
): string {
  const t = recordTitle(m);
  return `${RECORD_TAG} ${maxTitle > 0 ? cut(t, maxTitle) : t}`.trimEnd();
}

/** 预览行摘要文本：服务端已带 [ 前缀的照用，否则按 kind 补标签（[图片]/[文件] 名/[语音]/[聊天记录]） */
export function recordItemText(item: Pick<RecordPreviewItem, "kind" | "text">): string {
  const text = (item.text ?? "").trim();
  if (text.startsWith("[")) return text;
  const tag =
    item.kind === "image"
      ? "[图片]"
      : item.kind === "file"
        ? "[文件]"
        : item.kind === "audio"
          ? "[语音]"
          : item.kind === "chat_record"
            ? RECORD_TAG
            : "";
  return tag ? `${tag} ${text}`.trimEnd() : text;
}

/** 卡片摘要行：最多 4 条「发送者：摘要」（count>4 也不再补「等 N 条」行） */
export function recordCardLines(preview: RecordPreview | null | undefined): { sender: string; text: string }[] {
  return (preview?.items ?? []).slice(0, 4).map((it) => ({
    sender: it.sender_name,
    text: recordItemText(it),
  }));
}

export interface RecordSection {
  /** 本地日期的分隔键（同一天内的条目同组） */
  dayKey: string;
  /** 展示用日期文案（设备本地时区） */
  dayLabel: string;
  items: RecordItem[];
}

/** 本地日期分隔文案——与 ChatView 的时间展示同一套 locale 习惯（toLocaleDateString） */
export function recordDayLabel(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? "" : d.toLocaleDateString();
}

const dayKey = (iso: string) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? "?" : `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
};

/** 按本地自然日分组（REQUIRE 5：跨天插日期分隔条） */
export function recordSections(items: RecordItem[]): RecordSection[] {
  const out: RecordSection[] = [];
  for (const it of items) {
    const k = dayKey(it.created_at);
    const last = out[out.length - 1];
    if (last && last.dayKey === k) last.items.push(it);
    else out.push({ dayKey: k, dayLabel: recordDayLabel(it.created_at), items: [it] });
  }
  return out;
}

/** 条目行内时间（HH:MM，设备本地） */
export function recordItemTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

// ---------- MF2-C8 记录上下文查看器工具栏 ----------

export interface ViewerAction {
  /** 复用既有 testid，顺序固定：下载 → 转发 → 关闭 */
  testid: "viewer-download" | "viewer-forward" | "viewer-close";
  label: string;
}

/** 记录上下文工具栏（C8）：恰为 下载/转发/关闭 三个；无回复/撤回/多选 */
export const recordViewerActions = (): ViewerAction[] => [
  { testid: "viewer-download", label: "下载" },
  { testid: "viewer-forward", label: "转发" },
  { testid: "viewer-close", label: "关闭" },
];

/**
 * 预览/查看器工具栏的 testid 顺序（含收尾的 viewer-close）。
 * record 上下文恰为 [download, forward, close]；
 * normal 上下文 = 菜单派生项（reply/forward/recall 序）+ download + close，多选永不进工具栏。
 */
export function viewerToolbarIds(kind: "record" | "normal", menuActionIds: string[] = []): string[] {
  if (kind === "record") return ["viewer-download", "viewer-forward", "viewer-close"];
  const acts = menuActionIds
    .filter((a) => ["reply", "forward", "recall"].includes(a))
    .map((a) => `viewer-${a}`);
  return [...acts, "viewer-download", "viewer-close"];
}

/** 快照条目里的克隆附件可直接喂给现有附件管线（预览/下载/音频播放） */
export const recordItemAttachment = (it: RecordItem) => it.attachment;

/** 嵌套记录层压栈用：该条目可点开下一层 iff 有内嵌快照 */
export const hasNestedRecord = (it: RecordItem): it is RecordItem & { record: ChatRecord } =>
  it.kind === "chat_record" && !!it.record;
