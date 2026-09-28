import { useEffect, useRef, useState, type ReactNode } from "react";
import { agents, ApiError } from "../api";
import type { RegistryEntry } from "../types";
import { filterPrograms } from "./agentPage";
import { registryEntryUi } from "./registryEntry";

/** 添加 Agent：本机登记表卡片。安装走原来的接口，进度来自 WS 帧。 */
export default function RegistryPicker({
  hostId,
  query,
  selectedId,
  onSelect,
  children,
}: {
  hostId: string;
  query: string;
  selectedId: string;
  onSelect: (entry: RegistryEntry) => void;
  children?: ReactNode;
}) {
  const [entries, setEntries] = useState<RegistryEntry[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [progress, setProgress] = useState<{ id: string; text: string } | null>(null);
  const onSelectRef = useRef(onSelect);
  onSelectRef.current = onSelect;

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
      })
      .catch((e) => {
        setEntries([]);
        setError(e instanceof ApiError ? e.message : "登记表加载失败");
      })
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    setProgress(null);
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
        agents
          .registry(hostId)
          .then((res) => {
            const list = res.entries ?? [];
            setEntries(list);
            setError(res.error ?? "");
            const done = list.find((e) => e.id === f.entry_id && e.status === "ready");
            if (done) onSelectRef.current(done);
            setProgress(null);
          })
          .catch(() => {});
      }
    };
    window.addEventListener("chat:registry-progress", onProgress);
    return () => window.removeEventListener("chat:registry-progress", onProgress);
  }, [hostId]);

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

  const shown = filterPrograms(entries, query);
  const selected = entries.find((e) => e.id === selectedId) ?? null;
  const prog = progress && selected && progress.id === selected.id ? progress : null;
  const installing = !!prog && !prog.text.startsWith("失败");

  if (!hostId) return <div className="am-hint">这台电脑还没就绪</div>;

  return (
    <>
      {loading && entries.length === 0 && <div className="am-hint">正在读取登记表…</div>}
      {error && <div className="auth-err">{error}</div>}
      <div className="am-cards">
        {shown.length === 0 && query.trim() && !loading && (
          <div className="am-hint am-span">
            登记表里没有「{query.trim()}」。可以用下面的自定义命令。
          </div>
        )}
        {!loading && entries.length === 0 && !error && !query.trim() && (
          <div className="am-hint am-span">登记表是空的。可以用下面的自定义命令。</div>
        )}
        {shown.map((entry) => {
          const ui = registryEntryUi(entry.status, entry.reason);
          const pickable = entry.status === "ready" || entry.status === "installable";
          const on = selectedId === entry.id;
          const st = entry.status === "ready" ? "ok" : entry.status === "installable" ? "need" : "no";
          const desc =
            entry.status === "ready" || entry.status === "installable"
              ? entry.description
              : ui.reasonLabel || entry.description;
          return (
            <button
              key={entry.id}
              type="button"
              className={`am-pick${on ? " on" : ""}${pickable ? "" : " dis"}`}
              aria-disabled={!pickable}
              onClick={() => pickable && onSelectRef.current(entry)}
            >
              <b>{entry.name}</b>
              <span className={`am-st ${st}`}>
                {ui.statusText}
                {pickable ? ` · ${entry.version}` : ""}
              </span>
              {desc ? <span className="am-ds">{desc}</span> : null}
            </button>
          );
        })}
        {children}
      </div>
      {selected?.status === "installable" && (
        <div className="am-note">
          {registryEntryUi(selected.status, selected.reason).reasonLabel}
          <div className="am-note-act">
            {prog && <span>{installing ? `正在安装… ${prog.text}` : prog.text}</span>}
            {!installing && (
              <button type="button" className="primary" onClick={() => void install(selected)}>
                安装
              </button>
            )}
          </div>
        </div>
      )}
    </>
  );
}
