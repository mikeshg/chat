// Agent A01：Tauri 桌面端的宿主桥接。Web 与 Android 全部为 no-op。
//
// 取票回路：Rust 侧宿主库（agent-host crate）每次（重）连前发
// `agent-host://need-ticket { request_id }` 事件 → 这里用现有 API 层
// （自带 access token 刷新）调 GET /v1/ws/ticket → invoke
// agent_host_ticket 把 ticket 或 error 回填给 Rust 的 oneshot。

import { canUseAgent } from "./agentAccess";
import { convs, deviceLabel, getServer } from "./api";

/** 仅 Tauri 桌面壳：浏览器与 Android WebView 都不启动宿主 */
export function isDesktopShell(): boolean {
  const w = globalThis as {
    __TAURI_INTERNALS__?: unknown;
    __TAURI__?: unknown;
    navigator?: { userAgent?: string };
  };
  if (!w.__TAURI_INTERNALS__ && !w.__TAURI__) return false;
  const ua = w.navigator?.userAgent ?? "";
  return !/Android/i.test(ua);
}

let listening: Promise<void> | null = null;

function ensureTicketBridge(): Promise<void> {
  if (listening) return listening;
  listening = (async () => {
    const [{ listen }, { invoke }] = await Promise.all([
      import("@tauri-apps/api/event"),
      import("@tauri-apps/api/core"),
    ]);
    await listen<{ request_id: string }>("agent-host://need-ticket", (e) => {
      const requestId = e.payload.request_id;
      void (async () => {
        try {
          const t = await convs.ticket();
          await invoke("agent_host_ticket", { requestId, ticket: t.ticket });
        } catch (err) {
          // 取票失败（如登出中/会话失效）也要回填 error，宿主侧走重试退避
          try {
            await invoke("agent_host_ticket", {
              requestId,
              error: err instanceof Error ? err.message : "ticket 获取失败",
            });
          } catch {}
        }
      })();
    });
  })();
  listening.catch(() => {
    listening = null; // 下次调用重试
  });
  return listening;
}

/** 登录成功后调用：桌面端拉起宿主（同源部署时 server_url 用 location.origin） */
export async function startAgentHost(): Promise<void> {
  if (!isDesktopShell()) return;
  try {
    await ensureTicketBridge();
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("agent_host_start", {
      serverUrl: getServer() || location.origin,
      // 空串由 Rust 填机器名（拿不到则「桌面端」）。服务端只在首次登记写入，之后改名以服务器为准。
      label: "",
    });
  } catch (e) {
    console.warn("agent_host_start failed", e);
  }
}

/** 按生效使用权拉起或停掉桌面宿主。Web / Android 仍是 no-op。 */
export async function syncAgentHost(
  me: { agent_access?: boolean } | null | undefined,
): Promise<void> {
  if (!canUseAgent(me)) {
    await stopAgentHost();
    return;
  }
  await startAgentHost();
}

/** 退出登录时调用：停掉本机宿主（Web/Android 为 no-op） */
export async function stopAgentHost(): Promise<void> {
  if (!isDesktopShell()) return;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("agent_host_stop");
  } catch {}
}
