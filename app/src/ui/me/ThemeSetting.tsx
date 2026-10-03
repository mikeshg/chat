import { useState } from "react";
import { applyTheme, readThemeChoice, THEME_KEY, type ThemeChoice } from "../../theme";
import SetRow from "./SetRow";

const OPTIONS: { v: ThemeChoice; label: string }[] = [
  { v: "system", label: "跟随系统" },
  { v: "light", label: "浅色" },
  { v: "dark", label: "深色" },
];

// ===== V2 P1-3 =====
// 「我」页外观卡片：三选一，存 localStorage chat_theme（按设备），立即应用。
export default function ThemeSetting() {
  const [choice, setChoice] = useState<ThemeChoice>(() => readThemeChoice(localStorage));

  const pick = (v: ThemeChoice) => {
    setChoice(v);
    try {
      localStorage.setItem(THEME_KEY, v);
    } catch {}
    applyTheme(v);
  };

  return (
    <SetRow label="外观">
      <div className="seg" role="radiogroup" aria-label="外观">
        {OPTIONS.map((o) => (
          <label key={o.v} className={`seg-opt${choice === o.v ? " on" : ""}`}>
            <input type="radio" name="theme" checked={choice === o.v} onChange={() => pick(o.v)} />
            {o.label}
          </label>
        ))}
      </div>
    </SetRow>
  );
}
