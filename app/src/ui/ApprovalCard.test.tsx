// ===== Agent A20 =====
// 审批卡片完整披露：request.detail 全文展示（保留换行、不截断），
// locations 全部列出；没有 detail 的老卡片不出现正文区。

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import ApprovalCard from "./ApprovalCard";
import type { ApprovalView } from "../types";

const approval = (request: ApprovalView["request"], over: Partial<ApprovalView> = {}): ApprovalView => ({
  id: "ap1",
  status: "pending",
  request: {
    title: "发消息到「项目群」：开头",
    kind: "send_message",
    request_id: "r1",
    invocation_id: "i1",
    ...request,
  },
  options: [
    { optionId: "allow_once", name: "本次允许", kind: "allow_once" },
    { optionId: "reject_once", name: "拒绝", kind: "reject_once" },
  ],
  ...over,
});

describe("Agent A20：审批卡片完整披露", () => {
  it("展示 detail 全文（含 80 字预览之外的部分）和全部路径", () => {
    const tail = "这是预览八十字之后才出现的尾巴";
    const detail = `第一行\n第二行\n${"填".repeat(100)}${tail}`;
    const a = approval({
      detail,
      locations: ["C:\\work\\a.txt", "D:\\more\\b.txt"],
    });
    const html = renderToStaticMarkup(<ApprovalCard approval={a} content="请求操作：发消息" />);
    expect(html).toContain("approval-detail");
    expect(html).toContain("第一行");
    expect(html).toContain(tail);
    // 换行保留在 pre 里
    expect(html).toContain(`第二行\n${"填".repeat(100)}`);
    expect(html).toContain("C:\\work\\a.txt");
    expect(html).toContain("D:\\more\\b.txt");
  });

  it("没有 detail 的旧卡片不渲染正文区", () => {
    const a = approval({ locations: [] });
    const html = renderToStaticMarkup(<ApprovalCard approval={a} content="请求写入" />);
    expect(html).not.toContain("approval-detail");
    expect(html).toContain("本次允许");
  });

  it("空 detail 与 decided 状态不渲染正文区和按钮", () => {
    const a = approval({ detail: "", locations: [] }, { status: "decided", decision: "allow_once" });
    const html = renderToStaticMarkup(<ApprovalCard approval={a} content="请求操作" />);
    expect(html).not.toContain("approval-detail");
    expect(html).not.toContain("approval-actions");
    expect(html).toContain("已选择");
  });
});
