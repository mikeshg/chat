// ===== V2 P4-1 =====
// 备份/恢复面板的纯函数测试：确认词校验 + 文件大小格式化。
import { describe, expect, it } from "vitest";
import { restoreConfirmed } from "./BackupPanel";
import { fmtBytes } from "../mediaFormat";

describe("restoreConfirmed", () => {
  it("只有精确输入 RESTORE 才放行", () => {
    expect(restoreConfirmed("RESTORE")).toBe(true);
  });
  it("大小写、空格、空串都不放行", () => {
    expect(restoreConfirmed("restore")).toBe(false);
    expect(restoreConfirmed("Restore")).toBe(false);
    expect(restoreConfirmed(" RESTORE")).toBe(false);
    expect(restoreConfirmed("RESTORE ")).toBe(false);
    expect(restoreConfirmed("")).toBe(false);
    expect(restoreConfirmed("RESTOR")).toBe(false);
    expect(restoreConfirmed("RESTORED")).toBe(false);
  });
});

describe("备份列表大小格式化", () => {
  it("按量级格式化（复用 mediaFormat.fmtBytes）", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(512)).toBe("512 B");
    expect(fmtBytes(1024)).toBe("1.0 KB");
    expect(fmtBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(fmtBytes(3 * 1024 * 1024 * 1024)).toBe("3.0 GB");
  });
  it("非法输入归零", () => {
    expect(fmtBytes(-1)).toBe("0 B");
    expect(fmtBytes(NaN)).toBe("0 B");
  });
});
