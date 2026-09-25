import { describe, expect, it } from "vitest";
import { coverCrop } from "./avatar";
import { mergeAvatar, senderAvatar } from "./store";

describe("头像居中裁剪", () => {
  it("宽图裁掉左右，高图裁掉上下，正方形不裁", () => {
    // 800×200：源裁剪矩形是中间 200×200，目标缩到 256
    expect(coverCrop(800, 200, 256)).toEqual({ sx: 300, sy: 0, sw: 200, sh: 200 });
    // 100×400：源裁剪矩形是中间 100×100
    expect(coverCrop(100, 400, 256)).toEqual({ sx: 0, sy: 150, sw: 100, sh: 100 });
    expect(coverCrop(256, 256, 256)).toEqual({ sx: 0, sy: 0, sw: 256, sh: 256 });
  });

  it("奇数边长时左右/上下余量差一像素，仍取居中正方形", () => {
    expect(coverCrop(5, 3, 256)).toEqual({ sx: 1, sy: 0, sw: 3, sh: 3 });
    expect(coverCrop(3, 5, 256)).toEqual({ sx: 0, sy: 1, sw: 3, sh: 3 });
  });
});

describe("头像版本只增不减", () => {
  it("只接受更大的版本；同版本更新 has，旧版本丢弃", () => {
    const current = { version: 2, has: true };
    expect(mergeAvatar(current, { version: 1, has: false })).toEqual(current);
    expect(mergeAvatar(current, { version: 2, has: false })).toEqual({ version: 2, has: false });
    expect(mergeAvatar(current, { version: 3, has: false })).toEqual({ version: 3, has: false });
    expect(mergeAvatar(undefined, { version: 0, has: false })).toEqual({ version: 0, has: false });
  });

  it("消息行取消息自带版本与 store 中较大的那组", () => {
    expect(senderAvatar({ version: 1, has: false }, 2, true)).toEqual({ version: 2, has: true });
    expect(senderAvatar({ version: 4, has: true }, 2, false)).toEqual({ version: 4, has: true });
    expect(senderAvatar(undefined, 0, false)).toEqual({ version: 0, has: false });
  });
});
