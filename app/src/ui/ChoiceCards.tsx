/** 带说明的单选卡片。id 可以是空串（跟随默认）。 */
export default function ChoiceCards({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  options: { id: string; label: string; hint?: string }[];
  onChange: (id: string) => void;
}) {
  return (
    <div className="am-choices" role="radiogroup" aria-label={label}>
      {options.map((o) => {
        const on = value === o.id;
        return (
          <button
            key={o.id === "" ? "default" : o.id}
            type="button"
            role="radio"
            aria-checked={on}
            className={`am-choice${on ? " on" : ""}`}
            onClick={() => onChange(o.id)}
          >
            <i className="am-radio" />
            <span>
              <b>{o.label}</b>
              {o.hint ? <span>{o.hint}</span> : null}
            </span>
          </button>
        );
      })}
    </div>
  );
}
