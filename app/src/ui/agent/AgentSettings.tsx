import { useState } from "react";
import { agents } from "../../api";
import type { AgentOutputMode, AgentUserSettings } from "../../types";
import {
  OUTPUT_MODE_OPTIONS,
  QUEUE_TIMEOUT_OPTIONS,
  queueTimeoutLabel,
} from "./agentPage";
import ChoiceCards from "./ChoiceCards";

export { OUTPUT_MODE_OPTIONS, QUEUE_TIMEOUT_OPTIONS };

/** 默认设置：改动立即保存。输出方式、离线排队、互相调用层数。 */
export default function AgentSettings({
  value,
  onChange,
}: {
  value: AgentUserSettings | null;
  onChange: (next: AgentUserSettings) => void;
}) {
  const [err, setErr] = useState("");

  const save = async (patch: Partial<AgentUserSettings>) => {
    setErr("");
    try {
      onChange(await agents.updateSettings(patch));
    } catch {
      setErr("保存失败");
    }
  };

  if (!value) return <div className="am-hint">{err || "读取设置…"}</div>;
  const timeoutKnown = QUEUE_TIMEOUT_OPTIONS.some((o) => o.v === value.queue_timeout_secs);
  return (
    <div className="am-form">
      <div className="am-field">
        <span className="am-lb">输出方式</span>
        <ChoiceCards
          label="输出方式"
          value={value.output_mode}
          options={OUTPUT_MODE_OPTIONS.map((o) => ({ id: o.v, label: o.label, hint: o.hint }))}
          onChange={(id) => void save({ output_mode: id as AgentOutputMode })}
        />
      </div>
      <div className="am-field">
        <label htmlFor="d-queue">电脑离线时，排队多久后取消</label>
        <select
          id="d-queue"
          value={value.queue_timeout_secs}
          onChange={(e) => void save({ queue_timeout_secs: Number(e.target.value) })}
        >
          {!timeoutKnown && (
            <option value={value.queue_timeout_secs}>{queueTimeoutLabel(value.queue_timeout_secs)}</option>
          )}
          {QUEUE_TIMEOUT_OPTIONS.map((o) => (
            <option key={o.v} value={o.v}>
              {o.label}
            </option>
          ))}
        </select>
      </div>
      <div className="am-field">
        <label htmlFor="d-depth">Agent 互相调用最多几层</label>
        <select
          id="d-depth"
          value={value.max_call_depth}
          onChange={(e) => void save({ max_call_depth: Number(e.target.value) })}
        >
          {Array.from({ length: 10 }, (_, i) => i + 1).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </select>
        <span className="am-hint">
          A 调用 B、B 再调用 C 算两层。超过层数的调用会被拒绝，防止互相无限调用。
        </span>
      </div>
      <div className="am-hint">改动立即生效，不需要保存。</div>
      {err && <div className="auth-err">{err}</div>}
    </div>
  );
}
