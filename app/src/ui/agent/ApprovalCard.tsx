import { useState } from "react";
import { agents, ApiError } from "../../api";
import type { ApprovalOption, ApprovalView } from "../../types";
import { approvalKindLabel, optionButtonLabel } from "./approval";

/** Agent 专属会话里的审批卡片。按钮只对主人有用，别人点了服务端回 404。 */
export default function ApprovalCard({
  approval,
  content,
}: {
  approval: ApprovalView;
  content: string;
}) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const req = approval.request ?? {};
  const locations = Array.isArray(req.locations) ? req.locations.filter(Boolean) : [];
  const detail = typeof req.detail === "string" ? req.detail : "";
  const options = Array.isArray(approval.options) ? approval.options : [];
  const picked = options.find((o) => o.optionId === approval.decision);

  const onDecide = async (optionId: string) => {
    if (busy || approval.status !== "pending") return;
    setBusy(true);
    setErr("");
    try {
      await agents.decideApproval(approval.id, optionId);
    } catch (e) {
      const msg =
        e instanceof ApiError && e.status === 409
          ? "这个审批已经处理过了"
          : e instanceof ApiError && e.status === 400
            ? "这个选项无效"
            : e instanceof Error
              ? e.message
              : "决定失败";
      setErr(msg);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="approval-card" data-testid="approval-card">
      <div className="approval-title">{content}</div>
      <div className="approval-meta">
        类型：{approvalKindLabel(req.kind)}
        {locations.length > 0 && <span> · 路径：{locations.join("、")}</span>}
      </div>
      {/* A20：detail 是批准后实际发出的完整正文，必须全文可见；长文可折叠但不能丢内容 */}
      {detail && (
        <details className="approval-detail" open>
          <summary>完整正文</summary>
          <pre>{detail}</pre>
        </details>
      )}
      {approval.status === "pending" && (
        <div className="approval-actions">
          {options.map((o) => (
            <ApprovalButton key={o.optionId} option={o} busy={busy} onDecide={onDecide} />
          ))}
        </div>
      )}
      {approval.status === "decided" && (
        <div className="approval-result">
          已选择：{picked ? optionButtonLabel(picked.kind, picked.name) : approval.decision || "已决定"}
        </div>
      )}
      {approval.status === "expired" && <div className="approval-result">已失效</div>}
      {err && <div className="auth-err">{err}</div>}
    </div>
  );
}

function ApprovalButton({
  option,
  busy,
  onDecide,
}: {
  option: ApprovalOption;
  busy: boolean;
  onDecide: (id: string) => void;
}) {
  const label = optionButtonLabel(option.kind, option.name);
  return (
    <button type="button" disabled={busy} onClick={() => onDecide(option.optionId)}>
      {label}
    </button>
  );
}
