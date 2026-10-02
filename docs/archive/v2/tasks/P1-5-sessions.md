# P1-5 自助改密码 + 登录设备管理

先读 [../devin-rules.md](../devin-rules.md)。迁移号：**`0011_sessions.sql`**。本任务改动鉴权核心，务必保证现有全部鉴权 / 撤销测试继续通过。

## 背景

- 用户不能自己改密码：`PATCH /users/me` 只接受 `display_name`（`server/src/model.rs` `UpdateMeReq`）。
- `refresh_tokens.device_label` 字段存在但从没写过；refresh 每次轮换都插入新行、撤销旧行，没有稳定的「一次登录 = 一台设备」标识，所以没法列出设备、也没法只踢某一台。
- 现有撤销机制：`users.session_version`（改密 / 禁用时 +1）写进 access token 的 `sv` claim；`auth::require_user` 和 WS 的 `connection_valid` 每次都比对；`AppState::kick_user` 给该用户**所有**连接发 `force_logout` 并断开。

## 数据

```sql
-- 登录会话（= 一台设备的一次登录）。refresh 轮换沿用同一 session。
CREATE TABLE sessions (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_label text NOT NULL DEFAULT '' CHECK (char_length(device_label) <= 60),
    user_agent   text NOT NULL DEFAULT '' CHECK (char_length(user_agent) <= 300),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz
);
CREATE INDEX idx_sessions_user ON sessions(user_id) WHERE revoked_at IS NULL;

ALTER TABLE refresh_tokens ADD COLUMN session_id uuid REFERENCES sessions(id) ON DELETE CASCADE;
CREATE INDEX idx_refresh_tokens_session ON refresh_tokens(session_id);
```

迁移同时为**现存的有效 refresh token**（未撤销、未过期）各建一条 session 并回填 `session_id`（`device_label` 写「旧设备」）。已撤销 / 过期的行 `session_id` 留 NULL。

## 规则

- 登录、注册各新建一条 session；`issue_tokens` 需要知道 session id（签名怎么改自己定，调用点都要改到）。
- refresh 轮换：新 token 继承旧 token 的 `session_id`，同时更新 `sessions.last_used_at`；session 已撤销则 401。老 token（`session_id` 为 NULL）refresh 时给它补建一条 session。
- access token 增加 claim `sid`（session id，`#[serde(default)]` 兼容老 token）。`AccessGrant` 带上 `sid: Option<Uuid>`。
- `require_user`：`sid` 存在时，session 必须属于该用户且未撤销，否则 401 `session expired, please login again`。和现有查用户的 SQL 合成一次查询，别每个请求多打一轮。
- WS：ticket 记录 `sid`；升级前复核、`connection_valid` 每帧复核都要包含「session 未撤销」。
- 连接表：`AppState.conns` 的每条连接要能知道自己的 `sid`（改 `ConnMap` 的值类型或并行存一张表，自己取舍）。新增能力：
  - 踢掉某用户的某个 session 的所有连接；
  - 踢掉某用户除某个 session 以外的所有连接。
  - 被踢连接照旧先收到 `force_logout` 帧再断开；**只要该用户还有别的连接在线，就不能广播离线**（沿用现有 epoch 逻辑）。
- 登出（`/auth/logout`）：撤销该 refresh token 所属的 session（连带其全部 refresh token）。
- 管理员禁用 / 重置密码：保持现有行为（bump `session_version` + 撤销全部 refresh + 踢全部连接），另外把该用户全部 session 置撤销。

## 接口

- `POST /v1/users/me/password`，body `{ old_password, new_password }`：
  - 旧密码错 → 403 `原密码不正确`（注意别和 401 混，401 会让客户端以为 token 失效去刷新）；新密码长度规则与注册一致（6–72）→ 400。
  - 成功：更新哈希、`session_version + 1`、撤销**其他** session 及其 refresh token、踢掉其他 session 的连接（`force_logout` reason `password_changed`）；为**当前** session 签发新的 access + refresh（`session_id` 不变），返回 `AuthResponse`。
  - 该请求本身限流（复用 `rate_limit`，键 `pw:<uid>`）。
- `GET /v1/users/me/sessions` → `[{ id, device_label, user_agent, created_at, last_used_at, current, online }]`，只列未撤销且仍有未过期 refresh token 的 session，按 `last_used_at DESC`；`current` 由请求的 `sid` 判定；`online` = 该 session 有在线 WS 连接。
- `DELETE /v1/users/me/sessions/{id}`：撤销自己的某个 session（不是自己的 → 404），踢掉其连接（reason `session_revoked`）。撤销当前 session → 400 `当前设备请直接退出登录`。幂等，204。
- `POST /v1/users/me/sessions/revoke-others`：撤销除当前外的全部 session，踢掉对应连接，204。
- 登录 / 注册 body 增加可选 `device_label`（服务端截断到 60 字符，去控制字符）；`user_agent` 取请求头 `User-Agent` 截断到 300 字符。

## 客户端

- `app/src/api.ts`：登录 / 注册带上 `device_label`：Tauri 端 `Chat 桌面端 (Windows)` / `Chat Android`；Web 端由 UA 粗略生成 `Chrome · Windows`、`Edge · Windows`、`Safari · macOS`、`Firefox · Linux` 这类（抽成纯函数测）。新增 `auth.changePassword`、`auth.sessions`、`auth.revokeSession`、`auth.revokeOtherSessions`。
- 改密成功后：`saveSession(新响应)`，然后重建 WS 连接（`stopWs()` + `startWs()`），不要跳回登录页。
- `app/src/ws.ts` 的 `force_logout` 处理：按 reason 显示提示——`disabled`「账号已被禁用，请联系管理员」、`password_reset`「密码已被管理员重置，请重新登录」、`password_changed`「密码已在其他设备修改，请重新登录」、`session_revoked`「该设备已被移出登录，请重新登录」，未知 reason 用「登录已失效，请重新登录」。
- 新组件 `app/src/ui/SecuritySettings.tsx`，在 `MePanel.tsx` 里加一行挂载（放在「连接」卡片之前）：
  - 「修改密码」：原密码 / 新密码 / 确认新密码，前端校验两次一致和长度；成功提示「密码已修改，其他设备已退出」。
  - 「登录设备」：列表显示设备名、UA（小字、截断）、最近活动时间、在线点、「当前设备」标记；非当前设备有「移出」按钮；底部「退出其他所有设备」（二次确认）。
  - 所有异步操作要有 busy / 错误状态。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P1-5 =====`：

1. 同一用户登录两次（设备 A、B）：`GET /users/me/sessions` 返回 2 条，A 视角 `current` 只有 A；带 `device_label` 的登录能看到设备名。
2. refresh 后 session id 不变，`last_used_at` 前进。
3. A 撤销 B：B 的 access token 立即 401、B 的 refresh 401、B 的 WS 收到 `force_logout`（reason `session_revoked`）并被关闭；A 的 WS 不受影响、没有收到离线广播给好友（A 仍在线）。
4. 撤销当前 session 返回 400；撤销别人的 session id 返回 404。
5. 改密：旧密码错 403；成功后返回的新 token 可用；B 的 token / refresh / WS 全部失效；A 用旧 access token 401（`session_version` 已变）；用新密码能登录、旧密码不能。
6. `revoke-others` 后只剩当前 session。
7. 老 token 兼容：直接签发一个不带 `sid` 的 access token（或用迁移前的方式构造）仍能通过 `require_user`（只要 `session_version` 对）。
8. 登出后该 session 从列表消失。
9. 现有测试（`reset_and_disable_revoke_tokens_atomically`、`ws_rechecks_session_version_before_frames_and_preserves_other_connections`、`force_logout_actually_closes_socket`、`refresh_flood_no_pool_starvation` 等）全部保持通过。

Vitest：`device_label` 生成函数；`force_logout` reason → 提示文案函数。

浏览器实测（真实执行）：两个浏览器上下文登录同一账号，在 A 的「登录设备」里看到 B 在线 → 移出 B → B 回到登录页并显示「该设备已被移出登录」；A 改密码后 A 保持登录、B 被踢。

## 不做

- 二次验证（TOTP）、登录通知、异地登录提醒。
- 找回密码（没有邮箱）。
