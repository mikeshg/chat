# V2 第 2 阶段验收记录

**日期：2026-09-25　分支：`v2`　执行：主会话（合并 + 验收）**

## 1. 合并内容

| 任务 | 分支 | 模型 | 内容 | 交付文档 |
|---|---|---|---|---|
| P2-0 | （主会话直接提交） | — | 共用 schema：迁移 0012 + 模型 / TS 字段 | [tasks/P2-0-media-schema.md](tasks/P2-0-media-schema.md) |
| P2-1 | `v2-p2-1` | swe-2-max | 图片缩略图 + 每用户存储配额 | [delivery/P2-1.md](delivery/P2-1.md) |
| P2-2 | `v2-p2-2` | grok-4-7-xhigh | 头像上传 + 统一 Avatar 组件 | [delivery/P2-2.md](delivery/P2-2.md) |
| P2-3 | `v2-p2-3` | grok-4-7-xhigh | 语音消息 | [delivery/P2-3.md](delivery/P2-3.md) |
| P2-4 | `v2-p2-4` | swe-2-max | 文件传输助手 + 转发 / 收藏（迁移 0015） | [delivery/P2-4.md](delivery/P2-4.md) |

用户要求只开 2 个 swe-2-max（同时开 4 个时报容量不足），其余用 Grok；Grok 4.7 没有 fast 档，用 `grok-4-7-xhigh`。合并顺序：P2-1 → P2-2 → P2-3 → P2-4。

## 2. 审查与合并时的修改

| 位置 | 修改 | 原因 |
|---|---|---|
| `ChatView.tsx` `onFile` | 先把 `FileList` 拷成数组再清空 `input.value` | V1 起就有的 bug：📎 选择的文件永远进不了待发区（P2-1、P2-4 都报了，合并时取一份） |
| `server/src/api.rs` `upload_file` | 合成一条 INSERT，同时写尺寸、缩略图和语音时长 | P2-1 与 P2-3 各写了一半 |
| `server/src/model.rs`、`app/src/types.ts` | 合并顺序改为 P2-2 字段在前 | 两边都往 `AdminUserView` 末尾加字段，P2-1 还在后面接了新结构体 `StorageView`；按默认顺序合并会把头像字段放进错误的结构体 |
| `tauri.conf.json` CSP | `img-src` 与 `media-src` 都保留 `blob:` | P2-2、P2-3 各加一处，改的是同一行 |
| `ConvList.tsx` | 私聊显示对方头像，文件传输助手显示 🗂 | P2-2 与 P2-4 改的是同一段 |
| `styles.css` | `.card input { flex: 1 }` 不再作用于复选框和单选框 | 桌面端实测发现通知设置里复选框被拉伸到中间、文字挤到右边；这条规则影响所有卡片里的复选框 |
| `scripts/v2/` | 新增 `append-section.mjs`（末尾追加冲突）、`union.mjs` 加 `--theirs-first` | Git Bash 会把以 `/` 开头的参数当路径改写，传注释标记会丢一个斜杠，改为只传任务号 |

## 3. 自动化验证（全新隔离库 55610、后端 18110）

| 检查 | 结果 |
|---|---|
| `cargo fmt --check` | 通过 |
| `cargo test --tests` | it **83 passed** / migrations **2 passed** / 0 failed |
| `npx vitest run` | **134 passed** / 0 failed |
| `npm run build`（tsc + vite） | 通过 |
| `app/src-tauri` `cargo check` | 通过 |
| E2E phase1（先注册专用管理员） | **58 passed** / 0 failed |
| 真实重启后端后 E2E phase2 | **9 passed** / 0 failed |

## 4. 浏览器实测（内置浏览器，两个 origin 各登录一个账号）

| 功能 | 步骤 | 结果 |
|---|---|---|
| P2-1 缩略图 | 粘贴一张 1200×800 PNG（939 KB）发送 | 气泡加载 `?thumb=1`，实际 480×320；加载前已按 240×160 占位 |
| P2-1 配额 | 管理员设 1 MB 后再传 157 KB | 413「存储空间已满（已用 0.9 MB / 上限 1 MB）」；`/users/me/storage` 数值正确；测完恢复不限 |
| P2-2 头像 | 上传 256px PNG | 版本 0→1、`has_avatar=true`、当前版本返回 immutable 缓存头；伪装成 PNG 的 HTML 被 400 拒绝；对方会话列表实时换成 `?v=1` 的头像 |
| P2-3 语音 | 上传 `audio/webm` + `duration_ms=3200` 并经 WS 发送 | `kind=audio`、正文 `[语音]`；对方显示「播放 3″」气泡、列表显示 `[语音]`；`duration_ms` 配 `image/png` 被 400 |
| P2-4 文件传输助手 | 新库注册后查看会话列表 | 有且只有一个「🗂 文件传输助手」；退出它返回 400 |
| P2-4 收藏 | 把收到的图片转发到自己的文件传输助手，再用同一 `client_op_id` 重试 | 只产生 1 条消息；`forwarded_from=accadmin`；缩略图随副本带过去，下载 200 |
| P1-1 回归 | 后台标签页收到消息 | 标题计入未读（第 1 阶段修复仍有效） |

## 5. Tauri 桌面端实测（cua-driver，用户已同意）

调试版客户端（`npx tauri dev`）连接验收后端 18110。

| 检查 | 结果 |
|---|---|
| 登录页填服务器地址后登录 | 通过 |
| 原生窗口标题同步未读数（P1-1） | 显示 `(2) Chat`，打开会话后恢复 `Chat` |
| 通知权限（P1-1） | Windows 下 Tauri 直接为「已允许」，没有授权框 |
| 测试通知 | 进入 Windows 通知中心。本机开着「请勿打扰」，所以只有静默入列、没有弹窗；调试版无安装身份，来源显示在「Windows PowerShell」分组下（正式安装包会显示应用名） |
| 最小化时收到消息（P1-1） | 通知中心出现「Bob — Bob: 桌面通知测试，打开 https://example.com/…」 |
| 任务栏提醒（P1-1） | Chat 图标红色高亮闪烁 |
| 外链（P1-4） | 点消息里的链接，用系统默认浏览器（Edge）打开，应用窗口没有跳转 |
| 麦克风（P2-3） | 点 🎤 后 WebView2 弹出「想要使用麦克风 [阻止] [允许]」；用 ✕ 关闭，没有做选择 |
| 第 2 阶段功能在桌面端 | 缩略图、语音气泡、头像、文件传输助手图标都正常显示 |
| 登录设备名（P1-5） | 当前设备显示为「Chat 桌面端 (Windows)」 |

## 6. 未覆盖项

- 真实麦克风录音：WebView2 权限没有允许，实际录音 → 发送只有 P2-3 Devin 的 Playwright 假设备测试证据。
- Android：没有构建 APK。P2-3 已在 manifest 里声明 `RECORD_AUDIO`、`MODIFY_AUDIO_SETTINGS`，并在交付文档里核对了 wry 的权限转发逻辑。
- 系统通知的横幅弹出：受本机「请勿打扰」影响没看到横幅，只确认了通知中心收到。
- 测试留下的外部痕迹：Edge 里多了一个 example.com 标签页；Windows 通知中心里有两条测试通知。
