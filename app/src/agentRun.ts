// Agent A03：agent_stream 帧 → 临时渲染状态的纯函数。
//
// LiveRun 是一次进行中调用的「影子气泡」：不落库、不跨设备，
// 在调用进终态时由 store.applyInvocations 摘除（终态消息已含全部内容）。

/** 进行中步骤（ok=null）或刚收完尾的步骤（ok=true/false） */
export interface LiveStep {
  id: string;
  title: string;
  kind: string;
  ok: boolean | null;
}

/** 一次进行中调用的临时输出 */
export interface LiveRun {
  text: string;
  steps: LiveStep[];
}

/** conversation_id -> invocation_id -> 临时输出 */
export type AgentRuns = Record<string, Record<string, LiveRun>>;

/** WS agent_stream 帧形状（types.ts 的 WsOut 联合里同名成员的轻量版） */
export interface AgentStreamFrame {
  invocation_id: string;
  conversation_id: string;
  delta?: string;
  step?: LiveStep;
}

/** delta 追加、step 按 id upsert；无可应用内容时返回入参本身（引用相等 = 无变化） */
export function applyStreamFrame(
  convRuns: Record<string, LiveRun> | undefined,
  f: AgentStreamFrame,
): Record<string, LiveRun> {
  const cur = convRuns?.[f.invocation_id] ?? { text: "", steps: [] };
  let text = cur.text;
  let steps = cur.steps;
  if (f.delta) {
    text += f.delta;
  }
  if (f.step) {
    const i = steps.findIndex((s) => s.id === f.step!.id);
    steps =
      i < 0
        ? [...steps, f.step]
        : steps.map((s, j) => (j === i ? { ...s, ...f.step! } : s));
  }
  if (text === cur.text && steps === cur.steps) {
    return convRuns ?? {};
  }
  return { ...(convRuns ?? {}), [f.invocation_id]: { text, steps } };
}

/** 调用进终态：摘除其临时输出。无该调用时返回入参（引用相等 = 无变化）。 */
export function dropRun(
  convRuns: Record<string, LiveRun> | undefined,
  invId: string,
): Record<string, LiveRun> | undefined {
  if (!convRuns || !(invId in convRuns)) {
    return convRuns;
  }
  const next = { ...convRuns };
  delete next[invId];
  return next;
}

/** 秒 → m:ss（调用状态行的已用时长） */
export function fmtElapsed(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}
