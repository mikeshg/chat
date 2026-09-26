import { useEffect, useState } from "react";
import { convs } from "../api";
import type { MessageView, PinEntry } from "../types";

/** 置顶条摘要：有正文用第一行，否则用类型占位。 */
export function pinExcerpt(m: MessageView): string {
  if (m.deleted_at) return "消息已删除";
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
  readonly,
  reloadKey,
  onJump,
}: {
  convId: string;
  readonly: boolean;
  reloadKey: number;
  onJump: (id: string, seq: number) => void;
}) {
  const [pins, setPins] = useState<PinEntry[]>([]);
  const [open, setOpen] = useState(false);
  const [err, setErr] = useState("");

  // 换会话才收起面板；pins_updated 只刷新列表，避免正在看的面板被关掉
  useEffect(() => {
    setOpen(false);
    setErr("");
    setPins([]);
  }, [convId]);

  useEffect(() => {
    let alive = true;
    convs
      .pins(convId)
      .then((rows) => {
        if (alive) setPins(rows);
      })
      .catch(() => {
        if (alive) setPins([]);
      });
    return () => {
      alive = false;
    };
  }, [convId, reloadKey]);

  if (pins.length === 0) return null;
  const latest = pins[0];

  const unpin = async (messageId: string) => {
    setErr("");
    try {
      await convs.unpin(convId, messageId);
      setPins((rows) => rows.filter((p) => p.message.id !== messageId));
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
