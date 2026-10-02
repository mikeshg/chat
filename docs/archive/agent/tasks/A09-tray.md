# A09 桌面宿主常驻：托盘、开机自启、宿主命名

对应 Gitea issue xiefan/chat#10。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)、`CONTEXT.md` 的 AgentHost 词条、[../delivery/A01.md](../delivery/A01.md)（Tauri 桥接部分）。本票**没有迁移**。

## 背景

桌面端要能「关了窗口也继续当宿主」，可选开机自启；主人能给宿主起名并按宿主查看 Agent。

同批并行：A03、A08、A11。

## 一、服务端

- 宿主名称由主人决定：`host_hello` 里的 `label` **只在第一次登记时**写入（`ON CONFLICT` 时不再覆盖 label，只更新 platform / last_seen_at）。
- `PATCH /v1/agent-hosts/{id}`，body `{ "label": "…" }`（去首尾空白、1–60 字符），只能改自己的宿主（别人 404）；成功后给主人推 `agent_host_status`（带新 label）或 `Invalidate { what: "agents" }`，让其他客户端刷新。
- `GET /v1/agent-hosts` 已有 label / platform / online / last_seen_at，不变。

## 二、Tauri 桌面端

- 托盘：开 `tauri` 的 `tray-icon` 特性，托盘图标 + 菜单「显示窗口」「退出」；单击托盘图标显示窗口。
- 关窗口 = 隐藏到托盘（拦截 close 请求），宿主继续运行；只有托盘「退出」才真正退出（先停宿主）。第一次隐藏时发一条系统通知「Chat 已最小化到托盘，Agent 会继续运行」（复用现有通知插件）。
- 开机自启：允许新增依赖 `tauri-plugin-autostart`（及对应前端包 `@tauri-apps/plugin-autostart`）；默认关闭；开关放在「我」页（只在桌面端显示）。
- 宿主默认名称：首次登记时用机器名（拿不到用「桌面端」）；之后以服务端为准，本地不再覆盖。
- Android 和 Web 行为不变（相关代码 `#[cfg(desktop)]` / 运行时判断）。

## 三、前端

- Agent 管理面板：Agent 按宿主分组显示（组标题 = 宿主名 + 平台 + 在线状态 + 最后在线时间），宿主名可点编辑（调 PATCH）。
- 「我」页桌面端显示「开机自动启动」开关。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A09 =====`：

1. 首次 `host_hello` 写入 label；改名后宿主重连（`host_hello` 带另一个 label）→ label 保持改后的值。
2. PATCH 校验（空、超长 400；别人的宿主 404）；改名后主人的普通连接收到更新。

Vitest：按宿主分组的纯函数（排序：在线在前，其次按名称）。

额外检查：`cd app/src-tauri && cargo check`。托盘、关窗隐藏、自启无法在无头环境验证，写清手动验证步骤留给主会话验收；浏览器实测（无头 Playwright）只验证分组显示与改名。截图放 `server/target/wt/`。

## 不做

- macOS / Linux 打包（A10）；宿主数据目录策略调整。
