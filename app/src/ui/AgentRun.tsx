// Agent A03：调用状态标签 + 进行中的流式气泡 + 步骤记录（折叠）。

import { useEffect, useReducer, useState } from "react";
import type { LiveStep } from "../agentRun";
import { invocationStatusView } from "../store";
import type { AgentStep, InvocationView } from "../types";

/** 步骤项：持久化记录 ok:boolean，进行中步骤 ok:null */
type AnyStep = Omit<AgentStep, "ok"> & { ok: boolean | null };

/** 状态标签行：按 status 查表渲染图标与文字；带 current_step 时每秒刷新已用时长。 */
export function InvocationStatusLine({ inv }: { inv: InvocationView }) {
  const [, tick] = useReducer((n: number) => n + 1, 0);
  const ticking = inv.status === "running" && inv.current_step != null;
  useEffect(() => {
    if (!ticking) return;
    const t = window.setInterval(() => tick(), 1000);
    return () => window.clearInterval(t);
  }, [ticking]);
  const elapsed = inv.started_at
    ? Math.max(0, (Date.now() - Date.parse(inv.started_at)) / 1000)
    : 0;
  const v = invocationStatusView(inv, elapsed);
  if (!v) return null;
  return (
    <div className={`inv-status ${inv.status}`}>
      {v.icon} {v.text}
    </div>
  );
}

/** 步骤图标：进行中 ◌ / 完成 ✓ / 失败 ✗ */
function stepIcon(ok: boolean | null): string {
  return ok === null ? "◌" : ok ? "✓" : "✗";
}

/** 可折叠步骤列表（默认收起，摘要「· N 个步骤 ·」；服务进行中与已定稿两种步骤）。 */
export function StepRecord({ steps }: { steps: AnyStep[] }) {
  const [open, setOpen] = useState(false);
  const failed = steps.filter((s) => s.ok === false).length;
  return (
    <div className="step-record">
      <button
        type="button"
        className="step-summary"
        onClick={() => setOpen((o) => !o)}
      >
        · {steps.length} 个步骤{failed > 0 ? ` · ${failed} 失败` : ""} ·{" "}
        {open ? "收起" : "展开"}
      </button>
      {open && (
        <ul className="step-list">
          {steps.map((s) => (
            <li key={s.id} className={s.ok === false ? "failed" : ""}>
              {stepIcon(s.ok)} {s.title}
              <span className="step-kind">{s.kind}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** stream 模式的「生成中」气泡：触发消息下方，步骤折叠在上、正文实时增长。 */
export function LiveRunBubble({
  agentName,
  run,
  emptyHint,
}: {
  agentName: string;
  run: { text: string; steps: LiveStep[] };
  emptyHint: string;
}) {
  return (
    <div className="agent-live">
      <div className="agent-live-head">@{agentName} 生成中</div>
      {run.steps.length > 0 && <StepRecord steps={run.steps} />}
      {run.text ? (
        <div className="agent-live-text">{run.text}</div>
      ) : (
        run.steps.length === 0 && <div className="agent-live-empty">{emptyHint}</div>
      )}
    </div>
  );
}
