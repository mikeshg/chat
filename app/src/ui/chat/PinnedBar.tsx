import { useEffect, useState } from "react";
import type { MessageView, PinEntry } from "../../types";
import { RECORD_TITLE_MAX, recordLabel } from "./chatRecord";

/** 置顶条摘要：有正文用第一行，否则用类型占位。 */
export function pinExcerpt(m: MessageView): string {
  if (m.deleted_at) return "消息已删除";
  // MF2：聊天记录恒显示「[聊天记录] {title}」（标题 >30 字截断）
  if (m.kind === "chat_record") return recordLabel(m, RECORD_TITLE_MAX);
  const line = m.content.split("\n", 1)[0].trim();
  if (line) return line;
  if (m.kind === "image") return "[图片]";
  if (m.kind === "audio") return "[语音]";
  if (m.kind === "file") return "[文件]";
  if (m.kind === "poll") return "[投票]";
  return "（无文字）";
}

export default function PinnedBar({
  convId,
  pins,
  readonly,
  onUnpin,
  onJump,
}: {
  convId: string;
  /** 列表由 ChatView 持有（菜单的置顶状态也从它派生） */
  pins: PinEntry[];
  readonly: boolean;
  onUnpin: (messageId: string) => Promise<void>;
  onJump: (id: string, seq: number) => void;
}) {
  const [open, setOpen] = useState(false);
  const [err, setErr] = useState("");

  // 换会话才收起面板；pins_updated 只刷新列表，避免正在看的面板被关掉
  useEffect(() => {
    setOpen(false);
    setErr("");
  }, [convId]);

  if (pins.length === 0) return null;
  const latest = pins[0];

  const unpin = async (messageId: string) => {
    setErr("");
    try {
      await onUnpin(messageId);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "取消置顶失败");
    }
  };

  return (
    <div className="pin-bar">
      <button type="button" className="pin-toggle" onClick={() => setOpen(!open)}>
        <span>📌 {pins.length} 条置顶</span>
        <span className="pin-excerpt">{pinExcerpt(latest.message)}</span>
      </button>
      {open && (
        <div className="pin-panel">
          {err && <div className="mut-err">{err}</div>}
          {pins.map((p) => (
            <div className="pin-row" key={p.message.id}>
              <span className="pin-excerpt">{pinExcerpt(p.message)}</span>
              <button type="button" onClick={() => onJump(p.message.id, p.message.seq)}>
                跳转
              </button>
              {!readonly && (
                <button type="button" onClick={() => void unpin(p.message.id)}>
                  取消置顶
                </button>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
