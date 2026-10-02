# A08 AgentAccess：管理员开关与逐用户授权

对应 Gitea issue xiefan/chat#9（先看 issue 里 A01 审查备注）。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q21、Q22）、`CONTEXT.md` 的 AgentAccess 词条与「使用权」规则、`docs/adr/001-decisions.md` §11（禁用即踢的做法）、[../delivery/A01.md](../delivery/A01.md)、[../delivery/A04.md](../delivery/A04.md)。迁移号：**`0025_agent_access.sql`**。

## 背景

管理员要能控制谁能用 Agent：全局开关 + 全局默认（允许 / 禁止）+ 逐用户覆盖；管理员自己始终可用。另外 A01 审查发现：宿主连接不在普通连接表里，账号被禁用、登录会话被移出、改密码踢设备时，宿主 WS 不会断开——本票一起修。

同批并行：A03、A11、A09。

## 一、数据（迁移 0025）

- `server_settings` 加 `agent_enabled boolean NOT NULL DEFAULT true`、`agent_default_allowed boolean NOT NULL DEFAULT true`（沿用现有 ServerSettings 的存取方式，不加 Config 字段）。
- `users` 加 `agent_access boolean NULL`（NULL = 跟随全局默认）。

## 二、服务端

- 生效规则（写成一个函数，所有检查点复用）：管理员 → 可用；否则全局关 → 不可用；否则 `users.agent_access` 有值用它，没值用全局默认。
- 检查点：
  - 所有 Agent 相关 REST（宿主列表、Agent 增删改查、头像上传、probe、调用历史、取消、上下文重置、个人设置）→ 无权时 403 `agent_access_denied`（头像 GET 不鉴权，保持不变）；
  - 宿主 WS（`role=host`）→ 无权时握手后立即关闭，不登记；
  - 派发：无权用户的消息不产生调用（AgentConversation 里发消息 → 400 `agent_access_denied`，不落库；群/私聊里 @ 只是不触发）；
  - 宿主上线清排队时跳过无权用户。
- 使用权被收回时（管理员改全局开关 / 默认 / 该用户覆盖，账号被禁用）：该用户进行中（`queued` / `running` / `awaiting_approval`）的调用改为 `cancelled`、`error='agent_access_revoked'`，推帧；给宿主发取消；断开该用户的宿主连接。数据保留，恢复后照常。
- **宿主连接跟随登录会话**（A01 审查备注）：宿主连接登记时记下它用的登录会话（ticket 里的 sid）。以下情况断开对应宿主连接：账号被禁用（全部）、`DELETE /users/me/sessions/{id}` 移出某个会话（该会话的宿主）、改密码踢其他设备（除当前会话外的宿主）、恢复备份（全部）。复用现有的踢人路径（`kick_other_sessions` 等），不要另写一套。
- 管理接口：`GET/PUT /v1/admin/settings` 增加 `agent_enabled`、`agent_default_allowed`；`PUT /v1/admin/users/{id}/agent-access`，body `{ "allowed": true | false | null }`；`GET /v1/admin/users` 的条目增加 `agent_access`（覆盖值）和 `agent_access_effective`。
- `GET /v1/users/me` 增加 `agent_access: bool`（生效值），客户端据此显示或隐藏入口；使用权变化时给该用户推 `Invalidate { what: "me" }`（没有这个值就加上，前端收到后重拉 `/users/me`）。

## 三、前端

- 「我」页的 Agent 入口、会话列表里的 AgentConversation 输入框：`agent_access=false` 时隐藏入口，专属会话显示「管理员已关闭你的 Agent 功能」并禁用输入；@ 补全不列 Agent。
- 管理后台设置页加「允许使用 Agent」「新用户默认可用」两个开关；用户列表每行加 Agent 使用权下拉（跟随默认 / 允许 / 禁止），显示生效值。
- 桌面端：`agent_access=false` 时不启动宿主（已启动的停掉）。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A08 =====`，**用 `spawn_named_db` 独立库**（改全局设置）：

1. 全局关：普通用户 Agent REST 全部 403、宿主连不上、AgentConversation 发消息 400；管理员照常可用。
2. 默认禁止 + 某用户覆盖允许：该用户可用，其他普通用户 403；覆盖改成 null 后跟随默认。
3. 收回使用权时：`running` 的调用变 `cancelled/agent_access_revoked`，宿主收到取消并被断开；`queued` 的也被取消；恢复后可再建调用。
4. 账号被禁用：宿主连接被断开（`agent_host_status online=false`）。
5. 移出宿主所用的登录会话 → 该宿主断开；改密码 → 其他会话的宿主断开、当前会话的宿主保留。
6. `/users/me` 的 `agent_access` 随设置变化；变化时收到 `invalidate`。

浏览器实测（无头 Playwright）：管理员关闭全局开关后，普通用户刷新后「我」页没有 Agent 入口、专属会话输入框禁用；截图放 `server/target/wt/`。

## 不做

- 按 Agent 或按宿主的细粒度授权；配额 / 用量统计。
