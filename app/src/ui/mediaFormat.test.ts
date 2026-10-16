import { describe, expect, it } from "vitest";
import { attImgSrc, fmtBytes, imageBubbleSize, storageText } from "./mediaFormat";

describe("imageBubbleSize", () => {
  it("最长边缩到 240，比例保持", () => {
    expect(imageBubbleSize(1200, 800)).toEqual({ width: 240, height: 160 });
    expect(imageBubbleSize(800, 1200)).toEqual({ width: 160, height: 240 });
    expect(imageBubbleSize(480, 480)).toEqual({ width: 240, height: 240 });
  });
  it("超过上限的大图同样缩到 240", () => {
    expect(imageBubbleSize(300, 200)).toEqual({ width: 240, height: 160 });
    expect(imageBubbleSize(10000, 1)).toEqual({ width: 240, height: 1 });
  });
  it("小图不放大，按原尺寸占位", () => {
    expect(imageBubbleSize(100, 50)).toEqual({ width: 100, height: 50 });
    expect(imageBubbleSize(1, 1)).toEqual({ width: 1, height: 1 });
  });
  it("非法输入返回 0 尺寸", () => {
    expect(imageBubbleSize(0, 0)).toEqual({ width: 0, height: 0 });
    expect(imageBubbleSize(-5, 100)).toEqual({ width: 0, height: 0 });
  });
});

describe("attImgSrc（MM2 fix3：复用发送的乐观附件不闪破图）", () => {
  const PUBLIC = "https://srv/v1/files/att1?thumb=1";
  const OBJ = "blob:obj-url";

  it("普通附件走公共能力 URL", () => {
    expect(attImgSrc({ recalled: false }, OBJ, PUBLIC)).toBe(PUBLIC);
    expect(attImgSrc({}, undefined, PUBLIC)).toBe(PUBLIC);
  });

  it("撤回件复用的乐观附件：只用已取回的 objectURL，永不回退公共 URL", () => {
    expect(attImgSrc({ recalled: true }, OBJ, PUBLIC)).toBe(OBJ);
    // objectURL 还没取回 → null（调用方画中性占位），绝不返回公共 URL
    expect(attImgSrc({ recalled: true }, undefined, PUBLIC)).toBeNull();
    expect(attImgSrc({ recalled: true }, null, PUBLIC)).toBeNull();
  });
});

describe("fmtBytes", () => {
  it("按量级格式化", () => {
    expect(fmtBytes(500)).toBe("500 B");
    expect(fmtBytes(2048)).toBe("2.0 KB");
    expect(fmtBytes(614400)).toBe("0.6 MB");
    expect(fmtBytes(1048576)).toBe("1.0 MB");
    expect(fmtBytes(2 * 1024 * 1024 * 1024)).toBe("2.0 GB");
  });
});

describe("storageText", () => {
  it("有限额：已用 / 上限", () => {
    expect(storageText(614400, 1048576)).toBe("已用 0.6 MB / 上限 1.0 MB");
    expect(storageText(0, 1048576)).toBe("已用 0 B / 上限 1.0 MB");
  });
  it("不限额（null 或 0）", () => {
    expect(storageText(614400, null)).toBe("已用 0.6 MB（不限额）");
    expect(storageText(614400, 0)).toBe("已用 0.6 MB（不限额）");
  });
});
