// ===== MM2 =====
// 消息菜单 / 点按判定 / 墓碑文案 / 复制文本 —— 全部纯函数，UI 只负责执行。
// menuItems 是唯一菜单项来源：右键菜单、触屏动作面板、预览工具栏共用，
// MM3 的多选等后续能力直接在这里加项。

import type { MessageView, ReplyPreview } from "../../types";
import {
  FEATURE_MENU_EDIT,
  FEATURE_MENU_FAVORITE,
  FEATURE_MENU_PIN,
  FEATURE_MENU_REACTION,
  FEATURE_MULTI_SELECT,
} from "./features";

export type MenuAction =
  | "preview" // 预览（文件）/ 查看大图（图片）
  | "reply"
  | "copy"
  | "forward"
  | "download"
  | "multi"
  | "recall"
  // —— 已下线的生命周期项（feature flag 关闭；代码路径保留）——
  | "reaction"
  | "edit"
  | "pin"
  | "favorite";

export interface MenuItem {
  action: MenuAction;
  label: string;
  /** 危险项（撤回）：菜单里标红 */
  danger?: boolean;
}

export interface MenuCtx {
  meId?: string;
  /** 本人在该群的成员角色（"owner" | 其他）；DM/self 会话无意义 */
  myRole?: string;
  convType: string; // "dm" | "group" | "self"
  /** 打开菜单时的本地时钟（撤回窗口按客户端算，服务端仍是权威判定） */
  now: number;
  /** 只读会话（好友已解除的 DM）：回复类动作隐藏，复制/转发保留 */
  readOnly: boolean;
  /** 服务端是否支持 MM1 撤回语义（旧服务端撤回=无时限删除，见 api.serverSupportsRecall） */
  serverSupportsRecall: boolean;
  /** 该消息当前是否已置顶（仅下线中的「置顶」项用） */
  pinned?: boolean;
}

/** 发送者撤回窗口：24h（与服务端一致；客户端只决定菜单显隐，服务端权威） */
export const RECALL_WINDOW_MS = 24 * 60 * 60 * 1000;
/** 编辑窗口：15 分钟（旧 hover 工具栏同款判定，菜单项下线后仍保留给编辑路径） */
export const EDIT_WINDOW_MS = 15 * 60 * 1000;

export function isPending(m: MessageView): boolean {
  return m.id.startsWith("client:");
}

export function isTombstone(m: MessageView): boolean {
  return !!m.deleted_at;
}

/** MM1 撤回产生的墓碑（recalled_by 非空）；历史软删除墓碑无该字段 */
export function isRecallTombstone(m: MessageView): boolean {
  return !!m.deleted_at && m.recalled_by != null;
}

/** 撤回入口显隐（服务端仍二次校验；recall_expired 由错误提示兜底） */
export function canRecall(m: MessageView, ctx: MenuCtx): boolean {
  if (isTombstone(m) || isPending(m)) return false;
  if (m.sender_id === ctx.meId) {
    // 旧服务端没有 24h 撤回窗（撤回即旧版删除）
    return ctx.serverSupportsRecall
      ? ctx.now - Date.parse(m.created_at) < RECALL_WINDOW_MS
      : true;
  }
  // 群主可撤群内他人消息，无时限；站点 admin 无额外权限
  return ctx.convType === "group" && ctx.myRole === "owner";
}

/** 编辑入口（下线功能保留的判定路径）：本人文本消息、15 分钟内 */
export function canEditMsg(m: MessageView, ctx: MenuCtx): boolean {
  return (
    !isTombstone(m) &&
    !isPending(m) &&
    m.sender_id === ctx.meId &&
    m.kind === "text" &&
    ctx.now - Date.parse(m.created_at) < EDIT_WINDOW_MS
  );
}

/** 回复等写类动作：只读会话隐藏；墓碑/乐观消息由 menuItems 的早退兜底 */
export function canAct(m: MessageView, ctx: MenuCtx): boolean {
  return !ctx.readOnly && !isTombstone(m) && !isPending(m);
}

/** 转发：非投票、非墓碑、非乐观消息；只读会话仍可转出 */
export function canForward(m: MessageView): boolean {
  return !isTombstone(m) && !isPending(m) && m.kind !== "poll" && m.kind !== "system";
}

/**
 * 单条消息的全部菜单项（唯一来源）。顺序即展示顺序：
 * 预览/查看大图（附件，最前）→ 回复 → 复制 → 转发 → 下载 → [多选] → 撤回（末位红色）。
 * 墓碑与乐观消息没有任何菜单项（调用方据此不开菜单）。
 * 多选在 FEATURE_MULTI_SELECT 后：MM3 暂缓，等合并转发设计定稿再接回。
 */
export function menuItems(m: MessageView, ctx: MenuCtx): MenuItem[] {
  if (isTombstone(m) || isPending(m) || m.kind === "system") return [];
  const items: MenuItem[] = [];
  if (m.attachment) {
    items.push({ action: "preview", label: m.kind === "image" ? "查看大图" : "预览" });
  }
  if (canAct(m, ctx)) items.push({ action: "reply", label: "回复" });
  // MF2：聊天记录卡片不提供「复制」（快照文本非原文，复制会误导；菜单恰为 回复/转发/多选/撤回）
  if (m.kind !== "chat_record") items.push({ action: "copy", label: "复制" });
  // —— 下线的旧菜单项：开关关闭，代码保留（MM2 审查口径） ——
  if (FEATURE_MENU_REACTION && canAct(m, ctx)) items.push({ action: "reaction", label: "表情" });
  if (FEATURE_MENU_EDIT && canEditMsg(m, ctx)) items.push({ action: "edit", label: "编辑" });
  if (canForward(m)) items.push({ action: "forward", label: "转发" });
  if (FEATURE_MENU_PIN && canAct(m, ctx)) {
    items.push({ action: "pin", label: ctx.pinned ? "取消置顶" : "置顶" });
  }
  if (FEATURE_MENU_FAVORITE && canForward(m)) items.push({ action: "favorite", label: "收藏" });
  if (m.attachment) items.push({ action: "download", label: "下载" });
  // MF2 多选：菜单位置在下载之后、撤回之前（查看器/预览工具栏不进此项——那里手动挑白名单）
  if (FEATURE_MULTI_SELECT) items.push({ action: "multi", label: "多选" });
  if (canRecall(m, ctx)) items.push({ action: "recall", label: "撤回", danger: true });
  return items;
}

// ---------- 触屏点按 → 底部动作面板 ----------

/** 位移超过该值视为滑动，不弹菜单 */
export const TAP_MOVE_PX = 10;
/** 按压超过该值视为长按（交给系统文本选择），不弹菜单 */
export const TAP_PRESS_MS = 500;

/**
 * 点按不弹菜单的目标：这些元素有自己的动作（链接/按钮/输入框/音视频/
 * 回复引用/表态 chip/投票选项/代码块复制/重试按钮/自定义 data-own-action）。
 */
export const TAP_EXCLUDED_SEL =
  "a, button, input, textarea, audio, video, select, .reply-quote, .rx-chip, .poll-opt, .rt-copy, .send-fail, [data-own-action]";

export interface TapGesture {
  /** pointerup 的事件目标 */
  target: unknown;
  /** pointerdown→up 的位移（px） */
  movedPx: number;
  /** 按压时长（ms） */
  pressMs: number;
  /** 抬起瞬间是否存在非空文本选区 */
  hasSelection: boolean;
}

/** 目标是否落在「自己有动作」的元素里（closest 语义；target 只需有 closest 方法） */
export function tapTargetExcluded(target: unknown): boolean {
  const el = target as { closest?: (sel: string) => unknown } | null;
  return !!el && typeof el.closest === "function" && !!el.closest(TAP_EXCLUDED_SEL);
}

/** 单次点按是否应打开消息菜单（触屏 action sheet）；全部条件同时满足才开 */
export function shouldOpenMenuOnTap(g: TapGesture): boolean {
  if (g.movedPx > TAP_MOVE_PX) return false;
  if (g.pressMs > TAP_PRESS_MS) return false;
  if (g.hasSelection) return false;
  return !tapTargetExcluded(g.target);
}

/**
 * contextmenu 是否放行菜单：Android WebView 长按也发 contextmenu——
 * 源指针是 touch 时让给原生长按（系统文本选择），只有鼠标/笔右键开菜单；
 * 无指针来源（键盘菜单键等）照样开。
 */
export function shouldOpenMenuOnContextMenu(lastPointerType: string | null | undefined): boolean {
  return lastPointerType !== "touch";
}

/**
 * click 路径的点按判定输入：pointerdown/up 记下的手势（pointerType/位移/按压）
 * + click 当时的目标与选区（面板此时尚未打开，目标即抬起位置的元素）。
 */
export interface TapClick extends TapGesture {
  /** pointerdown 的 pointerType（仅触屏点按开面板） */
  pointerType: string;
}

/**
 * 触屏点按在 click 阶段才开面板：pointerup 就开会让随后的兼容 click 落到
 * 刚渲染的面板项上，触发随机动作（Android WebView 幽灵点击，MM2-fix1）。
 * pointerType 不是 touch 一律不开——鼠标单击行为照旧。
 */
export function shouldOpenMenuOnClick(t: TapClick): boolean {
  if (t.pointerType !== "touch") return false;
  return shouldOpenMenuOnTap(t);
}

/**
 * 浮层 Esc 关闭判定（右键菜单/触屏动作面板共用，MM2 fix3）：
 * 调用方在 document 上以「捕获阶段」挂 keydown——composer 的 Esc 处理挂在
 * textarea 上，捕获拦截后事件不再下沉，两处逻辑不会连发。
 * 返回 true = 已处理（已 stopPropagation + onClose）。
 */
export function escClose(
  e: { key: string; stopPropagation: () => void },
  onClose: () => void,
): boolean {
  if (e.key !== "Escape") return false;
  e.stopPropagation();
  onClose();
  return true;
}

/** 面板/菜单/预览刚打开时的幽灵点击豁免窗：同上防御，兼容 click 可能落到新浮层项上 */
export const GHOST_CLICK_MS = 350;

/** 浮层打开记录：at=打开时刻；touch=是否由触屏点按打开（豁免窗只对触屏打开生效） */
export interface OverlayOpen {
  at: number;
  touch: boolean;
}

/**
 * 幽灵点击判定：只有「触屏点按打开」的浮层才有兼容 click 落到刚渲染浮层项
 * 上的问题（Android WebView）；鼠标/笔打开的菜单与预览永不忽略点击——
 * 否则右键后快速点「复制」这类正常操作会被吞掉。
 */
export function isGhostClick(opened: OverlayOpen, now = Date.now()): boolean {
  return opened.touch && now - opened.at < GHOST_CLICK_MS;
}

// ---------- 墓碑文案 ----------

export interface TombstoneCtx {
  meId?: string;
  convType: string; // "dm" | "group" | "self"
}

/**
 * 墓碑正文文案（消息气泡与会话列表预览共用）。
 * 返回 null 表示不是墓碑。
 */
export function tombstoneText(m: MessageView, ctx: TombstoneCtx): string | null {
  if (!m.deleted_at) return null;
  const by = m.recalled_by;
  if (by == null) return "消息已删除"; // MM1 之前的历史墓碑保持旧文案
  const me = ctx.meId;
  const senderName = m.sender_name;
  const byName = m.recalled_by_name ?? senderName;
  if (by === me) {
    // 我撤自己的 → 你撤回了一条消息；我是群主撤别人的 → 你撤回了 X 的一条消息
    return m.sender_id === me ? "你撤回了一条消息" : `你撤回了 ${senderName} 的一条消息`;
  }
  if (m.sender_id === me) return "群主撤回了你的一条消息"; // 我的消息被群主撤
  if (ctx.convType === "dm") return "对方撤回了一条消息";
  if (by === m.sender_id) return `${byName} 撤回了一条消息`; // 群成员各撤各的
  return `群主撤回了 ${senderName} 的一条消息`; // recalled_by ≠ sender：现群主撤的
}

/** 回复引用块里被撤/被删原消息的文案（预览只有 deleted + recalled_by_name） */
export function quoteDeletedText(p: ReplyPreview): string {
  return p.recalled_by_name != null ? "原消息已撤回" : "原消息已删除";
}

/** 消息级版本：composer 回复横幅等持有完整 MessageView 时用 */
export function msgDeletedText(m: MessageView): string {
  return m.recalled_by != null ? "原消息已撤回" : "原消息已删除";
}

// ---------- 复制 ----------

/** 「复制」项的文本：文本→正文（@按原文）；附件→文件名；投票→标题 */
export function copyTextFor(m: MessageView): string {
  if (m.kind === "poll") return m.poll?.question ?? m.content;
  if (m.attachment) return m.attachment.file_name;
  return m.content;
}

/**
 * 「重新编辑」暂存的正文：附件消息没有用户配文，content 是服务端占位
 * （[图片] / [语音] / [文件] <名>，见 server send.rs），回填只能是空串——
 * 原件由「复用附件」chip 恢复。
 */
export function reeditTextFor(m: MessageView): string {
  return m.attachment ? "" : m.content;
}
