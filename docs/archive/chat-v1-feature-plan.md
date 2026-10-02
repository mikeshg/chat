# Chat V1 功能迭代设计与 Devin 执行计划

**设计日期：2026-09-24；契约整理日期：2026-09-25**

**基线：当前工作区（包含已有未提交的 P1/P2 修复，不 reset、不覆盖）**

任务总览与阶段进度见 [progress.md](progress.md)，2026-09-25 当前 Qoder 会话的实际集成验收见 [delivery-D.md](delivery-D.md)，结论为 NEEDS_CHANGES。本文件描述产品目标和技术契约，不代表全部验收已通过；A/B/C 已有阶段交付，原 Devin D 未恢复。第 8 节是精确约束，第 9 节对照当前实现并指出尚未满足的设计要求。

## 1. 产品目标

把当前“能稳定私聊/群聊”的基础能力升级为“适合熟人和小团队长期使用”的聊天产品。本轮只做高频、强反馈、与现有领域模型贴合的能力：

1. 消息生命周期：回复/引用、编辑、软删除、emoji reactions。
2. 会话控制：按用户的置顶、静音、归档偏好。
3. 群协作：轻量投票。

不做 E2EE、实时音视频、Stories、公共频道、Bot 平台、AI 总结、复杂工作流；这些需要单独的产品路线和基础设施，不和本轮混在一起。

## 2. 设计原则

- **保持消息 seq 不变**：编辑、删除、reaction、投票都不能重新分配消息序号。
- **事件可重放**：所有 WS 事件带实体 id、会话 id 和版本/seq；重复事件必须幂等。
- **旧客户端可用**：新增字段全部可选；旧客户端至少能看到普通文本占位，不因未知 kind 崩溃。
- **服务端强制权限**：客户端隐藏按钮不算权限控制。
- **软删除优先**：保留消息 id、seq、回复关系和审计时间，避免同步/搜索/投票关系断裂。
- **当前未提交改动优先级最高**：所有 Devin 必须先读取工作区，不得 reset、覆盖或提交他人改动。

## 3. 功能设计

### 3.1 消息回复 / 引用

#### 行为

- 任意可访问会话中的成员都可以回复一条消息。
- 回复消息展示被引用消息的发送者、前 120 个字符和附件/投票摘要。
- 被引用消息已删除时展示“原消息已删除”，但保留跳转定位能力。
- 点击引用跳转到原消息；若本地缺失，按 seq 拉取上下文。

#### 数据

在 `messages` 增加：

- `reply_to_id uuid NULL REFERENCES messages(id)`

`MessageView` 增加可选：

- `reply_to_id`
- `reply_to_preview`

#### 协议

现有 `send_message` 入参增加可选 `reply_to_id`。服务端在同一事务内验证被引用消息属于同一会话且当前用户有访问权。

### 3.2 消息编辑

#### 行为

- 仅消息发送者可以编辑普通文本消息。
- 编辑窗口：发送后 15 分钟内；附件、投票、已删除消息不可编辑。
- 编辑后保留 `edited_at`，客户端显示“已编辑”。
- 编辑必须产生新的 WS 事件，但不改变原消息 seq/id。

#### 数据

在 `messages` 增加：

- `edited_at timestamptz NULL`
- `edit_version integer NOT NULL DEFAULT 0`

#### API / WS

本文件 REST 路径均位于 `/v1` 下。

- REST：`PATCH /messages/{id}`，body `{content, expected_version, client_op_id?}`。
- WS：`edit_message {message_id, content, expected_version, client_op_id?}`。
- 广播：`message_updated {conversation_id, message, edit_version}`，`message` 是完整、按接收者计算的 `MessageView`。
- 使用 `expected_version` 与当前 `edit_version` 做乐观并发控制；不匹配返回 409/conflict，不覆盖新内容。
- 当前网页 UI 使用 REST；服务端同时支持上述 WS mutation。`client_op_id` 与发送消息的 `client_msg_id` 分离，具体重试边界见第 9 节。

### 3.3 消息软删除 / 撤回

#### 行为

- 普通成员可以删除自己的消息。
- 群主可以删除群内任意消息；DM 中只能删除自己的消息。
- 删除不物理移除记录，所有成员看到“消息已删除”。
- 删除后附件不再可访问，异步清理附件文件；消息的 seq、id、reply_to 关系保留。

#### 数据

在 `messages` 增加：

- `deleted_at timestamptz NULL`
- `deleted_by uuid NULL REFERENCES users(id)`

删除后响应中的敏感内容统一清空，但保留 `id/seq/sender_id/created_at`。

#### API / WS

- REST：`DELETE /messages/{id}?client_op_id=`。
- WS：`delete_message {message_id, client_op_id?}`。
- 广播：`message_deleted {conversation_id, message, edit_version}`，携带完整墓碑 `MessageView`，不能只发送删除时间等部分字段。
- REST 同样返回完整 `MessageView`；消息删除重复执行不再递增版本。

### 3.4 Emoji reactions

#### 行为

- 每个用户对同一消息的同一 emoji 只能有一条 reaction。
- 同一用户每条消息最多 20 种 emoji；单个 emoji 字符串最大 32 字节。
- 删除消息时级联删除 reactions。
- 旧客户端忽略 reaction 事件，消息正文仍可正常显示。

#### 数据

新增表：

```sql
message_reactions(
  message_id uuid REFERENCES messages(id) ON DELETE CASCADE,
  user_id uuid REFERENCES users(id) ON DELETE CASCADE,
  emoji text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY(message_id, user_id, emoji)
)
```

#### API / WS

- `PUT /messages/{id}/reactions/{emoji}`
- `DELETE /messages/{id}/reactions/{emoji}`
- WS：`reaction_upsert` / `reaction_remove`
- REST 返回完整 `MessageView`；广播为 `reaction_update {conversation_id, message, edit_version}`，同样携带完整视图。
- `message.reactions` 包含聚合后的 `{emoji, count, reacted_by_me}`；`reacted_by_me` 必须按每个接收者计算，不得全群复用发起者视图。
- 实际增加/移除才提升 `edit_version`，重复同状态操作不提升版本。

### 3.5 会话偏好：置顶 / 静音 / 归档

#### 行为

偏好是用户维度，不改变会话本身：

- 置顶：会话列表固定在顶部。
- 静音：支持 1 小时、今天、永久；静音不影响 @mention 强提醒策略。
- 归档：从默认列表隐藏，但收到新消息时不自动恢复，除非用户主动打开或被 @mention。

#### 数据

新增表：

```sql
conversation_preferences(
  conversation_id uuid REFERENCES conversations(id) ON DELETE CASCADE,
  user_id uuid REFERENCES users(id) ON DELETE CASCADE,
  pinned_at timestamptz NULL,
  muted_until timestamptz NULL,
  mute_forever boolean NOT NULL DEFAULT false,
  archived_at timestamptz NULL,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY(conversation_id, user_id),
  CHECK (NOT mute_forever OR muted_until IS NULL)
)
```

#### API

- `GET /conversations/preferences` 返回本人的 `{conversation_id, preference}` 列表。
- `PUT /conversations/{id}/preferences` 返回更新后的 `ConversationPref`。
- body 为 `{pinned?, archived?, mute?:{mode:'none'|'until'|'forever', until?}}`；未提供字段保持不变。
- 会话列表直接返回嵌套 `preference`，避免客户端为每个会话 N+1 请求。
- WS `preference_updated {conversation_id, preference}` 只发本人的所有连接。当前客户端没有 `updated_at` 乱序守卫，不能据此宣称跨设备并发已完全收敛。

### 3.6 轻量投票

#### 行为

- 仅群聊支持创建投票。
- 创建者可设置：问题、2–10 个选项、单选/多选、是否匿名、截止时间。
- 每个用户只能提交一份当前投票结果；重复提交采用幂等更新。
- 群主或创建者可关闭投票。
- 投票关闭后只读，结果仍可查看。

#### 数据

新增：

- `polls(message_id PK/FK, question, allow_multiple, anonymous, closed_at, expires_at, version)`
- `poll_options(id uuid PK, poll_id, option_index, label)`，唯一约束 `(poll_id, option_index)` 与 `(poll_id, id)`。
- `poll_votes(poll_id, option_id, user_id, created_at)`，主键 `(poll_id, option_id, user_id)`，复合外键 `(poll_id, option_id)` 保证选项归属于该投票。

消息使用 `kind='poll'`，旧客户端显示 content 中的 `[投票] question` fallback。`MessageView.poll` 包括题目、选项计数、`total_voters`、本人 `my_option_ids`、关闭/截止状态和投票版本；匿名投票不返回其他投票者身份。

#### API / WS

- `POST /conversations/{id}/polls`：创建，body `{question, options, allow_multiple, anonymous, expires_at?, client_msg_id?, client_op_id?}`。
- `PUT /messages/{id}/poll/votes`：替换本人选择，body `{option_ids, client_op_id?}`；空数组表示撤票。
- `POST /messages/{id}/poll/close?client_op_id=`：创建者或群主关闭，重复关闭不提升版本。
- 三个 REST 接口返回发起者视角的完整 `MessageView`。创建复用 `message` 广播；投票/关闭复用完整视图的 `message_updated` 广播。
- **不新增** `create_poll/vote_poll/close_poll` WS 入站指令或 `poll_created/poll_updated/poll_closed` 事件；这是 C2 执行时收敛后的契约，不沿用早期草图中的独立帧设计。
- 所有投票变更在事务内按 conversation → message → poll 加锁，重新检查成员资格、关闭、过期和墓碑状态。
- 真正投票变化或首次关闭同时递增 `polls.version` 与 `messages.edit_version`，不改变 `seq`；reaction 只提升消息版本，因此两个版本不要求数值相等。

## 4. 客户端设计

### 状态模型

`MessageView` 扩展 `reply_to_id`、`reply_to_preview`、`edited_at`、`edit_version`、`deleted_at`、`reactions` 与 `poll`；前端对旧数据缺省版本按 0 处理。

- 会话偏好嵌套在 `ConversationView.preference`，不另建重复的 `conversationPreferences` 数据源。
- 消息缓存由 store 按 id 与 `edit_version` 合并；mutation 和缓存刷新共用完整视图处理，`last_message` 也需版本保护。
- 回复目标、编辑草稿、提交 busy/error、投票创建草稿由 store 与对应组件状态承担，不要求照搬早期草图的字段名。
- 新消息发送沿用内存 outbox；编辑、删除、reaction、偏好、投票由 UI 调用 REST，当前没有独立 mutation outbox。
- 会话目录、实时消息、ACK 的所有接入路径都应满足同一版本不回退约束；当前缺口见第 9 节及进度文档。

### UI

- 消息 hover/context menu：回复、编辑、删除、reaction。
- 消息底部显示 reactions 聚合条。
- 回复时 composer 上方显示引用卡片，可取消。
- 编辑时复用 composer，显示“正在编辑”。
- 群聊新增投票创建弹窗和投票卡片。
- 会话列表增加置顶、静音、归档菜单和视觉状态。

### 失败与恢复

- `client_op_id` 与 `client_msg_id` 分别关联 mutation 与新消息；UUID、幂等存储范围和未知结果重试必须按第 9 节区分，不能把“接受一个操作 ID”当成永久去重保证。
- 新增 mutation 先采用服务器确认后更新，失败保留行内错误与可重试入口；不新增乐观回滚系统。原有新消息发送保留乐观 outbox。
- 收到旧版本 `message_updated` 不覆盖新版本；删除/reaction/投票更新也采用完整视图。
- 重连先刷新目录和本人偏好，再按连续 seq 分页补新消息，并按已缓存 message ids 每批最多 100 条回读历史 mutation。
- 缓存刷新遇到无权限或会话不存在时清理对应缓存。真实断网、乱序与账号切换仍需集成验收，不能以 reducer 测试代替。

## 5. Devin 分工与执行顺序

由于当前工作区有大量未提交修复，**不并发修改同一 checkout**。使用多个 Devin，但按阶段串行交接，所有人共享当前工作区并读取本设计文档。

### Devin A：消息生命周期

负责：

- migrations、Rust model/domain/api/ws/state/tests
- `reply_to_id`、编辑、软删除、reactions
- 前端 `types.ts`、`api.ts`、`ws.ts`、`store.ts`、`ChatView.tsx`
- 兼容现有多设备 seq、session generation、outbox
- 新增服务端回归测试和基础 UI 行为

禁止修改：会话偏好、投票 schema/UI，除非为共享类型增加可选字段。

### Devin B：会话偏好

负责：

- `conversation_preferences` migration/model/API
- 会话列表返回偏好
- 置顶、静音、归档 UI
- 通知策略与当前 unread/mention 逻辑的兼容
- 偏好 API、权限和重连恢复测试

前置条件：Devin A 已 idle，且其改动已通过 cargo test + tsc。

### Devin C：投票

负责：

- polls/options/votes migrations、事务和权限
- WS/REST 协议与旧客户端 fallback
- 群聊投票创建、投票、关闭 UI
- 并发重复投票、关闭后写入、过期处理测试

前置条件：Devin B 已 idle，且完成基础验证。

### Devin D：集成审查

只负责：

- 检查三阶段 diff 是否覆盖设计
- 解决跨模块冲突、类型不一致、协议回归
- 运行全量测试、隔离 E2E、前端 build
- 不自行扩展新功能，不重写已有模块

## 6. 验收标准

### Server

- `cargo fmt --check`
- `cargo check --locked --all-targets`
- `cargo test --locked`，包括现有 18 个回归测试和新增测试
- 迁移从空库、当前历史库均可执行
- 旧客户端发送普通 `send_message` 仍可用

### Client

- `tsc --noEmit`
- Vite production build
- 现有 E2E phase1/phase2 全通过
- 新增手工/自动验证：
  - 回复目标删除后仍有占位
  - 编辑版本乱序不会覆盖新内容
  - 删除权限和附件清理
  - reaction 幂等与重连恢复
  - 置顶/静音/归档跨设备同步
  - 投票重复投票、关闭后投票、匿名结果

### 安全与数据

- 不能绕过会话成员资格或群权限修改消息/投票。
- 所有新 mutation 要有幂等键或数据库唯一约束。
- 不把测试账号、token、临时脚本提交进 Git。
- 不执行 `git reset`、不删除当前已有未提交文件、不自动 commit/push。

## 7. 暂不纳入本轮

E2EE、实时音视频、Stories、公共频道、Bot/插件平台、AI 总结、链接预览抓取服务、服务端草稿同步。这些功能需要单独立项和架构评审。

## 8. 实施细则与验收补充（优先于上文概念草图）

### 8.1 版本与断线修复

消息 seq 只标识创建顺序，不能当编辑/删除/reaction 的变更游标。本轮采用最小方案，不新增通用事件存储框架：
- 在线变更帧携带消息 id 与单调 edit_version（每次编辑、软删除和 reaction 实际变更均递增）；旧版本不得复活墓碑或覆盖新版本。
- 重连后保留现有新消息补拉，同时批量刷新本地已缓存的 message ids（每批最多 100），从当前数据库读回正文、墓碑、reaction、reply preview；未授权会话直接清除本地缓存。不逐条 N+1 请求。
- HTTP/WS sync/history/ACK 均通过同一套按 id+version 合并规则；无版本的旧服务端消息按 0 处理。
- REST 编辑请求必须携带 expected_version；WS 编辑同样携带 expected_version。版本冲突返回 409/conflict，不覆盖。
- client_op_id 使用 UUID；REST 新 mutation 同样支持 client_op_id。操作结果未知时只能用原 id 重试，不能用新 id 重发。
- 最小 mutation 回执必须能区别 client_op_id 与 client_msg_id，不得将消息发送的 outbox 错误地消费成编辑操作。
- 客户端网络状态不明时保留可重试提示；先采用服务器确认后更新，避免新增一套未经测试的乐观回滚机制。

### 8.2 消息与安全

- 所有 mutation 在会话行锁内重新检查成员资格；删除好友后的 DM 为只读，回复、编辑、删除、reaction 均不可写。
- 编辑 15 分钟按服务端时间判定，content 限制沿用 1–4000 字符；只允许普通文本。编辑重新解析 mentions，不产生新的 unread 消息。
- 删除不改 message seq/last_seq，不倒退 read cursor；search 和 reply preview 不返回已删除正文。tombstone 对旧客户端返回无敏感内容的文本占位。
- 删除事务使附件下载失效；历史重复引用指向同一 storage_path 时，不得删掉仍被其他有效 attachment 引用的物理文件。已被对方下载的内容和浏览器旧缓存无法收回，UI 不宣称远程抹除。
- reply 只能指向同会话；引用链只展开一层。返回 message_id+seq 供上下文定位；原消息删除后所有 reply preview 也需更新。
- reaction 的用户维度字段必须个性化输出：不能把发送者的 reacted_by_me 广播给所有用户。离线恢复、重复 add/remove、并发两种 emoji 的版本回归必须测试。

### 8.3 会话偏好精确定义

- 在会话列表中增加 preference 字段（旧客户端可忽略）；不用单独 GET preferences 做重复列表请求。
- PUT /conversations/{id}/preferences body：pinned?:boolean, archived?:boolean, mute?:{mode:'none'|'until'|'forever',until?:RFC3339}。缺省字段不改，null 不作为 forever；非法时间/未知 mode 返回 400。
- 数据库存 mute_forever boolean NOT NULL DEFAULT false；muted_until NULL 只表示无定时静音。until 必须在未来；客户端“今天”指本地下一午夜再转 UTC，1 小时按当前时刻加 60 分钟。
- 置顶排序 pinned_at DESC，然后原活动时间 DESC、id；未置顶保持原顺序。归档是独立 tab/filter；点击归档会话即取消本人归档；新普通消息不取消，新的 @本人消息取消本人的归档。
- pinned/mute/archive 仅本人可见且同步本人全部设备；新增 preference_updated 帧只发给当前 uid 的连接。会话被解散/移除后不能再改偏好。
- 静音只抑制应用内通知提示（可附浏览器 Notification，但仅用户主动授权后）；未读数与 mention 标记照常更新，@本人可突破静音。当前不承诺 Android 后台推送、系统托盘或通知中心历史。
- 若没有现成通知出口，实现一个最小应用内提示，点击打开会话；不自动申请浏览器权限，不引入外部推送服务。

### 8.4 投票精确定义

- 仅有效群成员可创建；问题 1–200 字符，2–10 个去首尾空白后不同的选项，每项 1–100 字符；选项创建后不可修改，避免改题污染已投票结果。
- 创建走现有 post_message 的单事务 seq/idempotency，kind='poll'，content='[投票] 问题'；普通 send_message 不能伪造 poll 类型绕过 schema。
- polls: message_id PK/FK、allow_multiple、anonymous、closed_at、expires_at（可空）、version bigint default 0。
- poll_options: id uuid PK、poll_id FK、option_index、label，UNIQUE(poll_id,option_index)。poll_votes 必须以复合外键或事务校验证明 option 属于同一个 poll。
- vote 为“替换本人选择集合”，不是累加；单选至多 1 项，多选至多选项数；空集合表示撤票。闭票/过期/软删除后一律拒绝投票。
- 过期按数据库 now() 在读取/写入时计算，不要求后台定时器；客户端到期立即禁用并刷新。
- 创建者或群主可关票；重复关票幂等。成员资格与 poll 行锁必须在事务内校验（会话行锁 → 消息/poll 行锁），锁顺序与 A 一致。
- 返回 option counts、total_voters、my_option_ids、version；匿名投票不得返回其他投票者 id/name（含 WS、错误信息），非匿名可返回 user 摘要。所有 personalized 字段逐用户计算。
- poll version 乱序保护，重连刷新复用 A 的批量消息刷新；poll_closed/updated 不更改 message seq、不增加未读。
- REST 与 WS 至少共用领域函数；UI 可以用 REST 发起 mutation，WS 用于同步，避免在两端重复业务。

### 8.5 迁移与持久化设计

- A 从 0007 起、B 接续下一个空号、C 再接续；不修改已执行的 0001–0006，不通过删除/重建已有库解决 checksum。
- 本轮没有用户授权的破坏性数据整理；重复附件兼容须保留历史消息能访问的已有附件，发现旧迁移数据损失风险时记录并报告。
- 每个阶段交付 docs/archive/delivery-{A,B,C,D}.md：真实实现契约、测试命令/执行数、未覆盖项、下一阶段注意事项；没有运行的验证明确写“未运行”。不得把 token/password/state JSON 写到文档。
- P1/P2 上轮“18/18”仅是既有测试基线，不代表所有客户端竞态已覆盖。新增功能不能以此宣称客户端行为通过。
- 本轮只落实第 1 节的功能；消息收藏、全局通知中心/Android 推送、高级搜索、threads、邀请链接、语音消息、定时发送列入下一批，不偷偷插入当前范围。

### 8.6 多 Devin 交接控制

- 只有当前阶段的一个 Devin 可以写实现文件；后续阶段只在前一阶段 idle 且通过主 agent 的检查后启动。
- 主 agent 负责本设计文档和任务状态，执行 Devin 不整体重写设计；约定变更在 delivery 文档记录。
- 所有 execute 权限须核对实际命令；不使用 bypass、不代替用户批准未知/破坏性动作。只清理自己本次创建且能证明归属的临时文件/资源。
- UI 保持现有视觉系统，键盘可操作，手机宽度不溢出，所有异步提交有 busy/error 状态。

## 9. 当前实现契约与设计差距（2026-09-25）

本节依据当前源码核对，优先于早期概念草图；实现差距不是放弃原验收要求。详细风险、证据和后续步骤统一维护在 [progress.md](progress.md)。

### 9.1 数据与读模型

| 迁移 | 数据变更 | 状态边界 |
|---|---|---|
| `0005_sync_invariants.sql` | 发送幂等键、附件唯一引用、解散标记、`message_views` | 历史重复附件引用会被置空，升级数据保留问题尚未收敛 |
| `0006_session_version.sql` | `users.session_version` | 既有鉴权修复基线，不代表当前全套安全验收通过 |
| `0007_message_lifecycle.sql` | 回复、编辑/删除字段、最近一次 `edit_op_id`、`message_reactions` | 无通用操作日志或完整编辑历史表 |
| `0008_conversation_preferences.sql` | 用户维度偏好及永久/定时静音互斥约束 | `updated_at` 当前未用于客户端乱序保护 |
| `0009_polls.sql` | `polls/options/votes`、选项归属复合外键、视图末尾追加 `poll` | 过期依赖数据库读取/写入时的时间判断，不自动发到期事件 |

`message_views` 聚合回复、表态和投票数据；返回给客户端时按 viewer 计算 `reacted_by_me`、`my_option_ids`，匿名投票剥离其他 voter 身份。墓碑清除可见正文、附件、提及和投票数据，保留消息定位字段。视图中新列只能按兼容要求追加；空库与历史库迁移仍待 D 分别验收。

### 9.2 写入、权限与幂等

| 操作 | 当前实现 | 不应宣称的保证 |
|---|---|---|
| 新消息/投票创建 | `(conversation_id, sender_id, client_msg_id)` 唯一键；投票复用消息事务 | 没有持久化客户端 outbox；创建 UI 失败重试未固定原 ID |
| 消息编辑 | 作者、文本、15 分钟、`expected_version`；保存最近一次 `edit_op_id` | 不是可永久重放的操作账本；需复核幂等捷径与权限校验顺序 |
| 删除/reaction | 墓碑或唯一约束实现状态幂等；真实变化才升消息版本 | 删除/reaction 的 `client_op_id` 不持久化 |
| 偏好更新 | 按显式字段 UPSERT，只推本人设备 | 重复置顶可能改时间/顺序；没有偏好版本冲突保护 |
| 投票/关票 | 替换本人选择、空集合撤票、重复相同状态不升版本 | `client_op_id` 不持久化；普通 mutation 无自动重试队列 |

成员资格必须在事务锁内重检；DM 解除好友后只读。所有发往多用户的视图，包括创建请求的幂等回放，都必须逐接收者计算；当前创建回放广播仍复用发起者视图，已列为集成必检项。

### 9.3 UI 和同步尚需闭环的要求

- 版本保护已用于消息 upsert、mutation 和批量刷新；目录合并、实时预览、ACK 还需用反序场景证明不会回退，不能泛化为“所有入口已收敛”。
- PollCard 当前依赖最近一次响应中的 `expired/closed_at` 禁用按钮，没有本地到期计时与自动刷新；第 8.4 节“到期立即禁用并刷新”尚未实现完整闭环。
- 回复/编辑/删除/reaction、会话菜单、toast 与投票卡片已有 UI 代码，但 A/B/C 都明确没有真实浏览器/Tauri 验收。键盘、窄屏、失败重试、双设备和跨时区必须实测。
- 部分 ChatView 异步结果和新交互状态尚需账号切换回归；已有 session generation 不能直接证明所有新增流程安全。
- 现有 `read_sync` 对新到达的较大已读序号仍直接清零未读，需要覆盖“本地已有更大消息 seq”的情况。

### 9.4 交付范围

本轮目标仍是消息生命周期、个人会话偏好与群投票，不包含消息收藏、通知中心、系统推送等后续功能。A/B/C 阶段报告是各自时间点的交付快照；D 必须处理上述缺口、锁文件、迁移与真实 UI 验收后才能声明本轮完成。
