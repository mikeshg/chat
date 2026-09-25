import { useEffect, useRef, useState } from "react";
import { files } from "../api";
import type { AttachmentView } from "../types";
import { audioBubbleWidth, formatVoiceDuration } from "./voice";

/** 同一时间只播放一条语音。 */
let current: HTMLAudioElement | null = null;

function pauseOthers(mine: HTMLAudioElement) {
  if (current && current !== mine) current.pause();
  current = mine;
}

export default function AudioBubble({ att }: { att: AttachmentView }) {
  const audioRef = useRef<HTMLAudioElement>(null);
  const [playing, setPlaying] = useState(false);
  const [progress, setProgress] = useState(0);
  const duration = att.duration_ms ?? 0;

  useEffect(() => {
    const el = audioRef.current;
    if (!el) return;
    const onTime = () => {
      const total = duration > 0 ? duration / 1000 : el.duration;
      setProgress(total > 0 && Number.isFinite(total) ? el.currentTime / total : 0);
    };
    const onEnd = () => {
      setPlaying(false);
      setProgress(0);
      if (current === el) current = null;
    };
    const onPause = () => setPlaying(false);
    el.addEventListener("timeupdate", onTime);
    el.addEventListener("ended", onEnd);
    el.addEventListener("pause", onPause);
    return () => {
      el.pause();
      el.removeEventListener("timeupdate", onTime);
      el.removeEventListener("ended", onEnd);
      el.removeEventListener("pause", onPause);
      if (current === el) current = null;
    };
  }, [duration, att.id]);

  const toggle = () => {
    const el = audioRef.current;
    if (!el) return;
    if (el.paused) {
      pauseOthers(el);
      void el.play().then(() => setPlaying(true)).catch(() => setPlaying(false));
    } else {
      el.pause();
    }
  };

  return (
    <div className="voice-bubble" style={{ width: audioBubbleWidth(duration) }} data-voice="1">
      <button
        type="button"
        className="voice-play"
        aria-label={playing ? "暂停语音" : "播放语音"}
        onClick={toggle}
      >
        {playing ? "暂停" : "播放"}
      </button>
      <span className="voice-track" aria-hidden>
        <i style={{ width: `${Math.min(100, progress * 100)}%` }} />
      </span>
      <span className="voice-dur">{formatVoiceDuration(duration, "bubble")}</span>
      <audio ref={audioRef} src={files.url(att.id)} preload="none" />
    </div>
  );
}
