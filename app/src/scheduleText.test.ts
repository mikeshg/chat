import { describe, expect, it } from "vitest";
import { describeSchedule } from "./scheduleText";

describe("describeSchedule", () => {
  it("每周一、三、五", () => {
    expect(
      describeSchedule({ type: "weekly", days: [5, 1, 3], time: "09:00" }),
    ).toBe("每周一、三、五 09:00");
  });

  it("每天、间隔、周日", () => {
    expect(describeSchedule({ type: "daily", time: "09:00" })).toBe("每天 09:00");
    expect(describeSchedule({ type: "interval", minutes: 30 })).toBe("每 30 分钟");
    expect(describeSchedule({ type: "weekly", days: [7], time: "18:30" })).toBe("每周日 18:30");
  });

  it("一次：按给定时区显示钟点，不跟浏览器时区走", () => {
    expect(describeSchedule({ type: "once", at: "2026-09-26T01:00:00Z" }, "Asia/Shanghai")).toBe(
      "一次，2026-09-26 09:00",
    );
    expect(describeSchedule({ type: "once", at: "2026-09-26T09:00:00Z" })).toBe(
      "一次，2026-09-26 09:00",
    );
  });
});
