import { describe, expect, it } from "vitest";
import { assetFor, assetLabel, hasInstaller, isNewer } from "./update";

describe("isNewer", () => {
  it.each([
    ["0.2.0", "0.1.0", true],
    ["1.2.10", "1.2.9", true],
    ["1.0.0", "1.0.0", false],
    ["0.9.9", "1.0.0", false],
    ["1.1", "1.0.5", true],
    ["1.0.0", "1.0", false],
  ])("isNewer(%s, %s) = %s", (a, b, want) => {
    expect(isNewer(a, b)).toBe(want);
  });
});

describe("安装包挑选", () => {
  const names = [
    "Chat_0.2.0_amd64.AppImage",
    "Chat_0.2.0_amd64.AppImage.sig",
    "Chat_0.2.0_amd64.deb",
    "Chat_0.2.0_arm64.apk",
    "Chat_0.2.0_x64-setup.exe",
    "Chat_0.2.0_x64-setup.exe.sig",
  ];
  it.each([
    ["Windows", "Chat_0.2.0_x64-setup.exe"],
    ["Android", "Chat_0.2.0_arm64.apk"],
    ["Linux", "Chat_0.2.0_amd64.AppImage"],
    ["macOS", undefined],
    ["iOS", undefined],
  ])("%s → %s", (os, want) => {
    expect(assetFor(os, names)).toBe(want);
  });
  it("签名文件不列给人下载", () => {
    expect(names.map(assetLabel).filter(Boolean)).toEqual([
      "Linux（AppImage）",
      "Linux（deb）",
      "Android",
      "Windows",
    ]);
  });
});

describe("hasInstaller", () => {
  it("本平台包传完（桌面带 .sig）才算有", () => {
    const linuxOnly = ["Chat_0.2.1_amd64.AppImage", "Chat_0.2.1_amd64.AppImage.sig"];
    expect(hasInstaller("Linux", linuxOnly)).toBe(true);
    expect(hasInstaller("Windows", linuxOnly)).toBe(false);
    expect(hasInstaller("Windows", ["Chat_0.2.1_x64-setup.exe"])).toBe(false);
    expect(hasInstaller("Android", ["Chat_0.2.1.apk"])).toBe(true);
  });
});
