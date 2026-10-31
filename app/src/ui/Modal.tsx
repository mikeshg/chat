import { useEffect, useSyncExternalStore } from "react";

/**
 * 应用内 Modal（GLOSSARY: In-app Modal）——替代 window.confirm/alert。
 * 用法：App 根部挂一次 <DialogHost />；任何地方 await confirmDialog/alertDialog。
 */

export interface DialogOptions {
  /** 标题；默认 confirm=「请确认」、alert=「提示」 */
  title?: string;
  /** 主按钮文案，默认「确定」 */
  okText?: string;
  /** 副按钮文案，默认「取消」（alert 无副按钮） */
  cancelText?: string;
  /** 危险操作（删除/解散/退出）：主按钮红色填充 */
  danger?: boolean;
}

export interface PendingDialog {
  kind: "confirm" | "alert";
  message: string;
  opts: DialogOptions;
  resolve: (ok: boolean) => void;
}

let current: PendingDialog | null = null;
const queue: PendingDialog[] = [];
const listeners = new Set<() => void>();

function emit() {
  listeners.forEach((l) => l());
}

function pump() {
  if (current || queue.length === 0) return;
  current = queue.shift()!;
}

function enqueue(kind: PendingDialog["kind"], message: string, opts: DialogOptions) {
  return new Promise<boolean>((resolve) => {
    queue.push({ kind, message, opts, resolve });
    const had = current;
    pump();
    if (current !== had) emit();
  });
}

/** 确认框：确定 → true；取消/Esc/点遮罩 → false */
export function confirmDialog(message: string, opts: DialogOptions = {}) {
  return enqueue("confirm", message, opts);
}

/** 提示框：仅「确定」，resolve 即关 */
export function alertDialog(message: string, opts: DialogOptions = {}) {
  return enqueue("alert", message, opts).then(() => {});
}

/** 供 DialogHost 订阅；测试也可读 getDialogSnapshot 断言当前请求 */
export function subscribeDialogs(l: () => void) {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function getDialogSnapshot(): PendingDialog | null {
  return current;
}

export function resolveDialog(ok: boolean) {
  const c = current;
  if (!c) return;
  current = null;
  c.resolve(ok);
  pump();
  emit();
}

/** 全部按取消关闭（登出/会话切换时兜底；测试 afterEach 也用它排空） */
export function dismissAllDialogs() {
  while (current) resolveDialog(false);
  while (queue.length) queue.shift()!.resolve(false);
}

export function DialogHost() {
  const d = useSyncExternalStore(subscribeDialogs, getDialogSnapshot, () => null);

  useEffect(() => {
    if (!d) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") resolveDialog(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [d]);

  if (!d) return null;
  return (
    <div className="dlg-mask" data-testid="modal-mask" onClick={() => resolveDialog(false)}>
      <div
        className="dlg"
        role="dialog"
        aria-modal="true"
        data-testid="modal"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dlg-title" data-testid="modal-title">
          {d.opts.title ?? (d.kind === "confirm" ? "请确认" : "提示")}
        </div>
        <div className="dlg-msg" data-testid="modal-message">
          {d.message}
        </div>
        <div className="dlg-actions">
          {d.kind === "confirm" && (
            <button data-testid="modal-cancel" onClick={() => resolveDialog(false)}>
              {d.opts.cancelText ?? "取消"}
            </button>
          )}
          <button
            className={d.opts.danger ? "danger-fill" : "primary"}
            data-testid="modal-ok"
            autoFocus
            onClick={() => resolveDialog(true)}
          >
            {d.opts.okText ?? "确定"}
          </button>
        </div>
      </div>
    </div>
  );
}
