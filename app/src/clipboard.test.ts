import { describe, expect, it } from "vitest";
import { copyPlainText } from "./clipboard";

// ===== MM2 =====
describe("copyPlainText：剪贴板 helper", () => {
  it("Clipboard API 可用时优先走 writeText", async () => {
    let written = "";
    const ok = await copyPlainText("hi", {
      clip: { writeText: async (t) => { written = t; } },
      legacy: () => { throw new Error("不应走兜底"); },
    });
    expect(ok).toBe(true);
    expect(written).toBe("hi");
  });

  it("writeText 拒绝（非安全上下文/权限）→ 退化 execCommand 兜底", async () => {
    let legacyText = "";
    const ok = await copyPlainText("fallback", {
      clip: { writeText: async () => { throw new Error("denied"); } },
      legacy: (t) => { legacyText = t; return true; },
    });
    expect(ok).toBe(true);
    expect(legacyText).toBe("fallback");
  });

  it("clipboard 对象不存在 → 直接走兜底", async () => {
    const ok = await copyPlainText("x", { clip: null, legacy: () => true });
    expect(ok).toBe(true);
  });

  it("两条路都失败 → false（调用方据此提示「复制失败」）", async () => {
    const ok = await copyPlainText("x", {
      clip: { writeText: async () => { throw new Error("no"); } },
      legacy: () => false,
    });
    expect(ok).toBe(false);
    const ok2 = await copyPlainText("x", { clip: null, legacy: () => { throw new Error("no"); } });
    expect(ok2).toBe(false);
  });
});
