import { describe, expect, it } from "vitest";
import { registryEntryUi, runtimeLabel } from "./registryEntry";

describe("registryEntryUi", () => {
  it("ready 可以选，没有安装按钮", () => {
    expect(registryEntryUi("ready")).toEqual({
      selectable: true,
      installLabel: null,
      reasonLabel: null,
      statusText: "可用",
    });
  });

  it("installable 显示安装，还不能选", () => {
    expect(registryEntryUi("installable")).toEqual({
      selectable: false,
      installLabel: "安装",
      reasonLabel: null,
      statusText: "可安装",
    });
  });

  it("unsupported 显示原因，不能选也不能装", () => {
    expect(registryEntryUi("unsupported", "需要先安装 Node.js")).toEqual({
      selectable: false,
      installLabel: null,
      reasonLabel: "需要先安装 Node.js",
      statusText: "不可用",
    });
  });
});

describe("runtimeLabel", () => {
  it("登记表显示名称和版本，自定义命令显示命令行", () => {
    expect(runtimeLabel({ registry: { id: "claude-code", version: "1.2.3" } }, "Claude Code")).toBe(
      "登记表：Claude Code 1.2.3",
    );
    expect(runtimeLabel({ registry: { id: "claude-code", version: "1.2.3" } })).toBe(
      "登记表：claude-code 1.2.3",
    );
    expect(runtimeLabel({ command: "node", args: ["agent.mjs"] })).toBe("自定义命令：node agent.mjs");
  });
});
