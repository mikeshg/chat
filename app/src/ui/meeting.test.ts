import { describe, expect, it } from "vitest";
import { meetingMessage, randomRoomId } from "./meeting";

// ===== V2 P3-2 =====
const ROOM = /^[a-z0-9]{12}$/;

describe("meetingMessage", () => {
  it("uses the configured base and a 12-char lowercase room id", () => {
    const text = meetingMessage("https://meet.jit.si/", () => "ab12cd34ef56");
    expect(text).toBe(
      "📹 发起了视频会议，点击加入：https://meet.jit.si/chat-ab12cd34ef56",
    );
    expect(text.split("chat-")[1]).toMatch(ROOM);
  });

  it("draws each random character from lowercase letters and digits", () => {
    const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
    const text = meetingMessage("https://meet.example", () => {
      let out = "";
      for (let i = 0; i < 12; i++) out += alphabet[(i * 7) % alphabet.length];
      return out;
    });
    const room = text.split("chat-")[1];
    expect(room).toHaveLength(12);
    expect([...room].every((c) => alphabet.includes(c))).toBe(true);
    expect(text.startsWith("📹 发起了视频会议，点击加入：https://meet.example/chat-")).toBe(
      true,
    );
  });

  it("default room id is 12 lowercase letters or digits", () => {
    for (let i = 0; i < 20; i++) {
      expect(randomRoomId()).toMatch(ROOM);
    }
    const text = meetingMessage("https://meet.jit.si///");
    expect(text).toMatch(
      /^📹 发起了视频会议，点击加入：https:\/\/meet\.jit\.si\/chat-[a-z0-9]{12}$/,
    );
  });
});
