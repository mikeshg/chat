import { describe, expect, it } from "vitest";
import { Z } from "./layers";

// ===== MF2-fix1：浮层层级守护 =====
// 记录查看页内打开的查看器/预览不关原页，在其工具栏点「转发」弹出的
// ForwardModal 必须压过预览与查看页（DOM 序帮不上忙：JSX 里转发弹窗
// 先于预览渲染，同 z-index 会被盖住）。右键菜单/动作面板/提示保持更高。

describe("浮层叠放顺序（layers.ts）", () => {
  it("转发弹窗 > 查看器/预览 > 记录查看页", () => {
    expect(Z.forwardModal).toBeGreaterThan(Z.modal);
    expect(Z.modal).toBeGreaterThan(Z.recordView);
    expect(Z.forwardModal).toBeGreaterThan(Z.recordView);
  });

  it("菜单/动作面板/提示仍在所有弹窗之上", () => {
    expect(Z.ctxMask).toBeGreaterThan(Z.forwardModal);
    expect(Z.ctxMenu).toBeGreaterThan(Z.ctxMask);
    expect(Z.sheet).toBeGreaterThan(Z.ctxMenu);
    expect(Z.toast).toBeGreaterThan(Z.ctxMenu);
    expect(Z.hint).toBeGreaterThan(Z.sheet);
  });
});
