import { useEffect, useState } from "react";
import { isDesktopShell } from "../../api";
import SetRow from "./SetRow";

/** 「我」页开机自动启动。只在桌面壳显示，默认关闭（不在这里调用 enable）。 */
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
    <SetRow
      label="开机自动启动"
      desc={hint || "关闭窗口后 Chat 会留在托盘里继续接收消息"}
      toggle={{
        checked: enabled,
        disabled: !ready || busy,
        onChange: () => void toggle(),
      }}
    />
  );
}
