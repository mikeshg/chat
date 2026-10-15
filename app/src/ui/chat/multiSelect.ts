// ===== MF2 多选模式：纯状态机（不依赖 React，便于单测） =====
// 选中集是服务端消息 id 的 Set，作用域 = (server, conversation)。
// 墓碑/乐观(client:)/投票行置灰不可选；点行任意位置切换勾选（含卡片、⋯、链接）。

import type { MessageView } from "../../types";
import { isPending, isTombstone } from "./msgMenu";

export const MULTI_SELECT_MAX = 100;
export const MS_CAP_TOAST = `最多选择 ${MULTI_SELECT_MAX} 条`;

/** 某条消息在多选模式下能否勾选（墓碑/乐观/投票置灰，点击无反应、不提示） */
export function selectable(m: MessageView): boolean {
  return !isTombstone(m) && !isPending(m) && m.kind !== "poll" && m.kind !== "system";
}

export interface MultiSelect {
  /** 归属会话：切换会话/服务端/登出即整个丢弃 */
  convId: string;
  /** 已选的服务端消息 id；乐观消息确认后换新 id 时不会自动带入 */
  ids: ReadonlySet<string>;
}

/** 由消息菜单「多选」进入：能选则预选中该条（投票等不可选条目只进模式不预选） */
export function enterMultiSelect(convId: string, m: MessageView): MultiSelect {
  const ids = new Set<string>();
  if (selectable(m)) ids.add(m.id);
  return { convId, ids };
}

export interface ToggleResult {
  next: MultiSelect;
  /** 选第 101 条被拒时的提示文案 */
  toast?: string;
}

/** 点行切换勾选；不可选行静默忽略 */
export function toggleSelect(ms: MultiSelect, m: MessageView): ToggleResult {
  if (!selectable(m)) return { next: ms };
  if (ms.ids.has(m.id)) {
    const ids = new Set(ms.ids);
    ids.delete(m.id);
    return { next: { ...ms, ids } };
  }
  if (ms.ids.size >= MULTI_SELECT_MAX) return { next: ms, toast: MS_CAP_TOAST };
  const ids = new Set(ms.ids);
  ids.add(m.id);
  return { next: { ...ms, ids } };
}

/**
 * 收编：列表里已是墓碑（WS 撤回/删除）的选中 id 剔除。
 * 不在当前已加载窗口里的 id 保留——可能分页还没拉到，不视为消失。
 */
export function pruneSelection(ms: MultiSelect, msgs: MessageView[]): MultiSelect {
  const byId = new Map(msgs.map((m) => [m.id, m]));
  let changed = false;
  const ids = new Set<string>();
  for (const id of ms.ids) {
    const m = byId.get(id);
    if (!m || selectable(m)) ids.add(id);
    else changed = true;
  }
  return changed ? { ...ms, ids } : ms;
}

/** 提交时取选中消息、按 seq（气泡序）排好的服务端 id 列表 */
export function orderedSelectedIds(ms: MultiSelect, msgs: MessageView[]): string[] {
  const sel = ms.ids;
  return msgs.filter((m) => sel.has(m.id) && selectable(m)).map((m) => m.id);
}
