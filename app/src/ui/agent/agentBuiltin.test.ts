import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReportedOptions } from "../../types";

// 与 AgentAdd 相同的两个依赖都 mock 掉：选中「内置」后走服务端 models 接口，不碰本机 probeLocal
vi.mock("../../api", () => ({
  agents: {
    builtinModels: vi.fn(async () => [
      { id: "ep-1/deepseek-chat", name: "DeepSeek · deepseek-chat" },
      { id: "ep-1/deepseek-reasoner", name: "DeepSeek · deepseek-reasoner" },
    ]),
  },
}));
vi.mock("../../agentHost", () => ({
  probeLocal: vi.fn(async (): Promise<ReportedOptions> => ({
    models: [{ id: "local-m", name: "local-m" }],
    modes: [{ id: "local-mode", name: "local-mode" }],
  })),
}));

import { agents } from "../../api";
import { probeLocal } from "../../agentHost";
import { isBuiltin, readPickOptions } from "./agentBuiltin";
import { shapeAgentPatch } from "./agentForm";
import { programLine } from "./agentPage";
import { runtimeLabel } from "./registryEntry";

describe("Agent A28：内置运行时判定", () => {
  it("runtime {builtin:true} → 内置；命令 / 登记表 → 不是", () => {
    expect(isBuiltin({ builtin: true })).toBe(true);
    expect(isBuiltin({ builtin: false })).toBe(false);
    expect(isBuiltin({ command: "codex" })).toBe(false);
    expect(isBuiltin({ registry: { id: "x", version: "1" } })).toBe(false);
    expect(isBuiltin(undefined)).toBe(false);
  });

  it("programLine / runtimeLabel 显示「内置」", () => {
    expect(programLine({ builtin: true })).toBe("内置");
    expect(runtimeLabel({ builtin: true })).toBe("内置");
    // 不影响原有形状
    expect(programLine({ command: "codex" })).toBe("自定义命令 · codex");
    expect(runtimeLabel({ registry: { id: "x", version: "1.0" } })).toContain("登记表");
  });
});

describe("Agent A28：AgentAdd 选内置的可选项来源", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("选中内置 → 调 agents.builtinModels，不调 probeLocal", async () => {
    // 与 AgentAdd.probe() 相同的调用形状
    const res = await readPickOptions("builtin", {
      probe: () => probeLocal({ builtin: true }, ""),
      builtinModels: agents.builtinModels,
    });
    expect(agents.builtinModels).toHaveBeenCalledTimes(1);
    expect(probeLocal).not.toHaveBeenCalled();
    expect(res.models?.map((m) => m.id)).toEqual([
      "ep-1/deepseek-chat",
      "ep-1/deepseek-reasoner",
    ]);
    expect(res.modes ?? []).toEqual([]);
  });

  it("选中登记表 / 命令 → 调 probeLocal，不调 models 接口", async () => {
    const res = await readPickOptions("program", {
      probe: () => probeLocal({ command: "codex", args: [] }, ""),
      builtinModels: agents.builtinModels,
    });
    expect(probeLocal).toHaveBeenCalledTimes(1);
    expect(agents.builtinModels).not.toHaveBeenCalled();
    expect(res.models?.[0]?.id).toBe("local-m");
  });
});

describe("Agent A28：编辑内置 Agent 的保存", () => {
  const base = {
    name: "a",
    command: "",
    argsText: "",
    cwd: "",
    extraDirsText: "",
    model: "ep-1/m1",
    mode: "",
    instructions: "",
    envRows: [],
    outputMode: "",
    queueTimeout: "",
    permissionTier: "read_only",
  };

  it("builtin=true → runtime 提交 {builtin: true}，不看空命令", () => {
    const patch = shapeAgentPatch({ ...base, builtin: true });
    expect(patch.runtime).toEqual({ builtin: true });
  });

  it("不带 builtin → 仍是原形状", () => {
    expect(shapeAgentPatch({ ...base, command: "codex" }).runtime).toEqual({
      command: "codex",
      args: [],
    });
    expect(
      shapeAgentPatch({ ...base, registry: { id: "x", version: "1" } }).runtime,
    ).toEqual({ registry: { id: "x", version: "1" } });
  });
});
