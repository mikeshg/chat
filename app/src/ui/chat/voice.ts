/** 录音格式候选：按浏览器支持顺序挑。 */
export const RECORDER_CANDIDATES = [
  { mime: "audio/webm;codecs=opus", ext: "webm" },
  { mime: "audio/ogg;codecs=opus", ext: "ogg" },
  { mime: "audio/mp4", ext: "m4a" },
] as const;

export function pickRecorderMime(
  isTypeSupported: (mime: string) => boolean,
): { mime: string; ext: string } | null {
  return RECORDER_CANDIDATES.find((c) => isTypeSupported(c.mime)) ?? null;
}

/** 气泡宽度：1s→140px，120s→220px，其间线性，两端封顶。短消息也要留出播放键和进度条。 */
export function audioBubbleWidth(durationMs: number): number {
  const clamped = Math.min(120_000, Math.max(1_000, durationMs));
  return Math.round(140 + ((clamped - 1_000) / 119_000) * 80);
}

export function formatVoiceDuration(ms: number, mode: "timer" | "bubble"): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  if (mode === "bubble" && total < 60) return `${total}"`;
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

export function voiceTooShort(ms: number): boolean {
  return ms < 1_000;
}

export const VOICE_MAX_MS = 120_000;
export const MIC_UNAVAILABLE = "需要 HTTPS 或 localhost 才能录音";
export const MIC_DENIED = "麦克风权限被拒绝";
export const VOICE_TOO_SHORT = "说话时间太短";

export function canRecord(): boolean {
  return typeof navigator !== "undefined" && !!navigator.mediaDevices?.getUserMedia;
}
