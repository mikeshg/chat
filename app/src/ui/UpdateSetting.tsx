import { useEffect, useState } from "react";
import { getServer } from "../api";
import { apkUrl, fetchLatest, isNewer, type ClientLatest } from "../update";

/** 「我」页应用更新。只在 Tauri 壳里显示（浏览器端刷新就是新版）。 */
export default function UpdateSetting() {
  const shell = "__TAURI_INTERNALS__" in window;
  const android = /Android/i.test(navigator.userAgent);
  const [current, setCurrent] = useState("");
  const [latest, setLatest] = useState<ClientLatest | null>(null);
  const [busy, setBusy] = useState(false);
  const [hint, setHint] = useState("");

  useEffect(() => {
    if (!shell) return;
    void import("@tauri-apps/api/app").then((m) => m.getVersion()).then(setCurrent);
    void fetchLatest().then(setLatest);
  }, [shell]);

  if (!shell || !current) return null;
  const hasNew = !!latest && isNewer(latest.version, current);

  const install = async () => {
    if (!latest || busy) return;
    setBusy(true);
    setHint("");
    try {
      if (android) {
        const url = apkUrl(latest);
        if (!url) throw new Error("这个版本没有 Android 安装包");
        const { openUrl } = await import("@tauri-apps/plugin-opener");
        await openUrl(url);
        setHint("下载完成后点开安装包即可覆盖安装");
      } else {
        setHint("正在下载，完成后会自动安装并重启…");
        const { invoke } = await import("@tauri-apps/api/core");
        const updated = await invoke<boolean>("install_update", { server: getServer() });
        if (!updated) setHint("这个平台暂时没有可用的安装包");
      }
    } catch (e) {
      setHint(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <h3>应用更新</h3>
      <div className="muted">
        当前版本 {current}
        {latest && !hasNew && " · 已是最新"}
      </div>
      {hasNew && (
        <>
          <div>发现新版本 {latest.version}</div>
          {latest.notes && <div className="muted" style={{ whiteSpace: "pre-wrap" }}>{latest.notes}</div>}
          <button className="primary" disabled={busy} onClick={() => void install()}>
            {android ? "下载新版" : "更新并重启"}
          </button>
        </>
      )}
      {hint && <div className="muted">{hint}</div>}
    </section>
  );
}
