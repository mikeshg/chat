# Chat V1 · Devin A 交付说明：消息生命周期

> 范围：仅「消息生命周期」子任务（回复/编辑/软删除/reactions）。会话偏好与投票未实现。
> 设计真源：`docs/archive/chat-v1-feature-plan.md`（含第 8 节实施细则）。

## 1. 变更清单

### Migration（0001–0006 未改动）

- `server/migrations/0007_message_lifecycle.sql`（新增）
  - `messages` 新增列：`reply_to_id`（FK→messages.id, ON DELETE RESTRICT 语义靠应用层）、`edited_at`、`edit_version`（默认 0）、`edit_op_id`、`deleted_at`、`deleted_by`
  - 新表 `message_reactions (message_id, user_id, emoji, created_at, PK(message_id,user_id,emoji))`
  - 索引 `idx_messages_reply_to`
  - 重建视图 `message_views`：软删除掩蔽（content='消息已删除'、attachments/mentions 置空）、`reply_to_preview`（一级展开，目标删除时 `deleted=true`、excerpt 清空、保留 id/seq）、reactions 聚合（emoji+count+user_ids）

### 服务端

- `model.rs`
  - `MessageView` 增加 `reply_to_id / reply_to_preview / edited_at / edit_version / deleted_at / deleted_by / reactions`
  - `ReplyPreview { message_id, seq, sender_name, kind, excerpt, deleted }`
  - `ReactionView { emoji, count, reacted_by_me }`
  - `WsIn` 新增 `EditMessage / DeleteMessage / ReactionUpsert / ReactionRemove`，`SendMessage` 增加 `reply_to_id`
  - `WsOut` 新增 `MessageUpdated / MessageDeleted / ReactionUpdate / OpAck`；`Error` 同时携带 `client_msg_id`（发送路径）与 `client_op_id`（mutation 路径），两者严格不混用
  - REST 类型：`EditMessageReq{content, expected_version, client_op_id}`、`ClientOpQuery{client_op_id: Option<Uuid>}`、`RefreshMessagesReq{ids}`、删除响应载荷
  - 全部 `client_op_id` 均为 `Uuid`（非 UUID 输入由 serde/axum 直接 400）

- `domain.rs`
  - `MsgViewRow` 扩展全部生命周期字段；`into_view(viewer)` 按查看者计算 `reacted_by_me`
  - 消息查询函数（after/before/latest/search/by_ids）全部带 viewer 参数
  - `post_message` 支持 `reply_to_id`，同事务校验目标必须同会话且未删除
  - `lock_message_for_mutation`：会话行锁→消息行锁的固定锁序；DM 复核好友关系（只读 DM 拒绝一切 mutation）；`edit_expired = created_at + 15min <= now()`
  - `edit_message`：仅作者、仅 `kind='text'`、15 分钟窗口、`expected_version` 乐观并发；`edit_op_id` 命中即幂等回放（不二次递增版本）；群聊重解析 mentions；**不动 seq/last_seq**
  - `delete_message`：本人可删、群主可删群内任意消息；幂等返回现状；删 reactions/mentions/attachments 行；`orphan_files` 只收集**无其他 attachments 行仍引用同一 storage_path** 的物理文件
  - `reaction`：upsert/remove 幂等，仅真实状态变化才 `edit_version+1`；每人每消息 ≤20 种 emoji；返回聚合 + 成员列表供个性化广播

- `ws.rs`
  - 新帧分发；编辑/删除/reaction 广播均携带**完整 MessageView** 并**逐成员个性化序列化**（`reacted_by_me` 各不相同；见 §7 反序修复）
  - `op_ack` 只回发起连接；error 帧携带对应 `client_op_id` 而非 `client_msg_id`
  - 既有 seq 同步、session generation、ticket 校验全部保留

- `api.rs`
  - 新路由：`PATCH /v1/messages/{id}`、`DELETE /v1/messages/{id}`、`PUT/DELETE /v1/messages/{id}/reactions/{emoji}`、`POST /v1/conversations/{id}/messages/refresh`（ids ≤100，超了 400；非成员 403）
  - `post_err` 统一映射 domain 错误码→HTTP 状态
  - `spawn_file_cleanup`：事务提交后异步清理 orphan 物理文件
  - REST 编辑/删除/reaction 响应均为发起者视角的完整 `MessageView`（与 WS 帧同形），发起端不等广播即可合并
  - 会话列表 last_message 构造补齐生命周期字段；历史/搜索查询传 viewer

### 前端

- `types.ts`：`ReplyPreview`、`ReactionView`、`MessageView` 新字段、WS 新帧类型
- `api.ts`：`msgs.edit / remove / react / unreact / refresh`
- `ws.ts`：`message_updated / message_deleted / reaction_update / op_ack` 分发到 store
- `store.ts`：`applyMessageUpdated / applyMessageDeleted / applyReactionUpdate` 均按 `edit_version` 拒绝乱序（`<=` 本地版本即丢弃）；重连后除 seq 补拉外，按 message id 批量 refresh（≤100/批）刷新本地缓存旧消息
- `ChatView.tsx`：回复模式（引用条、点击跳转、缺失目标按 seq 拉取）、编辑模式（15 分钟、草稿还原、Esc）、删除确认、reaction 选择器与聚合条、墓碑渲染、`已编辑` 标记、只读 DM composer、mutation busy/error 态、`crypto.randomUUID()` 生成 client_op_id
- `styles.css`：操作条/墓碑/引用块/reactions/横幅/错误提示样式，沿用既有视觉体系

## 2. §8 契约落实对照

| 要求 | 落实 |
| --- | --- |
| seq 不用于离线编辑覆盖 | 编辑/删除/reaction 全部只升 `edit_version`，不动 `seq`/`last_seq` |
| 重连批量刷新缓存 | `messages/refresh` 端点按 id 回读，前端 ≤100/批 |
| 版本防乱序 | 所有 mutation 帧带 `edit_version`；store 按 `<=` 丢弃旧版本 |
| `expected_version` 精确契约 | REST/WS 编辑必填；不等即 409 |
| `client_op_id` = UUID ≠ client_msg_id | 全链路 `Option<Uuid>`；op_ack/error 与 message_ack 分离 |
| reaction 个性化 | 读模型 `into_view(viewer)`；广播逐成员成帧 |
| 只读 DM 禁 mutation | `lock_message_for_mutation` 复核好友关系，回复/编辑/删除/reaction 一律 403/not_friends |
| 共享 storage_path 不误删 | 删 attachments 行后 `EXISTS` 复查引用，仅真孤儿进 orphan_files |
| 不改 0001–0006 | 仅新增 0007，`sqlx::migrate!` 自动接入 |

## 3. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check`（server） | 通过 |
| `cargo check --locked --all-targets` | 通过，0 错误 0 警告 |
| `cargo test --locked --test it` | **26/26 通过**（8 个生命周期测试 + 修正断言后的既有竞态测试，见 §5） |
| `npm run build`（tsc + vite build） | 通过，39 modules，无 TS 错误 |
| `npx vitest run`（app） | **5/5 通过**（store 反序合并回归，见 §7；新增 vitest@^3.2.4 devDependency） |

新增测试（`server/tests/it.rs`，均通过）：

- `lifecycle_reply`：回复预览字段、跨会话回复拒绝、目标删除后墓碑预览
- `lifecycle_edit`：非作者 403、版本冲突 409、正常编辑（版本+1/seq 不变/edited_at）、附件消息 400、15 分钟窗口 400、已删 409、幂等 client_op_id
- `lifecycle_delete`：DM 越权 403、群权限矩阵、删除幂等、正文掩蔽（历史+搜索）、附件删除后 404
- `lifecycle_reactions`：加/删幂等、版本只在真实变更时递增、`reacted_by_me` 个性化、20 种 emoji 上限、墓碑 409
- `lifecycle_ws_frames`：error 帧 `client_op_id` 与 `client_msg_id` 隔离、`op_ack` 仅回发起连接、双端 `message_updated`/`reaction_update`/`message_deleted`、reaction_update 双端个性化不一致（a=true, b=false）
- `lifecycle_frames_carry_full_view`（§7 回归）：edit(v1)→reaction(v2)→delete(v3)，断言每个广播帧携带完整视图——v2 帧含编辑后正文+reaction、v3 帧含完整墓碑态，任一帧独立到达即收敛正确终态
- `lifecycle_refresh`：刷新回最新正文/版本、>100 ids 400、非成员 403
- `lifecycle_readonly_dm`：解除好友后回复（WS not_friends）、编辑/删除/reaction（REST 403）

测试基建：复用既有 `chat_test` 库与 `sqlx::migrate!`，未 DROP/重建任何库；服务绑 `127.0.0.1:0`。

## 4. 未执行/未验证项（如实标注）

- **客户端 UI 交互未验证**：store 合并逻辑有 vitest 用例（见 §7），但回复/编辑/删除/reaction 的 DOM 交互、断线批量刷新流程仍无组件级/e2e 用例——`tsc`/build/vitest 不构成 UI 行为验证。
- `vite build` 产物未做运行时冒烟（未启动真实 Tauri/浏览器会话验证帧流）。

## 5. 既有测试 `owner_transfer_leave_race` 断言修正（交付前修复的红灯）

- 现象：全量并发跑时失败（`owner_id` 得到 cc 而非 b），单独跑稳定通过。
- 根因：测试断言了竞态的特定赢家——`transfer` 必须先拿锁；若 `leave` 先拿锁，群主按 `ORDER BY joined_at` 转让，而 b/c 同事务插入、`joined_at` 相同，顺序不稳定，可能选到 cc。两条路径的终态都满足不变量（恰好一个群主且仍是成员），但断言写死 `owner_id == b`。
- 处理（按交付要求只改断言，未动产品语义）：断言改为验证不变量——成员列表中恰好一个 `role=owner`、`conversations.owner_id` 与该成员一致、b/cc 仍在、a 已离群。修正后全量并发跑 25/25 稳定通过。涉及的 `leave_conversation`/`transfer_ownership` 产品代码未触碰（本任务仅将 `membership/lock_conversation/member_ids` 返回类型由 `AppResult` 调整为 `sqlx::Error`，语义不变）。

## 7. 反序交付修复（本轮追加）

**问题**：`reaction_update`/`message_deleted` 原为部分字段帧（仅 reactions 数组 / 墓碑字段）。当 `reaction_update(v2)` 先于 `message_updated(v1)` 到达时，客户端按全局 `edit_version` 丢弃 v1 帧——但 v2 帧只更新了 reactions，正文永远停在旧值；partial 帧之间同理互相吞掉更新。

**方案**（按 §8 优先方案）：所有 mutation 帧统一携带**完整 `MessageView`**：

- `WsOut::MessageDeleted` / `WsOut::ReactionUpdate` 载荷改为 `{conversation_id, message: MessageView, edit_version}`（与 `MessageUpdated` 同形）。
- `domain::Deleted` / `domain::ReactionChange` 改为携带事务内读回的完整 `MsgViewRow`（删除后 reactions 已清空，墓碑视图天然一致；reaction 行含最新聚合）。
- 广播仍逐成员 `into_view(viewer)` 序列化，`reacted_by_me` 个性化不变。
- REST `PATCH/DELETE messages`、`PUT/DELETE reactions` 响应统一为发起者视角完整 `MessageView`（`DeletedRes`/`ReactionRes` 类型已删除）。
- 客户端收敛为唯一合并入口 `applyUpdatedMessage`：整视图 `upsertMessage`（`incoming.edit_version < cached` 丢弃）+ `last_message` 版本守卫。因高版本视图包含全部历史变更，任意到达顺序均收敛到最高版本终态，无需拆 content_version/reaction_version。
- `applyMessageDeleted`/`applyReactionUpdate` 两个部分合并函数已删除。

**回归用例**：

- 服务端 `lifecycle_frames_carry_full_view`：断言 v2 reaction 帧自身携带编辑后正文、v3 delete 帧携带完整墓碑视图——即乱序场景下任一帧自洽。
- 前端 `src/store.test.ts`（vitest，5 用例）：v2→v1 反序正文/reaction 都保留；v3 删除→v2 reaction 反序墓碑不被还原；正序逐帧合并；同版本幂等；last_message 版本守卫。

## 6. 交接备注（给后续 Devin 阶段）

- 版本语义：`edit_version` 是全 mutation 单调版本（编辑/删除/reaction 变更共用），帧字段一律带它；不要用 `seq` 做乱序判断。
- REST 删除/reaction 的 `client_op_id` 仅作契约参数（UUID 校验）；删除/reaction 本身已状态幂等，未持久化 op_id（只有编辑持久化 `edit_op_id`）。如后续要求删除/表态也按 op_id 幂等回放，需要扩列。
- `orphan_files` 物理清理在 `spawn_file_cleanup` 异步执行；行删除已使下载立即 404，物理清理失败只 warn。
- `messages/refresh` 上限 100 ids/批，403 时前端应清空该会话本地缓存（已实现）。
- 会话偏好（pin/mute/archive）与投票 schema/UI 仍未实现，勿在后续阶段遗漏。
- 工作区存在大量未提交改动（本任务 + 既有 P1/P2），未执行任何 commit/push。
