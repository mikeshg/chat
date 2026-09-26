import { describe, expect, it } from "vitest";
import { applyStreamFrame, dropRun, fmtElapsed, type LiveRun } from "./agentRun";
import { invocationStatusView } from "./store";
import type { InvocationView } from "./types";

// ===== Agent A03：agent_stream 帧 → 渲染状态（纯函数）+ 状态标签查表 =====

const CONV = "c1";
const frame = (invocation_id: string, patch: object) => ({
  invocation_id,
  conversation_id: CONV,
  ...patch,
});

describe("Agent A03：applyStreamFrame", () => {
  it("delta 追加文本；step 按 id upsert（开始 → 进行中，结束 → ok）", () => {
    let runs: Record<string, LiveRun> | undefined;
    runs = applyStreamFrame(runs, frame("i1", { delta: "你好" }));
    runs = applyStreamFrame(
      runs,
      frame("i1", { step: { id: "s1", title: "读取文件", kind: "read", ok: null } }),
    );
    runs = applyStreamFrame(runs, frame("i1", { delta: "，世界" }));
    runs = applyStreamFrame(
      runs,
      frame("i1", { step: { id: "s1", title: "读取文件", kind: "read", ok: true } }),
    );
    runs = applyStreamFrame(
      runs,
      frame("i1", { step: { id: "s2", title: "执行测试", kind: "execute", ok: false } }),
    );

    const run = runs["i1"];
    expect(run.text).toBe("你好，世界");
    expect(run.steps).toEqual([
      { id: "s1", title: "读取文件", kind: "read", ok: true },
      { id: "s2", title: "执行测试", kind: "execute", ok: false },
    ]);
  });

  it("不同 invocation 的状态互相隔离", () => {
    let runs: Record<string, LiveRun> | undefined;
    runs = applyStreamFrame(runs, frame("i1", { delta: "A" }));
    runs = applyStreamFrame(runs, frame("i2", { delta: "B" }));
    expect(runs["i1"].text).toBe("A");
    expect(runs["i2"].text).toBe("B");
  });

  it("空帧（无 delta 无 step）不产生状态变化", () => {
    const runs = applyStreamFrame(undefined, frame("i1", { delta: "x" }));
    const same = applyStreamFrame(runs, frame("i1", {}));
    expect(same).toBe(runs);
    expect(applyStreamFrame(undefined, frame("i9", {}))).toEqual({});
  });
});

describe("Agent A03：dropRun（终态清理）", () => {
  it("摘除指定调用的临时输出，别的调用保留", () => {
    let runs = applyStreamFrame(undefined, frame("i1", { delta: "a" }));
    runs = applyStreamFrame(runs, frame("i2", { delta: "b" }));
    const next = dropRun(runs, "i1");
    expect(next?.["i1"]).toBeUndefined();
    expect(next?.["i2"].text).toBe("b");
    // 不存在的 id：返回同一引用（无变化）
    expect(dropRun(runs, "nope")).toBe(runs);
    expect(dropRun(undefined, "i1")).toBeUndefined();
  });
});

// ----- 状态标签查表 -----

const inv = (patch: Partial<InvocationView>): InvocationView => ({
  id: "i1",
  agent_id: "a1",
  conversation_id: CONV,
  trigger_message_id: "m1",
  status: "running",
  error: null,
  created_at: "t0",
  started_at: "t0",
  finished_at: null,
  ...patch,
});

describe("Agent A03：invocationStatusView 查表", () => {
  it("running 无步骤 → 正在处理", () => {
    const v = invocationStatusView(inv({ status: "running" }), 0);
    expect(v).not.toBeNull();
    expect(v!.icon).toBe("⏳");
    expect(v!.text).toBe("正在处理");
  });

  it("running 带 current_step → 正在工作 · 已用 m:ss · 当前：<步骤>", () => {
    const v = invocationStatusView(
      inv({ status: "running", current_step: "读取配置" }),
      65,
    );
    expect(v!.text).toBe("正在工作 · 已用 1:05 · 当前：读取配置");
  });

  it("failed → 失败及原因（稳定错误码映射中文；未知码透传原文）", () => {
    const v = invocationStatusView(inv({ status: "failed", error: "host_offline" }), 0);
    expect(v!.icon).toBe("⚠");
    expect(v!.text).toContain("失败");
    expect(v!.text).toContain("不在线");
    const raw = invocationStatusView(inv({ status: "failed", error: "boom" }), 0);
    expect(raw!.text).toContain("boom");
  });

  it("succeeded / 未知状态 → null（不渲染）", () => {
    expect(invocationStatusView(inv({ status: "succeeded" }), 0)).toBeNull();
    expect(invocationStatusView(inv({ status: "weird" }), 0)).toBeNull();
  });

  it("A04 的排队 / 取消 / 超时也在同一张表里", () => {
    expect(invocationStatusView(inv({ status: "queued" }), 0)?.text).toBe("排队中：Agent 所在电脑不在线");
    expect(invocationStatusView(inv({ status: "cancelled" }), 0)?.text).toBe("已取消");
    expect(invocationStatusView(inv({ status: "timed_out" }), 0)?.text).toBe("排队超时，已取消");
  });
});

describe("Agent A03：fmtElapsed", () => {
  it("秒 → m:ss", () => {
    expect(fmtElapsed(0)).toBe("0:00");
    expect(fmtElapsed(7)).toBe("0:07");
    expect(fmtElapsed(65)).toBe("1:05");
    expect(fmtElapsed(600)).toBe("10:00");
  });
});
