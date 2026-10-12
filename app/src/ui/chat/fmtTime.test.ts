import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fmtTime } from "./fmtTime";

// 桌面端曾出现时间冒号「消失」（显示 1646）。根因是 Linux 字体环境的渲染问题
//（"Segoe UI" 经 fontconfig 落到冒号极窄的字体），而非格式化产生了特殊字符。
// 这里钉住格式化输出：只含 ASCII，冒号必须是普通 U+003A。
describe("fmtTime", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-12T17:30:00"));
  });
  afterEach(() => vi.useRealTimers());

  it("当天时间输出 HH:MM，分隔符是 ASCII 冒号", () => {
    const out = fmtTime(new Date("2026-10-12T16:46:00").toISOString());
    expect(out).toMatch(/^\d{2}:\d{2}$/);
    for (const ch of out) expect(ch.charCodeAt(0)).toBeLessThanOrEqual(0x7f);
  });

  it("昨天返回中文文案", () => {
    expect(fmtTime("2026-10-11T12:00:00")).toBe("昨天");
  });

  it("更早日期输出 M/D", () => {
    expect(fmtTime("2026-09-05T12:00:00")).toBe("9/5");
  });

  it("空输入返回空串", () => {
    expect(fmtTime(undefined)).toBe("");
    expect(fmtTime(null)).toBe("");
  });
});
