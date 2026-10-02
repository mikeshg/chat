import { useEffect, useState } from "react";
import { isDesktopShell } from "../../agentHost";

/** Agent A09：「我」页开机自动启动。只在桌面壳显示，默认关闭（不在这里调用 enable）。 */
export default function AutostartSetting() {
  const desktop = isDesktopShell();
  const [enabled, setEnabled] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [hint, setHint] = useState("");

  useEffect(() => {
    if (!desktop) return;
    let alive = true;
    void import("@tauri-apps/plugin-autostart")
      .then((m) => m.isEnabled())
      .then((on) => {
        if (!alive) return;
        setEnabled(on);
        setReady(true);
      })
      .catch(() => {
        if (alive) setHint("读取开机启动状态失败");
      });
    return () => {
      alive = false;
    };
  }, [desktop]);

  if (!desktop) return null;

  const toggle = async () => {
    if (!ready || busy) return;
    setBusy(true);
    setHint("");
    try {
      const m = await import("@tauri-apps/plugin-autostart");
      if (enabled) {
        await m.disable();
        setEnabled(false);
      } else {
        await m.enable();
        setEnabled(true);
      }
    } catch (e) {
      setHint(e instanceof Error ? e.message : "设置失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <h3>开机自动启动</h3>
      <label className="row pick">
        <input
          type="checkbox"
          checked={enabled}
          disabled={!ready || busy}
          onChange={() => void toggle()}
        />
        <span>登录系统后自动打开 Chat</span>
      </label>
      <div className="muted">默认关闭。关闭窗口后 Agent 会留在托盘里继续运行。</div>
      {hint && <div className="muted">{hint}</div>}
    </section>
  );
}
