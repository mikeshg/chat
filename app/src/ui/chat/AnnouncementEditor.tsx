import { useState } from "react";
import { announcementError } from "./AnnouncementBar";

/** 群公告编辑：保存 / 清空 / 取消。群信息面板与公告条共用。 */
export default function AnnouncementEditor({
  initial,
  onSave,
  onCancel,
}: {
  initial: string;
  onSave: (text: string | null) => Promise<void>;
  onCancel: () => void;
}) {
  const [text, setText] = useState(initial);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const submit = async (value: string | null) => {
    if (busy) return;
    setBusy(true);
    setErr("");
    try {
      await onSave(value);
    } catch (e) {
      setErr(announcementError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="ann-editor">
      <textarea
        rows={4}
        maxLength={2000}
        value={text}
        disabled={busy}
        placeholder="公告内容，1–2000 字"
        onChange={(e) => setText(e.target.value)}
      />
      {err && <div className="mut-err">{err}</div>}
      <div className="row">
        <button
          type="button"
          className="primary"
          disabled={busy || !text.trim()}
          onClick={() => void submit(text.trim())}
        >
          {busy ? "保存中…" : "保存"}
        </button>
        <button type="button" disabled={busy} onClick={() => void submit(null)}>
          清空
        </button>
        <button type="button" disabled={busy} onClick={onCancel}>
          取消
        </button>
      </div>
    </div>
  );
}
