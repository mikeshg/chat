import { PERMISSION_TIERS, tierChoice } from "./approval";

/** 创建 / 编辑 Agent 时选权限档位，下面附一句这个档位会自动放行什么。 */
export default function PermissionTierField({
  value,
  onChange,
}: {
  value: string;
  onChange: (tier: string) => void;
}) {
  const tier = tierChoice(value);
  return (
    <label className="agent-field">
      <span className="muted">权限档位</span>
      <select aria-label="权限档位" value={tier.id} onChange={(e) => onChange(e.target.value)}>
        {PERMISSION_TIERS.map((t) => (
          <option key={t.id} value={t.id}>
            {t.label}
          </option>
        ))}
      </select>
      <span className="muted approval-tier-hint">{tier.hint}</span>
    </label>
  );
}
