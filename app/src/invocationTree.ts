// Agent A15：agent_invocation 帧和历史列表 → 父调用下的子调用树。
// 纯函数。store 只负责按调用 id 合并；渲染只负责把树画出来。

import type { InvocationView } from "./types";

/** 嵌套卡片最多画这么多层，再深的用一句话带过 */
export const NEST_MAX_DEPTH = 5;

/** 与服务端 result_preview 的字数上限一致（Unicode 码位，不是 UTF-16 码元） */
export const RESULT_PREVIEW_CHARS = 2000;

/** parent_invocation_id → 子调用（派出顺序） */
export type ChildIndex = Record<string, InvocationView[]>;

export interface NestNode {
  inv: InvocationView;
  children: NestNode[];
  /** 这一层之下还有调用，但已经超过可显示层数 */
  deeper: boolean;
}

function byDispatch(a: InvocationView, b: InvocationView): number {
  if (a.created_at !== b.created_at) return a.created_at < b.created_at ? -1 : 1;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

/** 把新到的调用并进索引。没有父调用的忽略。同一 id 原地覆盖；新 id 追加后再按创建时间排序。 */
export function mergeChildIndex(index: ChildIndex, incoming: InvocationView[]): ChildIndex {
  if (!incoming.some((inv) => inv.parent_invocation_id)) return index;
  const next: ChildIndex = { ...index };
  const touched = new Set<string>();
  for (const inv of incoming) {
    const parent = inv.parent_invocation_id;
    if (!parent) continue;
    for (const pid of Object.keys(next)) {
      if (pid === parent) continue;
      const list = next[pid];
      if (!list.some((x) => x.id === inv.id)) continue;
      const kept = list.filter((x) => x.id !== inv.id);
      if (kept.length) next[pid] = kept;
      else delete next[pid];
    }
    const without = (next[parent] ?? []).filter((x) => x.id !== inv.id);
    without.push(inv);
    next[parent] = without;
    touched.add(parent);
  }
  for (const pid of touched) {
    const list = next[pid];
    if (list) next[pid] = [...list].sort(byDispatch);
  }
  return next;
}

/** 从某个父调用往下展开。depth 从 1 计（直接子调用是第 1 层）。 */
export function nestUnder(index: ChildIndex, parentId: string, depth = 1): NestNode[] {
  if (depth > NEST_MAX_DEPTH) return [];
  const kids = index[parentId] ?? [];
  return kids.map((inv) => {
    const more = (index[inv.id]?.length ?? 0) > 0;
    if (depth === NEST_MAX_DEPTH) {
      return { inv, children: [], deeper: more };
    }
    return { inv, children: nestUnder(index, inv.id, depth + 1), deeper: false };
  });
}

/** 服务端已经截到 2000 字。这里再按码位看一遍，满上限就让界面加「…」。 */
export function formatResultPreview(preview: string | null | undefined): {
  text: string;
  truncated: boolean;
} {
  const text = preview ?? "";
  const chars = Array.from(text);
  if (chars.length >= RESULT_PREVIEW_CHARS) {
    return { text: chars.slice(0, RESULT_PREVIEW_CHARS).join(""), truncated: true };
  }
  return { text, truncated: false };
}
