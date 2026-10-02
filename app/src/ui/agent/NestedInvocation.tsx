// Agent A15：原会话里，父调用下面的子调用卡片。默认收起，最多嵌套 5 层。

import { useState, type ReactNode } from "react";
import type { AgentRuns } from "../../agentRun";
import { formatResultPreview, nestUnder, type NestNode } from "../../invocationTree";
import { invocationErrorText, useApp } from "../../store";
import type { InvocationView } from "../../types";
import { InvocationStatusLine, LiveRunBubble } from "./AgentRun";

function nestReason(inv: InvocationView): string | null {
  if (inv.status === "failed") return `失败：${invocationErrorText(inv.error)}`;
  if (inv.status === "cancelled") {
    return inv.error ? `已取消：${invocationErrorText(inv.error)}` : "已取消";
  }
  if (inv.status === "timed_out") return "排队超时，已取消";
  return null;
}

/** 一张子调用卡片。open 由外层管，方便单测展开后的正文。 */
export function NestCard({
  node,
  open,
  onToggle,
  live,
  depth = 1,
  children,
}: {
  node: NestNode;
  open: boolean;
  onToggle: () => void;
  live: { text: string; steps: { id: string; title: string; kind: string; ok: boolean | null }[] } | null;
  depth?: number;
  children?: ReactNode;
}) {
  const name = node.inv.agent_name || "Agent";
  const preview = formatResultPreview(node.inv.result_preview);
  const reason = nestReason(node.inv);
  const showLive = open && !!live && node.inv.finished_at == null && node.inv.status === "running";
  return (
    <div className="nest-card" data-depth={depth}>
      <button type="button" className="nest-toggle" aria-expanded={open} onClick={onToggle}>
        <span className="nest-affix">{`↳ 🤖 ${name} · `}</span>
        {node.inv.status === "succeeded" ? (
          <span className="inv-status succeeded nest-status">完成</span>
        ) : (
          <InvocationStatusLine inv={node.inv} textOnly />
        )}
      </button>
      {node.deeper && <div className="nest-deeper">还有更深的调用</div>}
      {open && (
        <div className="nest-body">
          {showLive && live && (
            <LiveRunBubble agentName={name} run={live} emptyHint="正在思考…" />
          )}
          {node.inv.status === "succeeded" && preview.text && (
            <div className="nest-result">
              {preview.text}
              {preview.truncated ? "…" : ""}
            </div>
          )}
          {reason && <div className="nest-reason">{reason}</div>}
          {children}
        </div>
      )}
    </div>
  );
}

function NestCardLive({
  node,
  depth,
  runs,
}: {
  node: NestNode;
  depth: number;
  runs: AgentRuns;
}) {
  const [open, setOpen] = useState(false);
  const live = runs[node.inv.conversation_id]?.[node.inv.id] ?? null;
  return (
    <NestCard
      node={node}
      depth={depth}
      open={open}
      onToggle={() => setOpen((v) => !v)}
      live={live}
    >
      {node.children.map((child) => (
        <NestCardLive key={child.inv.id} node={child} depth={depth + 1} runs={runs} />
      ))}
    </NestCard>
  );
}

/** 挂在父调用状态行下面。取消按钮不在这里（取消父调用会级联）。 */
export function NestedInvocations({ parentId }: { parentId: string }) {
  const { childInvocations, agentRuns } = useApp();
  const nodes = nestUnder(childInvocations, parentId);
  if (nodes.length === 0) return null;
  return (
    <div className="nest-list">
      {nodes.map((n) => (
        <NestCardLive key={n.inv.id} node={n} depth={1} runs={agentRuns} />
      ))}
    </div>
  );
}
