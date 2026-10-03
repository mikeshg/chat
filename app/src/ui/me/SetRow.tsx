import type { ReactNode } from "react";

/** 「我」页分组：组名在卡片外，卡片里是一条条 SetRow / .set-block */
export function SetGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <>
      <h3 className="set-title">{title}</h3>
      <section className="card set-group">{children}</section>
    </>
  );
}

type Toggle = {
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
};

/** 设置行：左边标题 + 说明，右边开关（toggle）或任意控件（children）。带开关时整行可点。 */
export default function SetRow(props: {
  label: ReactNode;
  desc?: ReactNode;
  toggle?: Toggle;
  children?: ReactNode;
}) {
  const { toggle } = props;
  const body = (
    <>
      <div className="set-main">
        <div>{props.label}</div>
        {props.desc && <div className="muted">{props.desc}</div>}
      </div>
      {toggle && (
        <input
          type="checkbox"
          role="switch"
          className="switch"
          checked={toggle.checked}
          disabled={toggle.disabled}
          onChange={(e) => toggle.onChange(e.target.checked)}
        />
      )}
      {props.children}
    </>
  );
  return toggle ? (
    <label className="set-row pick">{body}</label>
  ) : (
    <div className="set-row">{body}</div>
  );
}
