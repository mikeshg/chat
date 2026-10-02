import { describe, expect, it } from "vitest";
import { audioBubbleWidth, formatVoiceDuration, pickRecorderMime, voiceTooShort } from "./voice";

// ===== V2 P2-3 =====

describe("语音录音与气泡", () => {
  it("按顺序挑第一个容器声明支持的录音格式", () => {
    expect(
      pickRecorderMime((t) => t === "audio/ogg;codecs=opus" || t === "audio/mp4"),
    ).toEqual({ mime: "audio/ogg;codecs=opus", ext: "ogg" });
    expect(pickRecorderMime((t) => t === "audio/mp4")).toEqual({ mime: "audio/mp4", ext: "m4a" });
    expect(pickRecorderMime(() => true)?.mime).toBe("audio/webm;codecs=opus");
    expect(pickRecorderMime(() => false)).toBeNull();
  });

  it("气泡宽度随时长线性增长，140–220 封顶", () => {
    // 1s = 140，60.5s 是端点中点 = 180，120s = 220；超出两端封顶
    expect(audioBubbleWidth(0)).toBe(140);
    expect(audioBubbleWidth(1_000)).toBe(140);
    expect(audioBubbleWidth(60_500)).toBe(180);
    expect(audioBubbleWidth(120_000)).toBe(220);
    expect(audioBubbleWidth(180_000)).toBe(220);
  });

  it("录音条计时与气泡时长分别格式化", () => {
    expect(formatVoiceDuration(7_000, "timer")).toBe("0:07");
    expect(formatVoiceDuration(65_000, "timer")).toBe("1:05");
    expect(formatVoiceDuration(12_000, "bubble")).toBe('12"');
    expect(formatVoiceDuration(65_400, "bubble")).toBe("1:05");
  });

  it("短于 1 秒不发送", () => {
    expect(voiceTooShort(999)).toBe(true);
    expect(voiceTooShort(1_000)).toBe(false);
  });
});
