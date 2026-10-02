# Chat V1 · Devin C 交付说明：轻量群投票

> 范围：仅「群投票」子任务（feature plan §3.6、§8.4）。最小可用闭环：migration/model/domain/REST/WS 同步/最小 UI/测试。
> 前置：保留 Devin A（消息生命周期）与 Devin B（会话偏好）全部未提交改动；未 commit/push；未动 0001–0008 migration。

## 1. 变更清单

### Migration（新增 0009，前序未改动）

- `server/migrations/0009_polls.sql`
  - `polls (message_id PK/FK → messages ON DELETE CASCADE, question, allow_multiple, anonymous, closed_at, expires_at, version bigint)`——投票 1:1 挂在 `kind='poll'` 消息上；`question` 明确落库（content 占位文本由它派生）
  - `poll_options (id uuid PK, poll_id FK, option_index, label)`；`UNIQUE(poll_id, option_index)` + `UNIQUE(poll_id, id)`——后者供复合外键引用
  - `poll_votes (poll_id, option_id, user_id, created_at)`，PK `(poll_id, option_id, user_id)`；**复合外键 `(poll_id, option_id) REFERENCES poll_options(poll_id, id)`** 兜底保证投票的选项归属本 poll；`idx_poll_votes_user(poll_id, user_id)` 供查本人选择
  - `CREATE OR REPLACE VIEW message_views`：末尾追加 `poll` 列（只能在末尾加列），LATERAL 子查询聚合 question/选项计数/total_voters/各选项 voters——**读模型单查询内聚合，无 N+1**；墓碑消息（deleted_at IS NOT NULL）掩去 poll，与正文/附件同口径；`expired` 由库内 `expires_at <= now()` 计算（读写同口径）

### 服务端

- `model.rs`
  - `PollView { question, allow_multiple, anonymous, closed_at, expires_at, expired, version, total_voters, options, my_option_ids }`；`PollOptionView { id, index, label, count, voters }`
  - `MessageView.poll: Option<PollView>`——可选字段，旧客户端忽略、看 `content='[投票] question'` 占位
  - `CreatePollReq { question, options, allow_multiple, anonymous, expires_at, client_msg_id, client_op_id }`；`VotePollReq { option_ids, client_op_id }`（`client_op_id` 均仅契约参数，不持久化）

- `domain.rs`
  - `PollAgg/PollOptionAgg`：`message_views.poll` jsonb 的反序列化形状（库内聚合，含全部 voter id，不出库）；`PollAgg::into_view(viewer)` 逐人计算 `my_option_ids` 并对匿名投票剥离 `voters`——**匿名掩蔽在视图层强制，REST/WS/广播全路径共用，无旁路**
  - `validate_poll_draft`：question trim 后 1–200 字符；options 各项 trim、非空、≤100 字符、去重后 2–10 个
  - `create_poll`：整形后走 `post_message` 同一事务（seq 分配、`client_msg_id` 幂等键、@提及解档全部复用）；`PostMessage.poll: Option<PollSpec>`——**kind='poll' 只能由此产生**，WS `send_message` 路径硬编码 `poll: None`，普通消息无法伪造投票；`write_message` 锁内强制 `kind_name=='group'`（DM → 400）、禁附件、`expires_at` 必须在库时间的未来；同事务插 `polls` + `poll_options`
  - `lock_poll_in_tx`：`lock_message_for_mutation`（会话锁+成员校验+消息行锁）→ `kind=='poll'` 检查 → `polls ... FOR UPDATE`，锁顺序固定 **conversation → message → poll**
  - `vote_poll`：替换语义——提交的 option_ids 成为本人完整新选择（去重、空集合=撤票）；软删除/已关闭/已过期 → 409（过期按库内 `now()` 判定）；选项归属校验（应用层 400 + 复合外键兜底）；单选 >1 项 → 400；**新旧集合比对，相同则幂等不升版本**；真实变更同事务 `polls.version+1` 与 `messages.edit_version+1`，`seq` 不动
  - `close_poll`：创建者或群主（`sender_id==uid || my_role=='owner'`）→ 403 其余成员；`closed_at` 已存在则幂等返回现状不升版本；软删除 → 409
  - `finish_poll_change`：事务内读回 `message_views` 完整行 + 成员表，组装 `Edited`（与编辑 mutation 同形）

- `api.rs`
  - `POST /v1/conversations/{id}/polls` → `MessageView`；成功后按普通消息广播 `WsOut::Message`（含 `posted.unarchived` 的 preference_updated 补推，与 ws.rs 发送路径同口径）
  - `PUT /v1/messages/{id}/poll/votes` → `MessageView`；成功后 `ws::broadcast_message_updated`
  - `POST /v1/messages/{id}/poll/close?client_op_id=` → `MessageView`；同上
  - `ConvRow`/`list_conversations`：last_message 预览加 `lm.poll AS m_poll` 列，`into_view(uid)` 个性化（会话卡片可显示投票预览）
  - 三个处理器均返回**发起者视角**的完整 MessageView，与 WS 广播帧同形

- `ws.rs`：**未新增任何帧类型**。`send_message` 的 `PostMessage` 显式 `poll: None`；投票同步完全复用 `message`（创建）与 `message_updated`（投票/关闭）帧，`broadcast_message_updated` 本就逐成员 `into_view` 个性化——`my_option_ids`/voters 掩蔽自动逐接收者正确

### 前端

- `types.ts`：`PollOptionView`、`PollView`、`CreatePollReq`；`MessageView.poll?: PollView | null`
- `api.ts`：`polls.create(convId, body)` / `polls.vote(msgId, optionIds, client_op_id?)` / `polls.close(msgId, client_op_id?)`
- `store.ts`：**零改动**——`applyUpdatedMessage`/`upsertMessage` 的 id+edit_version 整视图合并天然覆盖 poll 字段；poll 消息经 `applyLiveMessage` 入列、不占未读之外的额外路径
- `ui/NewPollModal.tsx`：创建弹窗（仅群聊入口挂载）——问题、动态 2–10 选项行、单/多选、匿名开关、`datetime-local` 截止（可空）；提交前本地预校验与服务端同口径；`client_msg_id=crypto.randomUUID()`；成功后 `applyLiveMessage` 即时合并（广播帧按 id 幂等去重）；busy 禁用 + 错误行内显示
- `ui/PollCard.tsx`：投票卡片——进度条 + 票数/百分比、单选 radio / 多选 checkbox 视觉、非匿名 voters 名单 title 悬浮、我的选择高亮、「撤票」（空集合提交）、创建者/群主可见「关闭投票」（confirm 确认）；`closed_at || expired` 禁用全部操作；busy 按消息 id 独立（`pollBusy` map）；错误经 `onErr` 入既有 `mutErr` 出口
- `ui/ChatView.tsx`：`kind==='poll' && m.poll` 分支渲染 PollCard（其余走正文）；composer 群聊显示 🗳 入口；`kindLabel` 补 `[投票]` 供回复预览
- `styles.css`：`.poll-card / .poll-q / .poll-meta / .poll-opt(.mine) / .poll-fill / .poll-label / .poll-count / .poll-foot / .poll-act / .poll-opts-edit / .poll-opt-row / .poll-add`，沿用既有 CSS 变量；卡片宽度 `min(320px, 100%)` 适配窄屏

## 2. §8.4 契约落实对照

| 要求 | 落实 |
| --- | --- |
| 仅群聊、群成员创建/投票 | `write_message` 锁内查 `kind_name`（DM→400）；成员校验复用 `post_message`/`lock_message_for_mutation`（非成员→403）；均有测试 |
| 复用 post_message + client_msg_id | 创建即 `PostMessage{poll: Some(spec)}`，seq/幂等/解档同事务；同 `client_msg_id` 重试返回同 id（有测试） |
| 替换语义、撤票、单/多选 | 新旧集合 diff；空集合删行；单选 >1 → 400；多选上限=选项数（PK 天然去重） |
| 过期/关闭/删除拒投 | 库内 `now()` 判定 closed_at/expires_at；软删除 → 409；墓碑视图掩 poll（有测试） |
| 关票权限与幂等 | `sender_id==uid \|\| owner`；重复关闭版本不升（有测试） |
| 版本契约 | 真实变更 `polls.version` 与 `messages.edit_version` 同步 +1；同集合并发收敛为 1 次变更（有测试）；`seq` 不变（有断言） |
| 完整 MessageView 唯一合并契约 | 投票/关闭广播 = `message_updated` 全视图；客户端 `upsertMessage` 版本守卫直接生效，前端零新合并路径（有 store 测试） |
| 无 N+1 | poll 聚合在 `message_views` 视图 LATERAL 内完成；history/列表/广播/REST 回读同一条 SELECT |
| 匿名隐私 | `PollAgg::into_view` 剥离 voters；REST 响应、WS 帧、列表预览共用此出口；`my_option_ids`/`count`/`total_voters` 仍可用（有测试断言全路径无 voter id） |
| 无独立 WS mutation | mutation 全走 REST；广播复用现有帧；`client_op_id` 契约参数不持久化 |

## 3. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check`（server） | 通过（已 `cargo fmt` 收敛） |
| `cargo check`（server） | 通过，0 错误 |
| `cargo test`（server） | **35/35 通过**（29 既有 + 6 个新投票测试） |
| `npm run test`（vitest，app） | **12/12 通过**（9 既有 + 3 个新投票用例） |
| `npm run build`（tsc -b + vite build） | 通过，41 modules，无 TS 错误 |

新增服务端测试（`server/tests/it.rs`，均通过，测试库 `chat_test`）：

- `poll_create_validate`：kind/content 占位、trim 落库、expires_at、client_msg_id 幂等、历史含 poll、DM 400、非成员 403、8 种非法形状 400、WS `send_message` 携 `kind:'poll'` 不可伪造
- `poll_vote_replace`：首投/同集合幂等/多选累加/替换回落/撤票、非成员 403、跨 poll 选项与随机 uuid 400、单选 >1 400 与换票、非 poll 消息 404、last_seq/unread 不推进
- `poll_close_perm`：普通成员关票 403、创建者关票（版本+1）、重复关幂等、关后拒投 409 且结果可读、群主（非创建者）可关、非 poll 404
- `poll_expire_and_tombstone`：回拨 expires_at → 投票 409 + `expired=true`；软删除后投票/关闭 409、墓碑 `poll=null`
- `poll_anonymous_and_ws`：创建广播 `message` 帧含 poll；匿名投票的 REST 响应与双端 `message_updated` 均无 voter id，`my_option_ids` 逐人正确；非匿名 voters 可见；帧携带完整视图
- `poll_concurrent_votes`：同用户并发不同选择 → 串行化两次真实变更（version=2、终态收敛单选项）；并发相同选择 → 仅一次变更（version 只 +1）

新增前端测试（`app/src/store.test.ts`）：

- 创建帧携带 poll 视图、投票更新按版本替换
- 反序安全：关票帧（v2）先到、投票帧（v1）迟到丢弃，`closed_at` 不回滚
- 墓碑帧后 `poll=null` 且不被旧帧还原

## 4. 未执行/未验证项（如实标注）

- **UI 交互未做组件级/e2e 验证**：弹窗开合、选项点击、busy 禁用、confirm 对话框无 DOM 测试；tsc/build/vitest 不构成 UI 行为验证。
- 截止时间输入为 `datetime-local`（本地时区→ISO），未做多时区用例。
- 匿名掩蔽在视图层单点强制；未做对抗性字段探测审计（如未来新增返回 poll 的端点须同样走 `into_view`）。
- `vite build` 产物未做运行时冒烟（未起真实浏览器会话验证帧流与卡片渲染）。
- e2e 脚本（`scripts/e2e.mjs`）未扩投票场景。
- 相同集合投票/重复关闭仍会发一次 `message_updated` 广播（版本不变，客户端等版本合并为无害 no-op）；如需完全静默可在 domain 返回变更标记，本次按最小闭环保留。

## 5. 交接备注（给后续 Devin 阶段）

- **kind='poll' 唯一入口是 REST `create_poll`**：`PostMessage.poll` 是内部字段，WS 路径硬编码 `None`——新增发消息路径时勿把 poll 透出去。
- `poll_votes` 复合外键 `(poll_id, option_id)` 依赖 `poll_options` 的 `UNIQUE(poll_id, id)`——拆表/改 PK 时必须先保留该约束。
- `message_views` 是 `CREATE OR REPLACE VIEW`：后续 migration 若再加列只能追加在 `poll` 之后；改列名会要求 DROP 重建（注意依赖）。
- 匿名隐私依赖 `PollAgg::into_view` 单点掩蔽——任何返回 `MessageView`/`poll` 的新出口（搜索、pin 预览等）必须复用该函数，不得直接序列化 `PollAgg`。
- 选项创建后不可改是刻意约束（无 edit API）；如将来允许改题需另设版本语义，勿复用 vote 的替换语义。
- `expires_at` 过期无后台任务：展示态与写入拒绝都靠读取时的 `now()` 判定；已过期未关闭的投票仍可被创建者/群主关闭以落定 `closed_at`。
- 工作区全部改动未提交（本任务 + A + B + P1/P2）；`app/package-lock.json` 一致性留 D 集成。
