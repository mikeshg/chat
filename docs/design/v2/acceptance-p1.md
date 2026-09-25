# V2 第 1 阶段验收记录

**日期：2026-09-25　分支：`v2`　执行：主会话（合并 + 验收）**

## 1. 合并内容

| 任务 | 分支 | 内容 | 交付文档 |
|---|---|---|---|
| P1-1 | `v2-p1-1` | 系统通知 + 标题未读数 + 任务栏提醒 | [delivery/P1-1.md](delivery/P1-1.md) |
| P1-2 | `v2-p1-2` | 注册开关 + 邀请码 + `server_settings`（迁移 0010） | [delivery/P1-2.md](delivery/P1-2.md) |
| P1-3 | `v2-p1-3` | 按会话草稿 + 暗色模式 | [delivery/P1-3.md](delivery/P1-3.md) |
| P1-4 | `v2-p1-4` | 链接 / 行内代码 / 代码块渲染 + Tauri 打开外链 | [delivery/P1-4.md](delivery/P1-4.md) |
| P1-5 | `v2-p1-5` | 自助改密码 + 登录设备管理（迁移 0011） | [delivery/P1-5.md](delivery/P1-5.md) |

合并顺序：P1-4 → P1-1 → P1-3 → P1-5 → P1-2。

## 2. 审查与合并时的修改

| 位置 | 修改 | 原因 |
|---|---|---|
| `app/src/richText.tsx` | 复制失败时不再显示「已复制」 | 原实现无论成败都显示 |
| `app/src/ws.ts`、`store.ts` | 窗口在后台时，打开着的会话照常计未读，回到前台（focus / visibilitychange）再补报已读；新增 2 个单测 | 原行为会让后台消息立即已读，标题不显示未读数，与 A1 目标冲突（P1-1 交付文档记录的遗留问题） |
| `app/src/styles.css` | 追加 `@supports not (color: light-dark(...))` 浅色兜底 | P1-3 全部颜色依赖 `light-dark()`（Chromium ≥ 123），未更新的 Android 系统 WebView 会整套失效成透明 |
| `app/src/styles.css` | P1-4 的两处写死颜色改用 P1-3 新变量（`--link-mine`、`--hover`） | 暗色下对比度 |
| `app/src/notify.test.ts` | 测试状态补 `drafts: {}` | P1-1 与 P1-3 交叉：`AppState` 新增必填字段 |
| 各分支 `server/` | 还原 P1-1、P1-3 中纯 `cargo fmt` 产生的格式化改动 | 这些任务不改服务端，避免合并冲突；合并后全仓 `cargo fmt --check` 已通过 |
| Tauri 配置 | 通知 + opener 两个插件、权限合并；`Cargo.lock` 与 `gen/schemas` 由 `cargo check` 重新生成 | 两个分支同时加插件 |

## 3. 自动化验证（全新隔离库 55610、后端 18110）

| 检查 | 结果 |
|---|---|
| `cargo fmt --check` | 通过 |
| `cargo test --tests` | it **59 passed** / migrations **1 passed** / 0 failed |
| `npx vitest run` | **114 passed** / 0 failed |
| `npm run build`（tsc + vite） | 通过 |
| `app/src-tauri` `cargo check` | 通过（notification 2.4 + opener 2.5） |
| E2E phase1（先注册专用管理员，带 `E2E_ADMIN_USER`） | **58 passed** / 0 failed |
| 真实重启后端后 E2E phase2 | **9 passed** / 0 failed |

说明：全新库上第一个注册用户自动成为管理员，E2E 脚本里的普通用户 A 会因此变成管理员、导致「非管理员 403」两项失败。这是测试前提问题，不是回归；先注册一个管理员再跑即可（上表结果即按此执行）。

## 4. 浏览器实测（内置浏览器，`127.0.0.1` 与 `localhost` 两个 origin 分别登录两个账号）

| 功能 | 步骤 | 结果 |
|---|---|---|
| P1-2 注册模式 | 管理员切「仅限邀请」→ `server-info` 返回 `invite` | 通过 |
| P1-2 邀请码 | 生成 1 次 / 7 天邀请码 → 另一 origin 打开 `/?invite=CODE` | 自动进注册页、邀请码已预填 |
| P1-2 失败路径 | 错误邀请码注册 | 提示「邀请码无效或已失效」 |
| P1-2 主路径 | 小写邀请码注册 | 成功进入应用（大小写不敏感） |
| P1-2 关闭注册 | 切 `closed` 后注册 / 提交非法模式 | 403「管理员已关闭注册」/ 400 |
| P1-4 富文本 | 发送含链接、中文逗号、行内代码、`js` 代码块（块内含 URL）的消息 | 链接在中文逗号处截断、`target=_blank rel=noopener noreferrer`；行内代码、代码块语言标签与复制按钮正确；代码块内 URL 不被识别 |
| P1-1 标题未读 | 后台标签页收到消息 | 标题 `(1) Chat`；打开会话后恢复 `Chat` |
| P1-3 草稿 | 私聊输入 → 刷新页面 → 列表 | 显示 `[草稿]draft text 123`；打开私聊内容恢复、群聊输入框为空；发送后标记消失 |
| P1-3 主题 | 切深色 / 浅色 | `data-theme` 与存储正确；深色 `body` rgb(22,22,22)、文字 rgb(232,232,232)；浅色恢复原配色 |
| P1-5 设备管理 | 另一 origin 用接口登录同账号（设备名「验收第二设备」）→ 管理员「登录设备」点「移出」 | 列表移除该设备；其 access 与 refresh 立即 401 |
| 「我」页 | 卡片顺序 | 通知 / 外观 / 修改密码 / 登录设备 / 连接 均在 |

深色效果另参考 P1-3 Devin 的截图（聊天、登录、管理后台、「我」页各一张），对比度正常。

## 5. 未覆盖项

- Tauri 桌面端真实窗口：系统通知弹出、任务栏闪烁、外链用系统浏览器打开，只做了编译与权限解析验证（需要 cua-driver 操作桌面，待用户同意后补测）。
- Android：只改了 manifest（`POST_NOTIFICATIONS`），未构建 APK。
- 内置浏览器不授予通知权限，Web 系统通知的实际弹出只有 P1-1 Devin 的 Playwright 桩测证据。
- 通过 API 建群时创建者的会话列表不会实时出现新群（界面里的建群流程会自行刷新），属 V1 既有行为，未改。
