# P1-1 系统通知 + 标题未读 + 任务栏提醒

先读 [../devin-rules.md](../devin-rules.md)。本任务**只改客户端**（含 Tauri 壳），不改服务端、不加迁移。

## 背景

现在唯一的新消息提示是应用内 toast（`app/src/ws.ts` 约 390–410 行调用 `pushToast`，判定函数是 `app/src/store.ts` 的 `shouldNotify`）。窗口最小化或切到别的程序后，用户完全不知道有新消息。

## 目标行为

1. **系统通知**：满足 `shouldNotify(state, m)`（非本人、非当前打开会话、静音会话只放行 @我）**且**窗口不在前台（`document.hidden || !document.hasFocus()`）**且**用户在本设备开启了通知时，弹一条系统通知。
   - 标题：会话名（`convTitle`）；正文：`发送者: 摘要`，摘要规则与 toast 一致（图片 `[图片]`、文件 `[文件] 名称`、其他 kind 用已有 `kindLabel` 思路或 content 前 80 字）；@我 时正文前加 `[有人@我] `。
   - 「通知显示消息内容」关闭时，正文只写 `你收到一条新消息`（@我 时 `有人@了你`），标题仍是会话名。
   - 同一会话的通知用同一个 `tag`（Web）替换旧通知，避免刷屏。
   - 点击通知：聚焦窗口并打开对应会话（复用 `ws.ts` 的 `openConversation`）。Tauri 桌面端如果插件拿不到点击回调，只做到弹出即可，在交付文档说明。
   - 窗口在前台时行为不变（只有 toast）。
2. **标题未读数**：未读总数 = 所有**未静音**会话的 `unread_count` 之和，加上已静音但 `has_mention` 的会话的未读数。`> 0` 时 `document.title = "(N) Chat"`，否则 `"Chat"`；N > 99 显示 `99+`。Tauri 桌面端同步调用窗口 `setTitle`。
3. **任务栏提醒（仅 Tauri 桌面端）**：弹系统通知的同时调用 `getCurrentWindow().requestUserAttention(UserAttentionType.Informational)`；Android 上调用失败要静默吞掉。
4. **设置入口**：「我」页新增「通知」卡片，独立组件 `app/src/ui/NotifySettings.tsx`，在 `MePanel.tsx` 里加一行挂载（放在「连接」卡片之前）。
   - 开关「桌面通知」：打开时请求权限（必须在点击事件里请求，浏览器不允许自动弹权限框）；权限被拒时开关回落并提示「通知权限被拒绝，请在浏览器 / 系统设置里允许本站通知」。
   - 开关「通知显示消息内容」，默认开。
   - 按钮「发送测试通知」。
   - 不支持通知的环境（如 HTTP 非安全上下文、老浏览器）显示「当前环境不支持系统通知」，开关禁用。
   - 两个开关存 `localStorage`（`chat_notify_enabled`、`chat_notify_preview`），按设备生效，读写都包 try/catch。

## 实现要点

- 新文件 `app/src/notify.ts`，对外只暴露少量函数，例如：
  - `notifySupport(): "web" | "tauri" | "none"`
  - `notifyPermission(): Promise<"granted" | "denied" | "default">`
  - `requestNotifyPermission(): Promise<boolean>`
  - `showSystemNotification({ title, body, convId }): Promise<void>`
  - 纯函数 `shouldSystemNotify(state, m, env)`、`unreadTitle(conversations)`、`notificationBody(...)`，方便单测。
- Tauri 检测：`"__TAURI_INTERNALS__" in window`。Tauri 相关 import 用动态 `import()`，保证 Web 端不受影响。
- 调用点：在 `ws.ts` 现有 `pushToast` 那段逻辑旁加一行调用（同样依赖 `!existed`，重复帧不能重复通知）。
- 标题更新放在 `ChatScreen.tsx` 或 `App.tsx` 的一个 `useEffect`（依赖 `s.conversations`），登出后恢复 `"Chat"`。
- 图标：Web 通知用 `/favicon.ico` 或 `app/public` 下已有图标；没有就不传 icon。

## Tauri 改动（允许新增的依赖）

- `app/src-tauri/Cargo.toml`：`tauri-plugin-notification = "2"`
- `app/src-tauri/src/lib.rs`：`.plugin(tauri_plugin_notification::init())`
- `app/src-tauri/capabilities/default.json`：加 `notification:default`、`core:window:allow-set-title`、`core:window:allow-request-user-attention`
- `app/package.json`：`@tauri-apps/api@^2`、`@tauri-apps/plugin-notification@^2`（`pnpm add` 后确认 `pnpm-lock.yaml` 更新；离线装不上就如实记录）
- Android：确认 `app/src-tauri/gen/android/app/src/main/AndroidManifest.xml` 最终会包含 `android.permission.POST_NOTIFICATIONS`（插件的 manifest 合并会带上的话无需手改；不确定就显式加上）。Android 13+ 需要运行时申请，走插件的 `requestPermission`。

## 测试

- Vitest（新文件 `app/src/notify.test.ts`）：
  - `shouldSystemNotify`：前台不弹；本人消息不弹；静音会话普通消息不弹、@我 弹；未开启不弹；当前打开会话且在前台不弹、在后台弹。
  - `unreadTitle`：0 → `Chat`；静音会话不计数、静音但 @我 计数；> 99 → `(99+) Chat`。
  - 预览关闭时正文不含消息内容。
- `app/src-tauri` 下 `cargo check` 必须通过（首次编译较慢）。
- 浏览器实测（用 Playwright 或手动描述都行，**需真实执行**）：两个账号，A 开启通知，把 A 的页面切到后台 / 失焦，B 发消息 → A 收到系统通知、标题出现 `(1) Chat`；A 打开会话后标题恢复。Playwright 可用 `context.grantPermissions(["notifications"])` 并在页面里 hook `window.Notification` 记录调用来断言。

## 不做

- 好友申请、群变更等其他事件的通知。
- Service Worker / Web Push（离线推送是第 3 阶段的 ntfy 方案）。
- 通知声音设置。
