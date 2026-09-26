# P3-3 ntfy 离线推送 + WS 心跳超时

先读 [../devin-rules.md](../devin-rules.md)。迁移号：**`0018_push.sql`**（要追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

- 手机上的客户端切到后台后，WebSocket 很快会被系统掐断，用户就收不到任何提醒；第 1 阶段的系统通知只在客户端还活着时有用。
- 自部署场景不想依赖 Google 的 FCM。[ntfy](https://ntfy.sh) 是开源的推送服务，可以自建也可以用公共的 ntfy.sh：服务端对 `https://<ntfy服务器>/<主题>` 发一个 HTTP POST，手机上的 ntfy App 订阅这个主题就会弹通知。
- 第 2 阶段 P2-4 已经把所有新消息的广播收敛到 `ws::broadcast_posted(st, conv, &Posted)`（WS 发送、建投票、转发三处共用），推送就挂在这里。
- 服务端现在每 30 秒发一次 WebSocket ping，但从不检查对方有没有回应：手机断网后，服务端可能要很久才发现连接已死，这期间一直认为用户「在线」，也就不会推送。

## 安全前提（重要）

**推送服务器地址只能由管理员配置**，用户只拿到服务端生成的随机主题名。不允许用户填任意 URL，否则服务端会替用户去请求内网地址（SSRF）。

## 数据

```sql
CREATE TABLE push_settings (
    user_id    uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    enabled    boolean NOT NULL DEFAULT false,
    -- 服务端生成的随机主题：chat-<24 位小写字母数字>
    topic      text NOT NULL UNIQUE,
    -- 通知里是否带消息内容；默认不带（公共 ntfy 服务器上主题可被猜到的人看到）
    preview    boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
```

`server_settings` 新键（在 `settings.rs` 的 `ServerSettings` 里加字段；同阶段 P3-2 也会加 `meeting_base_url`，各加各的）：

| 键 | 说明 |
|---|---|
| `push_ntfy_base` | ntfy 服务器地址，如 `https://ntfy.sh`；空 = 推送功能关闭（默认） |
| `push_ntfy_token` | 可选的 ntfy 访问令牌（自建且开了鉴权时用）；**只写不读**：GET 设置只返回 `push_ntfy_token_set: bool` |
| `public_url` | 可选，本聊天服务的对外地址，用于通知的点击跳转 |

## 规则

### 何时推送

在 `broadcast_posted` 里，对 `posted.inserted == true` 的新消息，对每个成员 m（跳过发送者）：

1. m 开启了推送，且服务器配置了 `push_ntfy_base`；
2. m **没有活跃连接**（见下文「活跃」）；
3. 会话没被 m 静音——除非这条消息 @了 m；
4. 会话被 m 归档时只推 @m 的消息；
5. 合并：同一 (m, 会话) 30 秒内只推一次（内存里记最近推送时间，`DashMap<(Uuid, Uuid), Instant>`；用 `// ponytail:` 注释写明是单实例内存状态、重启丢失，可接受）。

满足条件就 `tokio::spawn` 发送，**绝不能阻塞或拖慢消息广播**。

### 发送内容

- `POST {push_ntfy_base}/{topic}`，5 秒超时，失败只记日志，不重试。
- 头：`Title`（群名；私聊为发送者昵称）、`Tags: speech_balloon`、`Priority`（@我 时 `high`，否则 `default`）、有 `public_url` 时 `Click: {public_url}`、有 token 时 `Authorization: Bearer {token}`。
- 正文：`preview` 开时 `发送者: 摘要`（摘要规则与客户端通知一致：图片 `[图片]`、文件 `[文件] 名称`、语音 `[语音]`、投票 `[投票] 问题`、文本前 100 字）；关时 `你收到一条新消息`（@我 时 `有人@了你`）。
- 标题和正文都是 UTF-8；ntfy 的头部不支持非 ASCII 时按 ntfy 文档用 RFC 2047 编码，或改用 ntfy 的 JSON 发布接口（`POST {base}` + JSON body `{topic, title, message, priority, tags, click}`）。**建议直接用 JSON 接口**，避免头部编码问题。

### 活跃连接

- WS 入站新帧 `client_state { active: bool }`：客户端在连接建立时、`visibilitychange`、`focus` / `blur` 时上报（页面可见且窗口有焦点 = active）。
- 每条连接记录自己的 active 状态（加到 `state.rs` 的 `Conn` 里），**默认 true**（兼容不上报的旧客户端）。
- 「用户有活跃连接」= 至少一条连接 active。

### 心跳超时

- `ws.rs` `handle_session`：记录最近一次收到任何入站帧（含 Pong）的时间；在 ping 的定时分支里检查，超过超时时间就关闭连接，走正常的断开清理（离线广播等照旧）。
- 超时时间默认 75 秒（2.5 个 ping 周期）。为了能测试，在 `AppState` 里放一个可调的 `ws_idle_timeout_ms: AtomicU64`（不要加到 `Config`），测试用 `spawn_named_state` 拿到 state 后改短；ping 周期也要能跟着改短（同样放 `AppState`）。

## 接口

- `GET /v1/users/me/push` → `{ available, enabled, preview, subscribe_url }`：`available` = 服务器配了 ntfy；`subscribe_url` = `{push_ntfy_base}/{topic}`（首次请求时生成主题）。
- `PUT /v1/users/me/push`，body `{ enabled?, preview? }` → 同上。服务器未配置时 `enabled=true` 返回 400「管理员未配置推送服务」。
- `POST /v1/users/me/push/rotate` → 重新生成主题，返回同上（旧主题作废，用户要在 ntfy App 里重新订阅）。
- `POST /v1/users/me/push/test` → 立即发一条测试推送（不受 30 秒合并限制），200 / 失败时 502 带原因。
- `GET/PUT /v1/admin/settings` 增加 `push_ntfy_base`（http/https、≤ 200 字符，空 = 关闭）、`push_ntfy_token`（写入；传空字符串 = 清除）、`public_url`（同样的地址校验）；GET 返回 `push_ntfy_token_set` 而不是令牌本身。
- `/v1/server-info` 增加 `push_available: bool`。

## 客户端

- `app/src/ws.ts`：连接建立后和 `visibilitychange` / `focus` / `blur` 时发送 `client_state`（值 = `!document.hidden && document.hasFocus()`），去抖约 500ms。
- 新组件 `app/src/ui/PushSettings.tsx`（`MePanel.tsx` 加一行挂载，放在「通知」卡片后面）：
  - 服务器未配置：显示「管理员未配置推送服务」。
  - 开关「手机推送」、开关「推送显示消息内容」（旁边小字提示：公共 ntfy 服务器上内容可能被他人看到）。
  - 开启后显示订阅地址 + 「复制」按钮 + 简短说明：「在手机上安装 ntfy App → 订阅主题 → 粘贴这个地址」。
  - 「发送测试推送」「更换主题」（二次确认）按钮，都有 busy / 错误状态。
- `AdminSettings.tsx`：增加「推送服务（ntfy）」一栏：服务器地址、访问令牌（输入框 placeholder 显示「已设置」/「未设置」，不回显）、对外访问地址。

## 允许新增的依赖

`server/Cargo.toml` `[dependencies]`：`reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }`（开发依赖里已有 reqwest，保持不变）。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P3-3 =====`，**全部用 `spawn_named_state` 独立库**（改的是全局设置）。测试里用 axum 起一个假的 ntfy 服务器（监听 127.0.0.1:0，把收到的请求 body 和头记进 `Arc<Mutex<Vec<...>>>`），把 `push_ntfy_base` 指向它：

1. B 开启推送、B 没有 WS 连接 → A 在私聊发消息 → 假服务器收到 1 条，topic 正确、标题是 A 的昵称、正文不含消息内容（preview 默认关）。
2. B 有一条 active 连接 → 不推送；B 发 `client_state {active:false}` 后 → 推送。
3. B 静音了会话 → 普通消息不推；群里 @B 的消息推送（Priority high）。
4. 30 秒合并：连发 3 条只推 1 条。
5. preview 开 → 正文是「A: 内容」。
6. 服务器未配置 → 开启推送 400、`available=false`，发消息不产生任何请求。
7. 管理员设置：非管理员 403；`push_ntfy_token` 在 GET 里不出现，只有 `push_ntfy_token_set`；非法地址 400。
8. `rotate` 后主题变化，旧主题不再收到推送。
9. 心跳：把超时调到 1 秒、ping 周期调到 200ms，用一个**不读取帧**（因此不回 Pong）的原始 WS 客户端连上 → 1–3 秒内服务端关闭连接，好友收到该用户的离线 presence；正常读帧的客户端不会被断开。
10. 推送发送失败（假服务器返回 500 或连接拒绝）不影响消息正常送达。

Vitest：`client_state` 上报的去抖 / 取值函数；推送设置里订阅地址展示与复制的纯逻辑（如有）。

浏览器实测（真实执行，截图放 `server/target/wt/`）：管理员在后台填假 ntfy 地址（可用一个本地 node 小服务记录请求）→ 用户在「我」页开启推送并看到订阅地址 → 关掉用户页面 → 另一账号发消息 → 本地小服务收到推送。

## 不做

- Web Push（浏览器 Service Worker 推送）、FCM、APNs、UnifiedPush 分发器集成（App 内接收）。
- 推送重试队列、推送送达统计。
