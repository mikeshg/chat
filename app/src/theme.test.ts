import { describe, expect, it } from "vitest";
import { readThemeChoice, resolveDataTheme, THEME_KEY } from "./theme";

function memStorage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  return { getItem: (k: string) => (m.has(k) ? m.get(k)! : null) };
}

// ===== V2 P1-3 =====
describe("主题解析（存储值 → data-theme）", () => {
  it.each([
    ["light", "light"],
    ["dark", "dark"],
    ["system", undefined],
    [null, undefined],
    ["", undefined],
    ["blue", undefined],
  ])("resolveDataTheme(%s) → %s", (input, expected) => {
    expect(resolveDataTheme(input)).toBe(expected);
  });

  it("readThemeChoice：合法值原样返回，缺失/非法/异常 → system", () => {
    expect(readThemeChoice(memStorage({ [THEME_KEY]: "dark" }))).toBe("dark");
    expect(readThemeChoice(memStorage({ [THEME_KEY]: "light" }))).toBe("light");
    expect(readThemeChoice(memStorage({ [THEME_KEY]: "system" }))).toBe("system");
    expect(readThemeChoice(memStorage())).toBe("system");
    expect(readThemeChoice(memStorage({ [THEME_KEY]: "junk" }))).toBe("system");
    expect(
      readThemeChoice({
        getItem: () => { throw new Error("denied"); },
      }),
    ).toBe("system");
  });
});
