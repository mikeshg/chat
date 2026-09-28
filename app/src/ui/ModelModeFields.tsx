import type { AgentOptionItem, ReportedOptions } from "../types";

function OptionSelect({
  label,
  value,
  items,
  onChange,
}: {
  label: string;
  value: string;
  items: AgentOptionItem[];
  onChange: (v: string) => void;
}) {
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="">{label}：默认</option>
      {value && !items.some((m) => m.id === value) && <option value={value}>{value}</option>}
      {items.map((m) => (
        <option key={m.id} value={m.id}>
          {m.name}
        </option>
      ))}
    </select>
  );
}

/** 添加页和详情页共用：模型 / 模式两个下拉 + 重新读取。可选项来自程序自己上报。 */
export default function ModelModeFields({
  model,
  mode,
  options,
  probing,
  error,
  disabled,
  onModel,
  onMode,
  onReload,
}: {
  model: string;
  mode: string;
  options: ReportedOptions;
  probing: boolean;
  error: string;
  disabled?: boolean;
  onModel: (v: string) => void;
  onMode: (v: string) => void;
  onReload: () => void;
}) {
  return (
    <div className="am-field">
      <span className="am-lb">模型和模式</span>
      <div className="am-row">
        <OptionSelect label="模型" value={model} items={options.models ?? []} onChange={onModel} />
        <OptionSelect label="模式" value={mode} items={options.modes ?? []} onChange={onMode} />
      </div>
      <span className="am-hint">
        可选项由程序自己提供；列表不全时点
        <button type="button" className="am-link" disabled={probing || disabled} onClick={onReload}>
          {probing ? "读取中…" : "重新读取"}
        </button>
        （电脑需在线）。
      </span>
      {error && <span className="auth-err">读取失败：{error}</span>}
    </div>
  );
}
