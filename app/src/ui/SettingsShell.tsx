import { useState, type ReactNode } from "react";

export type SettingsSection = {
  id: string;
  label: string;
  icon: () => ReactNode;
  /** 表格类内容用宽版 */
  wide?: boolean;
  render: () => ReactNode;
};

/** 微信设置式布局：左边分类、右边当前分类的内容。窄屏先只列分类，点进去再看内容，左上角返回。 */
export default function SettingsShell({ sections }: { sections: SettingsSection[] }) {
  const [id, setId] = useState(sections[0].id);
  // 只影响窄屏：true = 正在看某个分类的内容
  const [opened, setOpened] = useState(false);
  const cur = sections.find((x) => x.id === id) ?? sections[0];

  return (
    <main className={`st-shell${opened ? " opened" : ""}`}>
      <nav className="st-nav">
        {sections.map((x) => (
          <button
            key={x.id}
            className={`st-nav-item${x.id === cur.id ? " on" : ""}`}
            onClick={() => {
              setId(x.id);
              setOpened(true);
            }}
          >
            <x.icon />
            {x.label}
          </button>
        ))}
      </nav>
      <div className="st-body">
        <div className={`st-page${cur.wide ? " wide" : ""}`}>
          <h2 className="st-head">
            <button className="chat-back" aria-label="返回" onClick={() => setOpened(false)}>
              ‹
            </button>
            {cur.label}
          </h2>
          {cur.render()}
        </div>
      </div>
    </main>
  );
}
