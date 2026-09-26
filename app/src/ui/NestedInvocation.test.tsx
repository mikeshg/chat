import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { NestCard } from "./NestedInvocation";
import type { NestNode } from "../invocationTree";
import type { InvocationView } from "../types";

function inv(patch: Partial<InvocationView> = {}): InvocationView {
  return {
    id: "c1",
    agent_id: "a1",
    conversation_id: "b-conv",
    trigger_message_id: "t1",
    status: "running",
    error: null,
    created_at: "2026-01-01T00:00:00Z",
    started_at: "2026-01-01T00:00:00Z",
    finished_at: null,
    parent_invocation_id: "p1",
    agent_name: "b",
    ...patch,
  };
}

function node(patch: Partial<InvocationView> = {}, extra: Partial<NestNode> = {}): NestNode {
  return { inv: inv(patch), children: [], deeper: false, ...extra };
}

describe("Agent A15：嵌套子调用卡片", () => {
  it("默认收起：标题是 ↳ 🤖 名字 · 正在处理，不展示结果，没有取消按钮", () => {
    const html = renderToStaticMarkup(
      <NestCard
        node={node({ result_preview: "hi" })}
        open={false}
        onToggle={() => {}}
        live={{ text: "实时", steps: [] }}
      />,
    );
    expect(html).toContain("↳ 🤖 b · ");
    expect(html).toContain("正在处理");
    expect(html).not.toContain("实时");
    expect(html).not.toContain(">hi<");
    expect(html).not.toContain("inv-cancel");
  });

  it("stream 进行中展开后复用生成中气泡；status/steps 没有实时输出时只留状态", () => {
    const live = renderToStaticMarkup(
      <NestCard
        node={node()}
        open
        onToggle={() => {}}
        live={{ text: "想到一半", steps: [] }}
      />,
    );
    expect(live).toContain("生成中");
    expect(live).toContain("想到一半");
    expect(live).not.toContain("inv-cancel");

    const quiet = renderToStaticMarkup(
      <NestCard node={node({ current_step: "读取配置" })} open onToggle={() => {}} live={null} />,
    );
    expect(quiet).toContain("当前：读取配置");
    expect(quiet).not.toContain("生成中");
  });

  it("完成后展示预览，超长加省略号；失败和取消展示原因", () => {
    const done = renderToStaticMarkup(
      <NestCard
        node={node({
          status: "succeeded",
          finished_at: "2026-01-01T00:00:02Z",
          result_preview: "你".repeat(2000),
        })}
        open
        onToggle={() => {}}
        live={null}
      />,
    );
    expect(done).toContain("↳ 🤖 b · ");
    expect(done).toContain("完成");
    expect(done).toContain("…");
    expect(done).not.toContain("生成中");

    const failed = renderToStaticMarkup(
      <NestCard
        node={node({ status: "failed", error: "host_offline", finished_at: "t" })}
        open
        onToggle={() => {}}
        live={null}
      />,
    );
    expect(failed).toContain("失败：Agent 所在电脑不在线");

    const cancelled = renderToStaticMarkup(
      <NestCard
        node={node({ status: "cancelled", error: null, finished_at: "t" })}
        open
        onToggle={() => {}}
        live={null}
      />,
    );
    expect(cancelled).toContain("已取消");
    expect(cancelled).not.toContain("inv-cancel");
  });

  it("更深的一层显示「还有更深的调用」，不再渲染那一层的名字", () => {
    const html = renderToStaticMarkup(
      <NestCard
        node={node({ id: "n5", agent_name: "e" }, { deeper: true })}
        open
        onToggle={() => {}}
        live={null}
      />,
    );
    expect(html).toContain("还有更深的调用");
    expect(html).toContain("↳ 🤖 e · ");
    expect(html).not.toContain("↳ 🤖 f");
  });
});
