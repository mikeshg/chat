import { useState, type ReactNode } from "react";

export type SettingsSection = {
  id: string;
  label: string;
  /** 一级菜单显示图标，子菜单可省略 */
  icon?: () => ReactNode;
  /** 表格类内容用宽版 */
  wide?: boolean;
  /** 有 children 的是分组（可折叠的一级菜单），本身没有内容 */
  render?: () => ReactNode;
  children?: SettingsSection[];
};

const leaves = (xs: SettingsSection[]): SettingsSection[] =>
  xs.flatMap((x) => (x.children ? leaves(x.children) : [x]));

/** 微信设置式布局：左边分类（可两级）、右边当前分类的内容。窄屏先只列分类，点进去再看内容，左上角返回。 */
export default function SettingsShell({ sections }: { sections: SettingsSection[] }) {
  const all = leaves(sections);
  const [id, setId] = useState(all[0].id);
  // 只影响窄屏：true = 正在看某个分类的内容
  const [opened, setOpened] = useState(false);
  // 折叠起来的分组；默认全部展开
  const [folded, setFolded] = useState<Set<string>>(new Set());
  const cur = all.find((x) => x.id === id) ?? all[0];

  const item = (x: SettingsSection, sub: boolean) => (
    <button
      key={x.id}
      className={`st-nav-item${sub ? " sub" : ""}${x.id === cur.id ? " on" : ""}`}
      onClick={() => {
        setId(x.id);
        setOpened(true);
      }}
    >
      {x.icon && <x.icon />}
      {x.label}
    </button>
  );

  return (
    <main className={`st-shell${opened ? " opened" : ""}`}>
      <nav className="st-nav">
        {sections.map((x) => {
          if (!x.children) return item(x, false);
          const open = !folded.has(x.id);
          return (
            <div key={x.id} className="st-nav-group">
              <button
                className={`st-nav-item st-nav-parent${open ? " open" : ""}`}
                aria-expanded={open}
                onClick={() =>
                  setFolded((f) => {
                    const n = new Set(f);
                    if (open) n.add(x.id);
                    else n.delete(x.id);
                    return n;
                  })
                }
              >
                {x.icon && <x.icon />}
                {x.label}
                <span className="st-caret">›</span>
              </button>
              {open && x.children.map((c) => item(c, true))}
            </div>
          );
        })}
      </nav>
      <div className="st-body">
        <div className={`st-page${cur.wide ? " wide" : ""}`}>
          <h2 className="st-head">
            <button className="chat-back" aria-label="返回" onClick={() => setOpened(false)}>
              ‹
            </button>
            {cur.label}
          </h2>
          {cur.render?.()}
        </div>
      </div>
    </main>
  );
}
