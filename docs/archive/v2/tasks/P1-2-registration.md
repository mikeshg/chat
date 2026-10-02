# P1-2 注册开关 + 邀请码 + server_settings

先读 [../devin-rules.md](../devin-rules.md)。迁移号：**`0010_server_settings_invites.sql`**。

## 背景

现在注册完全开放（`server/src/api.rs` 的 `register`，只有 IP 限流）。自部署到公网后陌生人也能注册。管理员需要在后台切换注册模式，并能发邀请码 / 邀请链接。

本任务同时建立**全局可配项的存储** `server_settings`，后续阶段（存储配额、会议地址等）会往里加键，所以接口要好扩展，但本任务只实现 `registration_mode` 一个键。

## 数据

```sql
-- 全局可配项：key → jsonb。缺省键按代码默认值处理（不预置行）。
CREATE TABLE server_settings (
    key        text PRIMARY KEY,
    value      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by uuid REFERENCES users(id) ON DELETE SET NULL
);

CREATE TABLE invites (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       text NOT NULL UNIQUE,
    note       text NOT NULL DEFAULT '' CHECK (char_length(note) <= 100),
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    max_uses   integer CHECK (max_uses IS NULL OR max_uses > 0),  -- NULL = 不限次数
    uses       integer NOT NULL DEFAULT 0,
    expires_at timestamptz,                                         -- NULL = 永不过期
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 审计：通过哪个邀请码注册（邀请码删除不影响用户）
ALTER TABLE users ADD COLUMN invite_id uuid REFERENCES invites(id) ON DELETE SET NULL;
```

## 规则

- `registration_mode`：`"open"`（默认，行不存在即 open，保证现有测试和部署行为不变）/ `"invite"` / `"closed"`。
- **引导例外**：库里还没有任何用户时，无论什么模式都允许注册（首个用户成为管理员的逻辑不变）。判断要和插入在同一事务内，别留竞态窗口。
- `invite` 模式：请求必须带 `invite_code`。在注册事务内原子消费：

  ```sql
  UPDATE invites SET uses = uses + 1
   WHERE code = $1 AND revoked_at IS NULL
     AND (expires_at IS NULL OR expires_at > now())
     AND (max_uses IS NULL OR uses < max_uses)
  RETURNING id
  ```

  0 行 → 403 `邀请码无效或已失效`。用户名冲突等后续失败时事务回滚，次数不被消耗。成功时写 `users.invite_id`。
- `open` 模式忽略 `invite_code`（带了也不校验、不计数）。
- `closed` 模式：403 `管理员已关闭注册`。
- 现有 IP 限流保持在最前面。
- 邀请码：10 位，字母表 `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`（去掉易混淆字符），用 `rand_core::OsRng` 生成；比较时大小写不敏感、去首尾空白（入库统一大写）。

## 接口

公共（无需登录）：

- `GET /v1/server-info` → `{ "registration_mode": "open" | "invite" | "closed" }`。只放可公开的字段，后续阶段会加字段。

管理员（`auth::require_admin`）：

- `GET /v1/admin/settings` → `{ "registration_mode": "..." }`
- `PUT /v1/admin/settings`，body 为部分字段 `{ "registration_mode"?: "..." }`；未知值 400；返回完整设置。
- `GET /v1/admin/invites` → 列表，按 `created_at DESC`：`{ id, code, note, created_by_name, max_uses, uses, expires_at, revoked_at, created_at, usable }`，`usable` 由服务端按「未撤销、未过期、未用完」算出。
- `POST /v1/admin/invites`，body `{ "max_uses"?: number | null, "expires_in_hours"?: number | null, "note"?: string }`；`max_uses` 1–1000，`expires_in_hours` 1–8760，缺省 / null 表示不限；返回新建条目（同上结构）。
- `DELETE /v1/admin/invites/{id}` → 撤销（置 `revoked_at`，幂等），204。

注册：`POST /v1/auth/register` body 增加可选 `invite_code`。

## 服务端实现要点

- 新模块 `server/src/settings.rs`（在 `lib.rs` 注册）：
  - `pub struct ServerSettings { pub registration_mode: String }`（或用 enum，自行取舍），`pub async fn load(executor) -> AppResult<ServerSettings>`：一次查出全部行，缺省键用默认值。
  - 更新函数：按 patch 逐键 UPSERT，记录 `updated_by`。
  - 邀请码的增删查也放这里；路由处理函数放这里还是 `api.rs` 自行决定，但**路由表只在 `api.rs` 的 `router()` 加几行**。
- 设置每次从库读（量极小），不做内存缓存。
- `register` 的改动尽量小：在现有事务里插入用户之前做模式判断与邀请消费。

## 客户端

- `app/src/api.ts`：`serverInfo()`（请求失败或 404 时视为 `open`，兼容旧服务端）、`admin.settings/updateSettings/invites/createInvite/revokeInvite`，`auth.register` 增加可选 `inviteCode`。
- `app/src/types.ts`：对应类型。
- `app/src/ui/AuthScreen.tsx`：
  - 挂载时和「服务器地址」输入变化后（防抖约 500ms）拉 `server-info`。
  - `closed`：隐藏注册入口，显示「管理员已关闭注册」。
  - `invite`：注册表单多一个必填「邀请码」输入框。
  - 打开页面时 URL 带 `?invite=CODE`：自动切到注册模式并预填邀请码（`open` 模式下也切到注册，但不显示邀请码框）。
- 新组件 `app/src/ui/AdminSettings.tsx`，在 `AdminPanel.tsx` 里挂载一行（放在统计卡片之后）：
  - 注册模式单选：开放注册 / 仅限邀请 / 关闭注册，切换即保存，显示保存结果。
  - 邀请码：创建表单（次数：1 次 / 5 次 / 不限；有效期：1 天 / 7 天 / 永久；备注），列表显示码、备注、已用 / 上限、过期时间、状态（可用 / 已用完 / 已过期 / 已撤销）、创建人。
  - 每条可用邀请提供「复制链接」（`${location.origin}/?invite=CODE`；Tauri 端 origin 不是服务器地址，改用 `getServer()` 拼，没有服务器地址时只复制码）和「复制邀请码」「撤销」按钮。复制用 `navigator.clipboard.writeText`，失败时提示手动复制。
  - 所有异步操作要有 busy / 错误状态。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P1-2 =====`，**全部使用 `spawn_named_db` 独立库**（改的是全局设置，会干扰并发跑的其他测试）：

1. `server-info` 无需登录可访问，默认 `open`。
2. 非管理员访问 `/admin/settings`、`/admin/invites` 返回 403。
3. `closed`：第二个用户注册 403；**空库时即使 closed 首个用户也能注册**（用一个全新库，先由某种方式设 closed —— 可直接用 SQL 往 `server_settings` 插行）。
4. `invite`：无码 403、错码 403、有效码 200 且 `uses` +1、`users.invite_id` 被写入；撤销后 403；过期后 403（可直接 SQL 改 `expires_at`）。
5. `max_uses = 1` 时两个请求并发用同一个码注册：恰好一个 200、一个 403。
6. 注册因用户名冲突失败时，邀请码次数不增加。
7. `open` 模式带错误邀请码仍能注册。
8. `PUT /admin/settings` 未知模式返回 400。

Vitest：若把邀请链接拼接、URL 参数解析抽成纯函数，就为它写单测。

浏览器实测（真实执行）：管理员切到「仅限邀请」→ 退出 → 注册页出现邀请码框；用生成的链接打开，邀请码自动填好并注册成功；切到「关闭注册」后注册入口消失。

## 不做

- 邮箱验证、注册审批流。
- 邀请直接加好友 / 进群（群邀请链接不在本轮）。
