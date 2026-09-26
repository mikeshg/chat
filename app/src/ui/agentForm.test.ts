import { describe, expect, it } from "vitest";
import { shapeAgentPatch } from "./agentForm";

describe("shapeAgentPatch", () => {
  it("参数、额外目录按行切开并丢掉空行；环境变量丢掉空键", () => {
    const body = shapeAgentPatch({
      name: "  coder ",
      command: " node ",
      argsText: "a\n\n  b  \n",
      cwd: " /work ",
      extraDirsText: "/one\n\n /two \n",
      model: "smart",
      mode: "code",
      instructions: "先读 README",
      envRows: [
        { key: " FAKE_ECHO ", value: "sekrit" },
        { key: "  ", value: "drop" },
        { key: "EMPTY", value: "" },
      ],
      outputMode: "",
      queueTimeout: "",
    });
    expect(body.name).toBe("coder");
    expect(body.runtime).toEqual({ command: "node", args: ["a", "b"] });
    expect(body.cwd).toBe("/work");
    expect(body.extra_dirs).toEqual(["/one", "/two"]);
    expect(body.env).toEqual({ FAKE_ECHO: "sekrit", EMPTY: "" });
    expect(body.instructions).toBe("先读 README");
    expect(body.model).toBe("smart");
    expect(body.mode).toBe("code");
    // 空串 = 跟随默认，必须是 null 而不是 0（0 表示永不超时）
    expect(body.output_mode).toBeNull();
    expect(body.queue_timeout_secs).toBeNull();
  });

  it("选了覆盖项就原样放进请求体", () => {
    const body = shapeAgentPatch({
      name: "a",
      command: "node",
      argsText: "",
      cwd: "",
      extraDirsText: "",
      model: "",
      mode: "",
      instructions: "",
      envRows: [],
      outputMode: "steps",
      queueTimeout: "0",
    });
    expect(body.output_mode).toBe("steps");
    expect(body.queue_timeout_secs).toBe(0);
    expect(body.runtime).toEqual({ command: "node", args: [] });
    expect(body.extra_dirs).toEqual([]);
    expect(body.env).toEqual({});
  });
});
