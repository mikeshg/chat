// ===== V2 P4-1 =====
// 管理后台：备份/恢复。备份 = 库 + FILES_DIR 全量 → tar.gz 可下载；
// 恢复 = 上传 tar.gz + 键入 RESTORE 确认 → 服务端整库回灌（所有人被登出）。
import { useCallback, useEffect, useRef, useState } from "react";
import { admin, clearSession, getServer } from "../../api";
import { stopWs } from "../../ws";
import { fmtBytes } from "../mediaFormat";
import type { BackupItem } from "../../types";

/** 恢复确认词：必须与服务端 confirm 语义一致 —— 精确 RESTORE，不 trim */
export function restoreConfirmed(input: string): boolean {
  return input === "RESTORE";
}

function fmtTime(s: string) {
  const t = Date.parse(s);
  return Number.isNaN(t) ? s : new Date(t).toLocaleString();
}

export default function BackupPanel() {
  const [items, setItems] = useState<BackupItem[]>([]);
  const [inProgress, setInProgress] = useState(false);
  const [busy, setBusy] = useState<string | null>(null); // backup | restore | <name>
  const [err, setErr] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [confirmText, setConfirmText] = useState("");
  const [pct, setPct] = useState<number | null>(null);
  const fileRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    try {
      const l = await admin.backups();
      setItems(l.items);
      setInProgress(l.in_progress);
      setErr("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : "加载失败");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 生成中约每 2s 轮询，完成后自动停（in_progress 翻 false 即最后一次已刷新）
  useEffect(() => {
    if (!inProgress) return;
    const t = window.setInterval(() => void load(), 2000);
    return () => window.clearInterval(t);
  }, [inProgress, load]);

  const doBackup = async () => {
    setBusy("backup");
    setErr("");
    try {
      await admin.createBackup();
      setInProgress(true);
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "备份失败");
    } finally {
      setBusy(null);
    }
  };

  const doDownload = async (name: string) => {
    setBusy(name);
    setErr("");
    try {
      const { url } = await admin.backupDownloadTicket(name);
      const a = document.createElement("a");
      a.href = `${getServer()}${url}`;
      a.download = name;
      document.body.appendChild(a);
      a.click();
      a.remove();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "下载失败");
    } finally {
      setBusy(null);
    }
  };

  const doDelete = async (name: string) => {
    if (!window.confirm(`删除备份 ${name}？`)) return;
    setBusy(name);
    setErr("");
    try {
      await admin.deleteBackup(name);
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "删除失败");
    } finally {
      setBusy(null);
    }
  };

  const doRestore = async () => {
    if (!file || !restoreConfirmed(confirmText) || busy) return;
    setBusy("restore");
    setPct(0);
    setErr("");
    try {
      await admin.restore(file, setPct);
      // 服务端已吊销全部凭据（含自己）：走 force_logout 同款出口回登录页
      sessionStorage.setItem("chat_kick_reason", "恢复完成，请重新登录");
      stopWs();
      clearSession();
      window.dispatchEvent(new Event("chat:logout"));
    } catch (e) {
      setErr(e instanceof Error ? e.message : "恢复失败");
      setBusy(null);
      setPct(null);
    }
  };

  return (
    <div className="card bk-panel">
      <h3>数据备份 / 恢复</h3>
      {err && <div className="auth-err">{err}</div>}

      <div className="row bk-toolbar">
        <button
          className="primary"
          disabled={busy !== null || inProgress}
          onClick={() => void doBackup()}
        >
          立即备份
        </button>
        {(inProgress || busy === "backup") && (
          <span className="muted">正在生成备份…</span>
        )}
      </div>

      <div className="bk-list">
        {items.map((b) => (
          <div key={b.name} className="bk-row">
            <span className="bk-name" title={b.name}>
              {b.name}
            </span>
            <span className="muted">{fmtBytes(b.size)}</span>
            <span className="muted">{fmtTime(b.created_at)}</span>
            <span className="bk-actions">
              <button
                disabled={busy !== null}
                onClick={() => void doDownload(b.name)}
              >
                下载
              </button>
              <button
                className="danger"
                disabled={busy !== null}
                onClick={() => void doDelete(b.name)}
              >
                删除
              </button>
            </span>
          </div>
        ))}
        {items.length === 0 && <div className="muted">还没有备份</div>}
      </div>

      <div className="bk-restore">
        <h4>从备份恢复</h4>
        <p className="bk-warn">
          恢复会用备份覆盖全部数据和文件，所有用户（包括你）都会被登出
        </p>
        <div className="row">
          <input
            ref={fileRef}
            type="file"
            accept=".tar.gz,application/gzip"
            disabled={busy !== null}
            onChange={(e) => setFile(e.target.files?.[0] ?? null)}
          />
        </div>
        <div className="row bk-confirm">
          <input
            placeholder="输入 RESTORE 确认"
            value={confirmText}
            disabled={busy !== null}
            onChange={(e) => setConfirmText(e.target.value)}
          />
          <button
            className="danger"
            disabled={!file || !restoreConfirmed(confirmText) || busy !== null}
            onClick={() => void doRestore()}
          >
            {busy === "restore" && pct !== null
              ? `恢复中 ${pct}%`
              : "确认恢复"}
          </button>
        </div>
        {busy === "restore" && pct !== null && (
          <div className="bk-progress">
            <div className="bk-progress-bar" style={{ width: `${pct}%` }} />
          </div>
        )}
      </div>
    </div>
  );
}
