import { describe, expect, it } from "vitest";
import { menuPos } from "./menuPos";

// 右键菜单定位：优先在点击点右下展开；出界则翻转到另一側；仍越界就夹紧，菜单边缘不越过视口 margin。
const VW = 1024;
const VH = 900;
const W = 120;
const H = 200;

describe("ctx menu viewport placement", () => {
  it("opens at the click point when everything fits", () => {
    expect(menuPos(400, 300, W, H, VW, VH)).toEqual({ x: 400, y: 300 });
  });

  it("flips left when the menu would overflow the right edge", () => {
    // 点击点离右边缘不足菜单宽：左翻，菜单右缘对齐点击点
    expect(menuPos(VW - 10, 300, W, H, VW, VH)).toEqual({ x: VW - 10 - W, y: 300 });
  });

  it("flips up when the menu would overflow the bottom edge", () => {
    expect(menuPos(400, VH - 10, W, H, VW, VH)).toEqual({ x: 400, y: VH - 10 - H });
  });

  it("flips both axes at the bottom-right corner", () => {
    expect(menuPos(VW - 5, VH - 5, W, H, VW, VH)).toEqual({
      x: VW - 5 - W,
      y: VH - 5 - H,
    });
  });

  it("clamps into the viewport when flipping would overflow the opposite edge", () => {
    // 点击点太靠左，向左翻会出左界 → 夹到 margin，菜单整体仍在视口内
    const p = menuPos(30, 300, W, H, 130, VH, 4);
    expect(p.x).toBe(4);
    expect(p.x + W).toBeLessThanOrEqual(130 - 4);
    // 垂直同理：点击点太高，向上翻出顶 → 夹到 margin
    expect(menuPos(400, 50, W, H, VW, 240, 4).y).toBe(4);
  });

  it("keeps a margin from the right/bottom edges when the click is inside the margin band", () => {
    // 点击点在右下 2px 处：左/上翻仍越 margin → 夹到 margin 边
    const p = menuPos(VW - 2, VH - 2, W, H, VW, VH);
    expect(p.x + W).toBeLessThanOrEqual(VW - 4);
    expect(p.y + H).toBeLessThanOrEqual(VH - 4);
  });

  it("clamps a menu larger than the viewport to the margin", () => {
    expect(menuPos(500, 500, 2000, 2000, VW, VH)).toEqual({ x: 4, y: 4 });
  });

  it("honours a custom margin", () => {
    // margin=10：点击点在右缘 5px 内 → 左翻后右缘仍侵入 margin → 夹到 vw-10-w
    expect(menuPos(VW - 5, 300, W, H, VW, VH, 10)).toEqual({ x: VW - 10 - W, y: 300 });
    // 高度方向夹紧到自定义 margin
    expect(menuPos(400, 60, W, H, VW, 240, 10).y).toBe(10);
  });
});
