// V2 P1-2：邀请链接/URL 参数的纯函数（可单测）

/** 从地址栏 search 提取邀请码（?invite=CODE，可与其他参数共存）；没有 → "" */
export function parseInviteParam(search: string): string {
  try {
    return (new URLSearchParams(search).get("invite") ?? "").trim();
  } catch {
    return "";
  }
}

/**
 * 邀请链接基址：优先显式服务器地址（Tauri 端 origin 是 tauri.localhost，不是服务器）；
 * 其次 http(s) 页面 origin（Web 同源部署）；都没有 → ""（调用方退化为只复制码）。
 */
export function inviteLink(server: string, origin: string, code: string): string {
  const s = server.trim().replace(/\/+$/, "");
  const o = /^https?:\/\//i.test(origin.trim()) && !/tauri\.localhost/i.test(origin)
    ? origin.trim().replace(/\/+$/, "")
    : "";
  const base = s || o;
  return base ? `${base}/?invite=${encodeURIComponent(code)}` : "";
}
