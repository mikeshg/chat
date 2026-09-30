import { useEffect, useState } from "react";
import { getServer, osOfUa } from "../api";
import {
  apkUrl,
  assetFor,
  assetLabel,
  downloadUrl,
  fetchLatest,
  isNewer,
  type ClientLatest,
} from "../update";

/** 「我」页：Tauri 壳里是应用更新；浏览器里是下载客户端（浏览器端刷新就是新版，不需要更新）。 */
export default function UpdateSetting() {
  return "__TAURI_INTERNALS__" in window ? <ShellUpdate /> : <DownloadClient />;
}

/** 浏览器里：按当前系统给一个推荐安装包，「更多」里列全部平台。没配置在线更新 / 没有 Release → 不显示 */
function DownloadClient() {
  const [latest, setLatest] = useState<ClientLatest | null>(null);
  useEffect(() => {
    void fetchLatest().then(setLatest);
  }, []);
  if (!latest) return null;
  const os = osOfUa(navigator.userAgent);
  const names = latest.assets.map((a) => a.name);
  const mine = assetFor(os, names);
  const all = names.filter((n) => assetLabel(n));
  if (!all.length) return null;
  return (
    <section className="card">
      <h3>下载客户端</h3>
      <div className="muted">最新版本 {latest.version}</div>
      {mine ? (
        <a className="btn primary" href={downloadUrl(mine)} download>
          下载 {assetLabel(mine)} 版
        </a>
      ) : (
        <div className="muted">暂时没有 {os} 版，可以在下面选其他平台</div>
      )}
      <details className="am-adv">
        <summary>更多</summary>
        {all.map((n) => (
          <div key={n}>
            <a href={downloadUrl(n)} download>
              {assetLabel(n)}
            </a>
            <span className="muted"> · {n}</span>
          </div>
        ))}
      </details>
    </section>
  );
}

function ShellUpdate() {
  const android = /Android/i.test(navigator.userAgent);
  const [current, setCurrent] = useState("");
  const [latest, setLatest] = useState<ClientLatest | null>(null);
  const [busy, setBusy] = useState(false);
  const [hint, setHint] = useState("");

  useEffect(() => {
    void import("@tauri-apps/api/app").then((m) => m.getVersion()).then(setCurrent);
    void fetchLatest().then(setLatest);
  }, []);

  if (!current) return null;
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
