import { useEffect, useRef, useState } from "react";
import { auth } from "../../api";
import type { PushState } from "../../types";

/**
 * ===== V2 P3-3 ===== 「我」页手机推送卡片（ntfy）。
 * 推送服务器地址只能由管理员配置；用户拿到的是服务端生成的随机订阅地址。
 */
export default function PushSettings() {
  const [info, setInfo] = useState<PushState | null>(null);
  const [err, setErr] = useState("");
  const [hint, setHint] = useState("");
  const [busy, setBusy] = useState("");
  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    auth
      .push()
      .then(setInfo)
      .catch((e) => setErr(e instanceof Error ? e.message : "加载失败"));
  }, []);

  const run = async (what: string, f: () => Promise<PushState>) => {
    if (busy) return false;
    setBusy(what);
    setErr("");
    setHint("");
    try {
      setInfo(await f());
      return true;
    } catch (e) {
      setErr(e instanceof Error ? e.message : "操作失败");
      return false;
    } finally {
      setBusy("");
    }
  };

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      if (copiedTimer.current) clearTimeout(copiedTimer.current);
      copiedTimer.current = setTimeout(() => setCopied(false), 2000);
    } catch {
      // 剪贴板不可用（非安全上下文/权限被拒）→ 弹出手动复制
      window.prompt("复制失败，请手动复制：", text);
    }
  };

  const rotate = () => {
    if (!confirm("更换订阅主题？旧主题立即作废，需要在 ntfy App 里重新订阅新地址。")) return;
    void run("rotate", () => auth.rotatePush()).then(
      (ok) => ok && setHint("已更换主题，请在 ntfy App 重新订阅上面的新地址"),
    );
  };

  const test = () =>
    void run("test", async () => {
      const v = await auth.testPush();
      setHint("已发送测试推送，请查看 ntfy App");
      return v;
    });

  return (
    <section className="card">
      <h3>手机推送</h3>
      {info == null ? (
        err ? (
          <div className="auth-err">{err}</div>
        ) : (
          <div className="muted">加载中…</div>
        )
      ) : !info.available ? (
        <div className="muted">管理员未配置推送服务</div>
      ) : (
        <>
          <label className="row pick">
            <input
              type="checkbox"
              checked={info.enabled}
              disabled={busy !== ""}
              onChange={() =>
                void run("toggle", () => auth.updatePush({ enabled: !info.enabled }))
              }
            />
            <span>手机推送（应用退到后台后通过 ntfy 提醒新消息）</span>
          </label>
          <label className="row pick">
            <input
              type="checkbox"
              checked={info.preview}
              disabled={busy !== "" || !info.enabled}
              onChange={(e) =>
                void run("preview", () => auth.updatePush({ preview: e.target.checked }))
              }
            />
            <span>推送显示消息内容</span>
          </label>
          <div className="muted">
            公共 ntfy 服务器上订阅了同一主题的人可能看到推送内容
          </div>
          {info.enabled && info.subscribe_url && (
            <>
              <div className="row push-url">
                <code className="push-url-text">{info.subscribe_url}</code>
                <button onClick={() => void copy(info.subscribe_url!)}>
                  {copied ? "已复制" : "复制"}
                </button>
              </div>
              <div className="muted">
                在手机上安装 ntfy App → 订阅主题 → 粘贴这个地址
              </div>
              <div className="row push-actions">
                <button disabled={busy !== ""} onClick={test}>
                  {busy === "test" ? "发送中…" : "发送测试推送"}
                </button>
                <button disabled={busy !== ""} onClick={rotate}>
                  {busy === "rotate" ? "更换中…" : "更换主题"}
                </button>
              </div>
            </>
          )}
          {err && <div className="auth-err">{err}</div>}
          {hint && <div className="muted">{hint}</div>}
        </>
      )}
    </section>
  );
}
