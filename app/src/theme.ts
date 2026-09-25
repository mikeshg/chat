// ===== V2 P1-3 =====
// 外观主题：system / light / dark，存 localStorage chat_theme（按设备生效）。
// system = 不设 data-theme，由 CSS color-scheme + 媒体查询跟随系统。

export type ThemeChoice = "system" | "light" | "dark";
export const THEME_KEY = "chat_theme";

export interface ThemeStorage {
  getItem(key: string): string | null;
}

/** 读存储值；缺失/非法/读取失败一律按 system */
export function readThemeChoice(storage: ThemeStorage): ThemeChoice {
  try {
    const v = storage.getItem(THEME_KEY);
    return v === "light" || v === "dark" || v === "system" ? v : "system";
  } catch {
    return "system";
  }
}

/** 存储值 → <html data-theme> 取值；system/非法值 → 不设属性（交给媒体查询） */
export function resolveDataTheme(choice: string | null | undefined): "light" | "dark" | undefined {
  return choice === "light" || choice === "dark" ? choice : undefined;
}

/** 立即应用：在 <html> 上落 data-theme（或移除） */
export function applyTheme(choice: ThemeChoice): void {
  const v = resolveDataTheme(choice);
  if (v) document.documentElement.dataset.theme = v;
  else delete document.documentElement.dataset.theme;
}
