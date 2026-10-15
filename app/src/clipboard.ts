// ===== MM2 =====
// 复制纯文本到剪贴板：优先异步 Clipboard API；非安全上下文（LAN http、
// 旧 WebView）退化到隐藏 textarea + execCommand('copy')。deps 可注入便于单测。

/** execCommand 兜底；无 DOM 环境直接 false */
export function legacyCopyText(text: string): boolean {
  if (typeof document === "undefined") return false;
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  try {
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    ta.remove();
  }
}

export async function copyPlainText(
  text: string,
  deps?: {
    clip?: { writeText(t: string): Promise<void> } | null;
    legacy?: (t: string) => boolean;
  },
): Promise<boolean> {
  const clip =
    deps?.clip !== undefined
      ? deps.clip
      : typeof navigator !== "undefined"
        ? (navigator.clipboard ?? null)
        : null;
  if (clip) {
    try {
      await clip.writeText(text);
      return true;
    } catch {
      // 继续走 execCommand 兜底
    }
  }
  const legacy = deps?.legacy ?? legacyCopyText;
  try {
    return legacy(text);
  } catch {
    return false;
  }
}
