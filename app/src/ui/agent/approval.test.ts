import { describe, expect, it } from "vitest";
import { approvalKindLabel, optionButtonLabel, PERMISSION_TIERS, tierChoice } from "./approval";

describe("Agent A12：审批选项按钮文案", () => {
  it("三种标准 kind 用固定中文，其余用 agent 给的 name", () => {
    expect(optionButtonLabel("allow_once", "Allow once")).toBe("本次允许");
    expect(optionButtonLabel("allow_always", "Always")).toBe("总是允许");
    expect(optionButtonLabel("reject_once", "Reject")).toBe("拒绝");
    expect(optionButtonLabel("custom", "先看 diff")).toBe("先看 diff");
    expect(optionButtonLabel("weird", "  ")).toBe("weird");
  });
});

describe("Agent A12：权限档位说明", () => {
  it("三个档位都有名称和一句说明", () => {
    expect(PERMISSION_TIERS.map((t) => t.label)).toEqual(["只读", "可改工作目录", "完全放开"]);
    for (const t of PERMISSION_TIERS) {
      expect(t.hint.length).toBeGreaterThan(8);
    }
    expect(tierChoice("read_only").hint).toContain("读取");
    expect(tierChoice("workspace").hint).toContain("工作目录");
    expect(tierChoice("full").hint).toContain("自动放行");
    expect(tierChoice("nope").id).toBe("read_only");
    expect(tierChoice(undefined).label).toBe("只读");
  });

  it("工具类型有中文名", () => {
    expect(approvalKindLabel("edit")).toBe("写入");
    expect(approvalKindLabel("execute")).toBe("执行");
    expect(approvalKindLabel("switch_mode")).toBe("切换模式");
    expect(approvalKindLabel("custom-kind")).toBe("custom-kind");
  });
});
