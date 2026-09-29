import { useEffect, useState } from "react";
import { agents as agentsApi } from "../api";
import type { DirListing } from "../types";
import { agentErrText } from "./agentPage";

/** A27：浏览那台电脑上的目录（经它的宿主连接列出），点进子目录，选定当前目录。 */
export default function DirPicker({
  hostId,
  onPick,
  onCancel,
}: {
  hostId: string;
  onPick: (path: string) => void;
  onCancel: () => void;
}) {
  const [path, setPath] = useState("");
  const [data, setData] = useState<DirListing | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    setBusy(true);
    setErr("");
    agentsApi
      .dirs(hostId, path)
      .then((d) => {
        if (!alive) return;
        setData(d);
        if (d.error) setErr(d.error);
      })
      .catch((e) => alive && setErr(agentErrText(e)))
      .finally(() => alive && setBusy(false));
    return () => {
      alive = false;
    };
  }, [hostId, path]);

  return (
    <div className="dir-picker">
      <div className="dir-picker-head">
        <button type="button" disabled={busy || path === ""} onClick={() => setPath(data?.parent ?? "")}>
          ↑ 上一级
        </button>
        <span className="dir-picker-path" title={data?.path || ""}>
          {data?.path || "选一个起点"}
        </span>
      </div>
      <div className="dir-picker-list">
        {busy && <div className="am-hint am-pad">读取中…</div>}
        {!busy && err && <div className="auth-err am-pad">{err}</div>}
        {!busy && !err && data?.dirs.length === 0 && <div className="am-hint am-pad">没有子目录</div>}
        {!busy &&
          data?.dirs.map((d) => (
            <button type="button" key={d.path} onClick={() => setPath(d.path)}>
              📁 {d.name}
            </button>
          ))}
      </div>
      <div className="am-row end">
        <button type="button" onClick={onCancel}>
          取消
        </button>
        <button
          type="button"
          className="primary"
          disabled={busy || !data?.path || !!err}
          onClick={() => data?.path && onPick(data.path)}
        >
          选这个目录
        </button>
      </div>
    </div>
  );
}
