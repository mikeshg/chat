// 在线更新：服务端转发 Gitea 最新 Release（GET /v1/client/latest）。
// 桌面端交给 Rust 的 install_update（tauri-plugin-updater，校验签名后安装重启）；
// Android 打开 APK 下载地址，由系统安装器接手。浏览器端每次加载都是服务器上的新版，不需要。

import { getServer } from "./api";

export interface ClientLatest {
  version: string;
  notes: string;
  published_at: string;
  assets: { name: string; size: number }[];
}

/** "1.2.10" > "1.2.9"；缺的段按 0，非数字段按 0 */
export function isNewer(latest: string, current: string): boolean {
  const p = (v: string) => v.split(/[.+-]/).slice(0, 3).map((x) => parseInt(x, 10) || 0);
  const a = p(latest);
  const b = p(current);
  for (let i = 0; i < 3; i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  }
  return false;
}

/** 没配置 / 没有 Release / 服务端太旧 → null */
export async function fetchLatest(): Promise<ClientLatest | null> {
  try {
    const res = await fetch(`${getServer()}/v1/client/latest`);
    return res.ok ? ((await res.json()) as ClientLatest) : null;
  } catch {
    return null;
  }
}

export function downloadUrl(name: string): string {
  return `${getServer()}/v1/client/download/${encodeURIComponent(name)}`;
}

export function apkUrl(latest: ClientLatest): string | null {
  const a = latest.assets.find((x) => x.name.endsWith(".apk"));
  return a ? downloadUrl(a.name) : null;
}

/** 安装包给人看的平台名；.sig 是更新签名，不给人下载 → null */
export function assetLabel(name: string): string | null {
  if (name.endsWith(".sig")) return null;
  if (name.endsWith("-setup.exe")) return "Windows";
  if (name.endsWith(".apk")) return "Android";
  if (name.endsWith(".deb")) return "Linux（deb）";
  if (name.endsWith(".AppImage")) return "Linux（AppImage）";
  return name;
}

/** 按 osOfUa 的结果挑推荐安装包；没有对应平台（macOS、iOS）→ undefined */
export function assetFor(os: string, names: string[]): string | undefined {
  const suffix = ({ Windows: "-setup.exe", Android: ".apk", Linux: ".AppImage" } as Record<string, string>)[os];
  return suffix ? names.find((n) => n.endsWith(suffix)) : undefined;
}

/** 本平台的安装包已上传（桌面还要有 .sig 才能在线更新）。Release 先建、各平台包陆续上传，
 *  没传完之前不提示新版本，免得点了装不上 */
export function hasInstaller(os: string, names: string[]): boolean {
  const pkg = assetFor(os, names);
  return !!pkg && (os === "Android" || names.includes(`${pkg}.sig`));
}
