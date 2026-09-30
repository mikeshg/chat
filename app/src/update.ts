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

export function apkUrl(latest: ClientLatest): string | null {
  const a = latest.assets.find((x) => x.name.endsWith(".apk"));
  return a ? `${getServer()}/v1/client/download/${encodeURIComponent(a.name)}` : null;
}
