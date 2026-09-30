import { useEffect, useRef, useState } from "react";
import { files } from "../api";
import type { AttachmentView } from "../types";
import { sendChat } from "../ws";
import { MicIcon } from "./icons";
import {
  MIC_DENIED,
  MIC_UNAVAILABLE,
  VOICE_MAX_MS,
  VOICE_TOO_SHORT,
  canRecord,
  formatVoiceDuration,
  pickRecorderMime,
  voiceTooShort,
} from "./voice";

interface Props {
  convId: string;
  disabled?: boolean;
  onError: (msg: string) => void;
}

/**
 * 输入框麦克风：点一下开始，录音条里取消或发送。
 * 时长用计时，不从文件读（Chrome 的 webm 没有时长元数据）。
 * 结束 / 取消 / 切会话 / 卸载都释放麦克风。
 */
export default function VoiceRecorder({ convId, disabled, onError }: Props) {
  const supported = canRecord();
  const [recording, setRecording] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const recRef = useRef<MediaRecorder | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const chunks = useRef<Blob[]>([]);
  const startedAt = useRef(0);
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);
  const convAtStart = useRef(convId);
  const convNow = useRef(convId);
  const alive = useRef(true);
  const stopReason = useRef<"send" | "cancel" | "drop">("drop");
  convNow.current = convId;

  const releaseTracks = () => {
    streamRef.current?.getTracks().forEach((t) => t.stop());
    streamRef.current = null;
  };

  const clearTimer = () => {
    if (timer.current) {
      clearInterval(timer.current);
      timer.current = null;
    }
  };

  const stop = (reason: "send" | "cancel" | "drop") => {
    stopReason.current = reason;
    clearTimer();
    const rec = recRef.current;
    if (rec && rec.state === "recording") {
      try {
        rec.requestData();
      } catch {
        /* 部分实现在 stop 前不允许再 requestData */
      }
      rec.stop();
    } else if (rec && rec.state !== "inactive") {
      rec.stop();
    } else {
      releaseTracks();
    }
    setRecording(false);
    setElapsed(0);
  };

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      stop("drop");
    };
    // 只在卸载时释放。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (recRef.current && convId !== convAtStart.current) stop("drop");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [convId]);

  const start = async () => {
    if (!supported || disabled || recording) return;
    const picked = pickRecorderMime((t) =>
      typeof MediaRecorder !== "undefined" && MediaRecorder.isTypeSupported(t),
    );
    if (!picked) {
      onError(MIC_UNAVAILABLE);
      return;
    }
    const conv = convId;
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch {
      onError(MIC_DENIED);
      return;
    }
    // 等待授权期间切走会话或卸载：立刻释放，不再进入录音条。
    if (!alive.current || conv !== convNow.current) {
      stream.getTracks().forEach((t) => t.stop());
      return;
    }
    convAtStart.current = conv;
    streamRef.current = stream;
    chunks.current = [];
    const rec = new MediaRecorder(stream, { mimeType: picked.mime });
    recRef.current = rec;
    rec.ondataavailable = (e) => {
      if (e.data.size > 0) chunks.current.push(e.data);
    };
    rec.onstop = () => {
      const reason = stopReason.current;
      const ms = Date.now() - startedAt.current;
      const blob = new Blob(chunks.current, { type: picked.mime });
      const target = convAtStart.current;
      recRef.current = null;
      releaseTracks();
      if (!alive.current || reason !== "send") return;
      if (voiceTooShort(ms)) {
        onError(VOICE_TOO_SHORT);
        return;
      }
      const file = new File([blob], `voice-${Date.now()}.${picked.ext}`, { type: picked.mime });
      void files
        .upload(target, file, { durationMs: ms })
        .then((att: AttachmentView) => {
          if (alive.current) sendChat(target, "", att);
        })
        .catch((e: unknown) => {
          if (alive.current) onError(e instanceof Error ? e.message : "语音发送失败");
        });
    };
    stopReason.current = "drop";
    startedAt.current = Date.now();
    rec.start(200);
    setElapsed(0);
    setRecording(true);
    timer.current = setInterval(() => {
      const ms = Date.now() - startedAt.current;
      setElapsed(ms);
      if (ms >= VOICE_MAX_MS) stop("send");
    }, 200);
  };

  if (recording) {
    return (
      <div className="voice-bar" role="status" aria-label="正在录音">
        <span className="voice-dot" aria-hidden />
        <span className="voice-timer">{formatVoiceDuration(elapsed, "timer")}</span>
        <button type="button" className="voice-cancel" onClick={() => stop("cancel")}>
          取消
        </button>
        <button type="button" className="voice-send" aria-label="发送语音消息" onClick={() => stop("send")}>
          发送
        </button>
      </div>
    );
  }

  return (
    <button
      type="button"
      className="icon-btn voice-btn"
      title={supported ? "发送语音" : MIC_UNAVAILABLE}
      aria-label="发送语音"
      disabled={!supported || disabled}
      onClick={() => void start()}
    >
      <MicIcon />
    </button>
  );
}
