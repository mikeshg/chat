import { useEffect, useState } from "react";
import { agents } from "../api";
import type { AgentOutputMode, AgentUserSettings } from "../types";

export const OUTPUT_MODE_OPTIONS: { v: AgentOutputMode; label: string }[] = [
  { v: "stream", label: "实时流式" },
  { v: "status", label: "状态 + 最终结果" },
  { v: "steps", label: "每步一条消息" },
];

/** 排队超时选项（秒），0 = 永不 */
export const QUEUE_TIMEOUT_OPTIONS: { v: number; label: string }[] = [
  { v: 600, label: "10 分钟" },
  { v: 3600, label: "1 小时" },
  { v: 6 * 3600, label: "6 小时" },
  { v: 86400, label: "1 天" },
  { v: 7 * 86400, label: "7 天" },
  { v: 0, label: "永不" },
];

// Agent 个人默认设置：输出方式 + 排队超时 + 互调深度。单个 Agent 可覆盖前两项。
export default function AgentSettings() {
  const [s, setS] = useState<AgentUserSettings | null>(null);
  const [err, setErr] = useState("");

  useEffect(() => {
    let alive = true;
    agents
      .settings()
      .then((v) => alive && setS(v))
      .catch(() => alive && setErr("读取设置失败"));
    return () => {
      alive = false;
    };
  }, []);

  const save = async (patch: Partial<AgentUserSettings>) => {
    setErr("");
    try {
      setS(await agents.updateSettings(patch));
    } catch {
      setErr("保存失败");
    }
  };

  if (!s) return err ? <div className="muted">{err}</div> : null;
  const timeoutKnown = QUEUE_TIMEOUT_OPTIONS.some((o) => o.v === s.queue_timeout_secs);
  return (
    <section>
      <h4 className="agent-sec">默认设置（单个 Agent 可覆盖）</h4>
      <label className="row agent-item">
        <span className="grow">输出方式</span>
        <select
          value={s.output_mode}
          onChange={(e) => void save({ output_mode: e.target.value as AgentOutputMode })}
        >
          {OUTPUT_MODE_OPTIONS.map((o) => (
            <option key={o.v} value={o.v}>
              {o.label}
            </option>
          ))}
        </select>
      </label>
      <label className="row agent-item">
        <span className="grow">宿主离线时排队多久后取消</span>
        <select
          value={s.queue_timeout_secs}
          onChange={(e) => void save({ queue_timeout_secs: Number(e.target.value) })}
        >
          {!timeoutKnown && <option value={s.queue_timeout_secs}>{s.queue_timeout_secs} 秒</option>}
          {QUEUE_TIMEOUT_OPTIONS.map((o) => (
            <option key={o.v} value={o.v}>
              {o.label}
            </option>
          ))}
        </select>
      </label>
      <label className="row agent-item">
        <span className="grow">Agent 互相调用最多几层</span>
        <select
          value={s.max_call_depth}
          onChange={(e) => void save({ max_call_depth: Number(e.target.value) })}
        >
          {Array.from({ length: 10 }, (_, i) => i + 1).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </select>
      </label>
      {err && <div className="auth-err">{err}</div>}
    </section>
  );
}
