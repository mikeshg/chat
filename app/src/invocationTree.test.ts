import { describe, expect, it } from "vitest";
import {
  NEST_MAX_DEPTH,
  RESULT_PREVIEW_CHARS,
  formatResultPreview,
  mergeChildIndex,
  nestUnder,
  type ChildIndex,
} from "./invocationTree";
import type { InvocationView } from "./types";

function inv(patch: Partial<InvocationView> & Pick<InvocationView, "id">): InvocationView {
  return {
    agent_id: "agent",
    conversation_id: "conv-b",
    trigger_message_id: `trig-${patch.id}`,
    status: "running",
    error: null,
    created_at: "2026-01-01T00:00:00Z",
    started_at: "2026-01-01T00:00:00Z",
    finished_at: null,
    parent_invocation_id: "parent",
    agent_name: "b",
    ...patch,
  };
}

describe("Agent A15：帧 / 历史 → 调用树", () => {
  it("乱序到达仍按派出时间排列，父调用本身不进索引", () => {
    const older = inv({ id: "c1", created_at: "2026-01-01T00:00:00Z", agent_name: "b" });
    const newer = inv({ id: "c2", created_at: "2026-01-02T00:00:00Z", agent_name: "c" });
    const root = inv({
      id: "p",
      conversation_id: "group",
      parent_invocation_id: null,
      agent_name: "a",
    });
    const prev: ChildIndex = {};
    // 子调用比父调用的帧先到，且后派出的先到
    const idx = mergeChildIndex(mergeChildIndex(prev, [newer, root]), [older]);
    expect(idx.p).toBeUndefined();
    expect(idx.parent.map((x) => x.id)).toEqual(["c1", "c2"]);
    expect(nestUnder(idx, "parent").map((n) => n.inv.id)).toEqual(["c1", "c2"]);
    // 没有子调用时保持原引用
    expect(mergeChildIndex(idx, [root])).toBe(idx);
  });

  it("同一调用多次更新：原地覆盖，不新开一行、不改顺序", () => {
    const c1 = inv({ id: "c1", created_at: "2026-01-01T00:00:00Z" });
    const c2 = inv({ id: "c2", created_at: "2026-01-01T00:00:01Z", agent_name: "c" });
    let idx = mergeChildIndex({}, [c1, c2]);
    idx = mergeChildIndex(idx, [
      { ...c1, status: "succeeded", finished_at: "2026-01-01T00:00:02Z", result_preview: "hi" },
    ]);
    expect(idx.parent.map((x) => x.id)).toEqual(["c1", "c2"]);
    expect(idx.parent[0].status).toBe("succeeded");
    expect(idx.parent[0].result_preview).toBe("hi");
    expect(idx.parent[1].status).toBe("running");
  });

  it("多层嵌套：子调用挂在各自的父调用下面", () => {
    const idx = mergeChildIndex({}, [
      inv({ id: "a", parent_invocation_id: "root", agent_name: "a" }),
      inv({ id: "b", parent_invocation_id: "a", agent_name: "b", created_at: "2026-01-01T00:00:01Z" }),
      inv({ id: "c", parent_invocation_id: "b", agent_name: "c", created_at: "2026-01-01T00:00:02Z" }),
    ]);
    const tree = nestUnder(idx, "root");
    expect(tree).toHaveLength(1);
    expect(tree[0].inv.id).toBe("a");
    expect(tree[0].deeper).toBe(false);
    expect(tree[0].children[0].inv.id).toBe("b");
    expect(tree[0].children[0].children[0].inv.id).toBe("c");
    expect(tree[0].children[0].children[0].children).toEqual([]);
  });

  it(`超过 ${NEST_MAX_DEPTH} 层不再展开，标出还有更深的调用`, () => {
    const rows: InvocationView[] = [];
    let parent = "root";
    for (let n = 1; n <= NEST_MAX_DEPTH + 1; n++) {
      const id = `n${n}`;
      rows.push(
        inv({
          id,
          parent_invocation_id: parent,
          agent_name: `a${n}`,
          created_at: `2026-01-01T00:00:0${n}Z`,
        }),
      );
      parent = id;
    }
    const tree = nestUnder(mergeChildIndex({}, rows), "root");
    let node = tree[0];
    for (let depth = 1; depth < NEST_MAX_DEPTH; depth++) {
      expect(node.deeper).toBe(false);
      expect(node.inv.id).toBe(`n${depth}`);
      node = node.children[0];
    }
    expect(node.inv.id).toBe(`n${NEST_MAX_DEPTH}`);
    expect(node.children).toEqual([]);
    expect(node.deeper).toBe(true);
  });

  it("结果预览按字截断并在超长时加省略号", () => {
    expect(formatResultPreview("hi")).toEqual({ text: "hi", truncated: false });
    expect(formatResultPreview(null)).toEqual({ text: "", truncated: false });
    expect(formatResultPreview("你".repeat(RESULT_PREVIEW_CHARS - 1)).truncated).toBe(false);
    const full = `${"你".repeat(RESULT_PREVIEW_CHARS - 1)}😀`;
    const shown = formatResultPreview(full);
    expect(shown.truncated).toBe(true);
    expect(Array.from(shown.text)).toHaveLength(RESULT_PREVIEW_CHARS);
    expect(shown.text.endsWith("😀")).toBe(true);
  });
});
