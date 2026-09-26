import type { InvocationView } from "../types";
import { invocationCancellable, invocationStatusLabel } from "../store";

/** 触发消息下方的调用状态行。文案按 status 查表（A03 会把这张表收进组件）。 */
export default function InvocationStatus({
  inv,
  allowCancel,
  onCancel,
}: {
  inv: InvocationView;
  allowCancel: boolean;
  onCancel: (id: string) => void;
}) {
  const label = invocationStatusLabel(inv.status, inv.error);
  const showCancel = allowCancel && invocationCancellable(inv.status);
  if (!label && !showCancel) return null;
  return (
    <div className={`inv-status ${inv.status}`}>
      {label && <span>{label}</span>}
      {showCancel && (
        <button type="button" className="inv-cancel" onClick={() => onCancel(inv.id)}>
          取消
        </button>
      )}
    </div>
  );
}
