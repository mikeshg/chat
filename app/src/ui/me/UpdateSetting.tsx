import { useEffect, useState } from "react";
import { getServer, osOfUa } from "../../api";
import {
  apkUrl,
  assetFor,
  hasInstaller,
  assetLabel,
  downloadUrl,
  fetchLatest,
  isNewer,
  type ClientLatest,
} from "../../update";
import SetRow from "./SetRow";

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
    <SetRow
      label="下载客户端"
      desc={
        <>
          最新版本 {latest.version}
          {!mine && ` · 暂时没有 ${os} 版`}
          <details className="set-more">
            <summary>其他平台</summary>
            {all.map((n) => (
              <div key={n}>
                <a href={downloadUrl(n)} download>
                  {assetLabel(n)}
                </a>
                <span> · {n}</span>
              </div>
            ))}
          </details>
        </>
      }
    >
      {mine && (
        <a className="btn primary" href={downloadUrl(mine)} download>
          下载 {assetLabel(mine)} 版
        </a>
      )}
    </SetRow>
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
  const newer = !!latest && isNewer(latest.version, current);
  const hasNew =
    newer &&
    hasInstaller(
      osOfUa(navigator.userAgent),
      latest.assets.map((a) => a.name),
    );
  // tag 说明第一行常就是版本号，不重复显示
  const notes = latest?.notes.replace(new RegExp(`^v?${latest.version}\\s*\\n`), "").trim();

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
        const updated = await invoke<boolean>("install_update", {
          server: getServer(),
        });
        if (!updated) setHint("这个平台暂时没有可用的安装包");
      }
    } catch (e) {
      setHint(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SetRow
      label="应用更新"
      desc={
        <>
          当前版本 {current}
          {/* 安装包还没传完的版本不算「有」 */}
          {latest && ` · 最新版本 ${hasNew ? latest.version : current}`}
          {hasNew && notes && <div style={{ whiteSpace: "pre-wrap" }}>{notes}</div>}
          {hint && <div>{hint}</div>}
        </>
      }
    >
      {hasNew && (
        <button className="primary" disabled={busy} onClick={() => void install()}>
          {android ? "下载新版" : "更新并重启"}
        </button>
      )}
    </SetRow>
  );
}
