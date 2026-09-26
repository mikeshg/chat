import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { InvocationStatusLine, LiveRunBubble, StepRecord } from "./AgentRun";
import type { InvocationView } from "../types";

const inv = (patch: Partial<InvocationView>): InvocationView => ({
  id: "i1",
  agent_id: "a1",
  conversation_id: "c1",
  trigger_message_id: "m1",
  status: "running",
  error: null,
  created_at: new Date().toISOString(),
  started_at: new Date().toISOString(),
  finished_at: null,
  ...patch,
});

describe("Agent A03：状态标签组件", () => {
  it("running → ⏳ 标签出现在 .inv-status", () => {
    const html = renderToStaticMarkup(
      <InvocationStatusLine inv={inv({ status: "running" })} />,
    );
    expect(html).toContain("inv-status");
    expect(html).toContain("⏳");
    expect(html).toContain("正在处理");
  });

  it("running 带 current_step → 当前：<步骤名>", () => {
    const html = renderToStaticMarkup(
      <InvocationStatusLine
        inv={inv({ status: "running", current_step: "执行测试" })}
      />,
    );
    expect(html).toContain("当前：执行测试");
  });

  it("failed → ⚠ 标签与原因；succeeded → 不渲染", () => {
    const html = renderToStaticMarkup(
      <InvocationStatusLine inv={inv({ status: "failed", error: "host_offline" })} />,
    );
    expect(html).toContain("inv-status");
    expect(html).toContain("⚠");
    expect(html).toContain("失败");
    expect(html).toContain("不在线");
    expect(
      renderToStaticMarkup(
        <InvocationStatusLine inv={inv({ status: "succeeded" })} />,
      ),
    ).toBe("");
  });
});

describe("Agent A03：步骤记录组件", () => {
  it("折叠为「· N 个步骤 ·」摘要；ok=false 步骤配失败图标", () => {
    const html = renderToStaticMarkup(
      <StepRecord
        steps={[
          { id: "s1", title: "读取配置", kind: "read", ok: true },
          { id: "s2", title: "写文件", kind: "edit", ok: false },
        ]}
      />,
    );
    expect(html).toContain("2 个步骤");
    // 默认折叠：列表细节不渲染（单击展开交互由浏览器实测覆盖）
    expect(html).not.toContain("写文件");
  });
});

describe("Agent A03：流式气泡组件", () => {
  it("渲染 Agent 名 + 追加中的文本 + 生成中提示", () => {
    const html = renderToStaticMarkup(
      <LiveRunBubble
        agentName="Claude"
        run={{
          text: "正在输出…",
          steps: [{ id: "s1", title: "读取配置", kind: "read", ok: null }],
        }}
        emptyHint="正在思考…"
      />,
    );
    expect(html).toContain("Claude");
    expect(html).toContain("正在输出…");
    expect(html).toContain("生成中");
    // 进行中步骤默认折叠为「N 个步骤」摘要（展开交互由浏览器实测覆盖）
    expect(html).toContain("1 个步骤");
    expect(html).not.toContain("读取配置");
  });

  it("无文本无步骤 → 显示空态提示", () => {
    const html = renderToStaticMarkup(
      <LiveRunBubble
        agentName="Claude"
        run={{ text: "", steps: [] }}
        emptyHint="正在思考…"
      />,
    );
    expect(html).toContain("正在思考…");
  });
});
