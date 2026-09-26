import { useEffect, useState } from "react";
import { agents, ApiError } from "../api";
import type { RegistryEntry } from "../types";
import { registryEntryUi } from "./registryEntry";

/** 添加 Agent：选定宿主后的登记表。安装进度从 WS 帧进来。 */
export default function RegistryPicker({
  hostId,
  selectedId,
  onSelect,
}: {
  hostId: string;
  selectedId: string;
  onSelect: (entry: RegistryEntry | null) => void;
}) {
  const [entries, setEntries] = useState<RegistryEntry[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [confirmId, setConfirmId] = useState("");
  const [progress, setProgress] = useState<{ id: string; text: string } | null>(null);

  const load = (id: string) => {
    if (!id) {
      setEntries([]);
      return;
    }
    setLoading(true);
    setError("");
    agents
      .registry(id)
      .then((res) => {
        setEntries(res.entries ?? []);
        setError(res.error ?? "");
        const still = (res.entries ?? []).find((e) => e.id === selectedId && e.status === "ready");
        if (!still && selectedId) onSelect(null);
      })
      .catch((e) => {
        setEntries([]);
        setError(e instanceof ApiError ? e.message : "登记表加载失败");
      })
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    setConfirmId("");
    setProgress(null);
    onSelect(null);
    load(hostId);
  }, [hostId]);

  useEffect(() => {
    const onProgress = (ev: Event) => {
      const f = (ev as CustomEvent).detail as {
        host_id?: string;
        entry_id?: string;
        stage?: string;
        error?: string;
      };
      if (!f || f.host_id !== hostId || !f.entry_id) return;
      const text = f.stage === "失败" ? `失败：${f.error || "安装失败"}` : f.stage || "";
      setProgress({ id: f.entry_id, text });
      if (f.stage === "完成") {
        setConfirmId("");
        agents
          .registry(hostId)
          .then((res) => {
            const list = res.entries ?? [];
            setEntries(list);
            setError(res.error ?? "");
            const done = list.find((e) => e.id === f.entry_id && e.status === "ready");
            if (done) onSelect(done);
          })
          .catch(() => {});
      }
    };
    window.addEventListener("chat:registry-progress", onProgress);
    return () => window.removeEventListener("chat:registry-progress", onProgress);
  }, [hostId, onSelect]);

  const install = async (entry: RegistryEntry) => {
    setError("");
    setProgress({ id: entry.id, text: "准备安装…" });
    try {
      await agents.installRegistry(hostId, entry.id, entry.version);
    } catch (e) {
      setProgress(null);
      setError(e instanceof ApiError ? e.message : "安装失败");
    }
  };

  if (!hostId) return <div className="muted">先选择一台宿主</div>;
  if (loading && entries.length === 0) return <div className="muted">正在读取登记表…</div>;

  return (
    <div className="registry-list" role="list" aria-label="登记表">
      {error && <div className="auth-err">{error}</div>}
      {entries.length === 0 && !error && <div className="muted">登记表是空的</div>}
      {entries.map((entry) => {
        const ui = registryEntryUi(entry.status, entry.reason);
        const selected = selectedId === entry.id;
        return (
          <div key={entry.id} className="registry-entry" role="listitem" aria-label={entry.name}>
            {entry.icon ? (
              <img className="registry-icon" src={entry.icon} alt="" />
            ) : (
              <span className="registry-icon registry-icon-fallback">◇</span>
            )}
            <div className="registry-body">
              <div className="registry-title">
                <label>
                  <input
                    type="radio"
                    name="registry-entry"
                    aria-label={`选择 ${entry.name}`}
                    disabled={!ui.selectable}
                    checked={selected}
                    onChange={() => ui.selectable && onSelect(entry)}
                  />{" "}
                  <b>{entry.name}</b>
                </label>
                <span className="muted">{entry.version}</span>
                <span className={`registry-status ${entry.status}`}>{ui.statusText}</span>
              </div>
              {entry.description && <div className="registry-desc">{entry.description}</div>}
              {ui.reasonLabel && <div className="registry-reason">{ui.reasonLabel}</div>}
              {progress?.id === entry.id && <div className="registry-progress">{progress.text}</div>}
              {ui.installLabel && confirmId !== entry.id && (
                <button type="button" onClick={() => setConfirmId(entry.id)}>
                  {ui.installLabel}
                </button>
              )}
              {confirmId === entry.id && (
                <div className="row">
                  <span className="muted">确认安装 {entry.name} {entry.version} 的 ACP 适配器？</span>
                  <button type="button" className="primary" onClick={() => void install(entry)}>
                    确认安装
                  </button>
                  <button type="button" onClick={() => setConfirmId("")}>
                    取消
                  </button>
                </div>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
