import type { AgentOutputMode, PatchAgentReq } from "../types";

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
    runtime: { command: input.command.trim(), args: lines(input.argsText) },
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
