import type { AgentOutputMode, PatchAgentReq } from "../../types";

export interface EnvRow {
  key: string;
  value: string;
}

/** 编辑表单的原始输入。outputMode / queueTimeout 空串 = 跟随个人默认。 */
export interface AgentFormInput {
  name: string;
  command: string;
  /** 每行一个参数 */
  argsText: string;
  cwd: string;
  /** 每行一个额外目录 */
  extraDirsText: string;
  model: string;
  mode: string;
  instructions: string;
  envRows: EnvRow[];
  outputMode: string;
  queueTimeout: string;
  /** 有它就按登记表形状提交，不再带自定义命令 */
  registry?: { id: string; version: string };
  /** A28：内置 Agent 提交 {"builtin": true}，优先于 registry/command */
  builtin?: boolean;
  /** read_only / workspace / full */
  permissionTier: string;
}

function lines(text: string): string[] {
  return text
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
}

/** 把编辑表单收成 PATCH 请求体。空环境变量键丢弃；覆盖项空串变成 null。 */
export function shapeAgentPatch(input: AgentFormInput): PatchAgentReq {
  const env: Record<string, string> = {};
  for (const row of input.envRows) {
    const key = row.key.trim();
    if (!key) continue;
    env[key] = row.value;
  }
  return {
    name: input.name.trim(),
    runtime: input.builtin
      ? { builtin: true }
      : input.registry
        ? { registry: { id: input.registry.id, version: input.registry.version } }
        : { command: input.command.trim(), args: lines(input.argsText) },
    cwd: input.cwd.trim(),
    extra_dirs: lines(input.extraDirsText),
    model: input.model,
    mode: input.mode,
    instructions: input.instructions,
    env,
    output_mode: input.outputMode === "" ? null : (input.outputMode as AgentOutputMode),
    queue_timeout_secs: input.queueTimeout === "" ? null : Number(input.queueTimeout),
    permission_tier: input.permissionTier || "read_only",
  };
}
