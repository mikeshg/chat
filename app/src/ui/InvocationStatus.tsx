import type { InvocationView } from "../types";
import { invocationCancellable } from "../store";
import { InvocationStatusLine } from "./AgentRun";

/** 触发消息下方的调用状态行：文案（A03 查表，含已用时长 / 当前步骤）+ 主人可见的取消按钮（A04）。 */
export default function InvocationStatus({
  inv,
  allowCancel,
  onCancel,
  onOpenApproval,
}: {
  inv: InvocationView;
  allowCancel: boolean;
  onCancel: (id: string) => void;
  /** 专属会话里跳到审批卡片。群里不传，只显示状态文字。 */
  onOpenApproval?: () => void;
}) {
  const showCancel = allowCancel && invocationCancellable(inv.status);
  return (
    <div className="inv-status-row">
      <InvocationStatusLine inv={inv} />
      {onOpenApproval && inv.status === "awaiting_approval" && (
        <button type="button" className="inv-approval-jump" onClick={onOpenApproval}>
          去审批
        </button>
      )}
      {showCancel && (
        <button type="button" className="inv-cancel" onClick={() => onCancel(inv.id)}>
          取消
        </button>
      )}
    </div>
  );
}
