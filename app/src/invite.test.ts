import { describe, expect, it } from "vitest";
import { inviteLink, parseInviteParam } from "./invite";

describe("parseInviteParam", () => {
  it("提取 ?invite=CODE 并去空白", () => {
    expect(parseInviteParam("?invite=ABC123")).toBe("ABC123");
    expect(parseInviteParam("?foo=1&invite=XY99Z&bar=2")).toBe("XY99Z");
    expect(parseInviteParam("?invite=%20AB%20")).toBe("AB");
  });
  it("缺失/空值/非法 search → 空串", () => {
    expect(parseInviteParam("")).toBe("");
    expect(parseInviteParam("?other=1")).toBe("");
    expect(parseInviteParam("?invite=")).toBe("");
    expect(parseInviteParam("?invite")).toBe("");
  });
});

describe("inviteLink", () => {
  it("优先用配置的服务器地址（忽略 origin）", () => {
    expect(inviteLink("http://192.168.1.5:8080/", "http://localhost:5173", "CODE1234AB")).toBe(
      "http://192.168.1.5:8080/?invite=CODE1234AB",
    );
  });
  it("无服务器地址时 Web 同源用 origin", () => {
    expect(inviteLink("", "https://chat.example.com/", "C123")).toBe(
      "https://chat.example.com/?invite=C123",
    );
  });
  it("Tauri origin 不算服务器地址", () => {
    expect(inviteLink("", "http://tauri.localhost", "C")).toBe("");
    expect(inviteLink("", "tauri://localhost", "C")).toBe("");
  });
  it("都没有 → 空串（调用方只复制码）", () => {
    expect(inviteLink("", "", "C")).toBe("");
    expect(inviteLink("  ", "about:blank", "C")).toBe("");
  });
});
