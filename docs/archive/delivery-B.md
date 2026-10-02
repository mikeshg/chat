# Chat V1 · Devin B 交付说明：会话偏好（置顶 / 静音 / 归档）

> 范围：仅「会话偏好」子任务（feature plan §3.5、§8.3）。投票未实现（留给后续 Devin 阶段）。
> 前置：保留 Devin A 消息生命周期实现与更早 P1/P2 修复；未 commit/push；未动 0001–0007 migration。

## 1. 变更清单

### Migration（新增 0008，前序未改动）

- `server/migrations/0008_conversation_preferences.sql`
  - 新表 `conversation_preferences (conversation_id, user_id, pinned_at, archived_at, mute_forever, muted_until, updated_at)`，PK `(conversation_id, user_id)`，双 FK `ON DELETE CASCADE`
  - `CHECK (NOT mute_forever OR muted_until IS NULL)`：永久静音与定时静音互斥（muted_until IS NULL 仅表示无定时静音，不代表永久）
  - `idx_conv_prefs_user` 供按用户拉全量
  - 偏好是用户维度数据，不改会话本身；解散/移出后成员资格检查兜底（偏好行随 memberships 清理策略不主动删，但因非成员 403 不可再写）

### 服务端

- `model.rs`
  - `ConversationPref { pinned_at, archived_at, muted_until, mute_forever, updated_at }`（无行时 `Default`：全 NULL/false，`updated_at=None` 表示从未写入）
  - `ConversationView.preference: ConversationPref`（始终序列化；旧客户端可忽略该字段）
  - `PrefEntry { conversation_id, preference }`——与 `preference_updated` 帧同构（嵌套），前端同一合并路径
  - `PrefUpdateReq { pinned?, archived?, mute? }`、`PrefMuteReq { mode, until? }`（`mode` = `none|until|forever`；非 UUID/非法时间由 serde 422 拦下）
  - `WsOut::PreferenceUpdated { conversation_id, preference }`

- `domain.rs`
  - `pref_of(executor, conv, uid)`：读偏好行，无记录返回全默认
  - `set_preferences(pool, conv, uid, req)`：事务内 `lock_conversation` 先行（与消息 mutation 同一锁序），解散会话与非成员一律 `forbidden`→403；`mute` 形状在进入事务前校验（未知 mode / until 用在非 until 模式 / until 缺失 → 400；`until <= now()` → 400）；缺省字段不改（`None` = 不动，`Some(false)` = 清除）；`UPSERT ... ON CONFLICT DO UPDATE` 状态覆写，天然幂等
  - `Posted.unarchived: Vec<Uuid>`：`write_message` 在消息插入事务内 `UPDATE conversation_preferences SET archived_at=NULL ... WHERE user_id = ANY(mentions) AND archived_at IS NOT NULL RETURNING user_id`——@本人解除归档与消息创建同事务；普通消息不触碰；幂等命中路径返回空 vec（首个事务已生效，不重复推送）

- `api.rs`
  - `GET /v1/conversations/preferences` → `Vec<PrefEntry>`（本人全部偏好行）
  - `PUT /v1/conversations/{id}/preferences` → `ConversationPref`；成功后 `st.send_to_user(uid, preference_updated)`——只发本人全部连接（含发起连接，幂等收敛）；`client_op_id` 仅作契约参数（`ClientOpQuery` 复用，UUID 校验），未持久化
  - `list_conversations`：`LEFT JOIN conversation_preferences ON (conv, user)` 无 N+1；`ORDER BY (cp.pinned_at IS NOT NULL) DESC, cp.pinned_at DESC, COALESCE(lm.created_at, c.created_at) DESC, c.id`——置顶块按 pinned_at DESC，其余维持原活动序
  - `ConvRow` 增加 5 个 `p_*` 字段；响应映射为 `ConversationPref`

- `ws.rs`
  - `send_message` 广播后遍历 `posted.unarchived`：逐被提及者 `pref_of` 读最新偏好 → `send_to_user` 推 `preference_updated`（仅本人连接，其他成员不可见）

### 前端

- `types.ts`：`ConversationPref`、`PrefUpdate`、`ConversationPrefEntry`（嵌套形）、`ConversationView.preference?`（可选，兼容旧服务端）、`WsOut` 增 `preference_updated`
- `api.ts`：`put` 支持可选 body（reaction 的无 body PUT 不受影响）；`convs.preferences()` / `convs.setPref(id, body)`
- `store.ts`
  - `isPinned / isArchived / isMuted`（永久或 muted_until 未过期）
  - `applyPrefUpdate`：REST 响应与 WS 帧共用合并入口，更新字段并重排；未知会话忽略（WS 侧补拉目录）
  - `sortConversations`：置顶块在前按 `pinned_at` DESC，其余按活动 DESC、id 兜底
  - `shouldNotify`：本人消息 / 当前打开会话不提示；静音会话抑制、@本人突破静音
  - `pushToast / dismissToast`：最小应用内提示栈（≤4 条，5 秒自动消失）
- `ws.ts`
  - `preference_updated` 分发 → `applyPrefUpdate`；本地不认识该会话时 `refreshCatalogs` 兜底
  - `onMessage`：`shouldNotify` 通过后 `pushToast`（附件占位摘要 + `[有人@我]` 前缀）
  - `openConversation(id)` 统一入口：置 active + 切 chats tab + 已读上报 + 本地清未读 + **归档会话打开即 PUT `archived:false`**（服务端确认后经 `applyPrefUpdate` 合并；WS 帧也会到达，同状态收敛）
- `ui/ConvList.tsx`
  - 会话/归档两个 tab（归档 tab 显示计数）；搜索在各自 tab 内过滤
  - 右键或 ⋯ 按钮（hover/聚焦显示）打开偏好菜单：置顶切换、静音 1 小时 / 静音至明天（本地午夜→UTC）/ 永久静音 / 取消静音、归档切换；菜单项 busy 禁用 + 失败行内错误；Esc 关闭；菜单位置窄屏收敛
  - 列表项显示 📌 / 🔕（title 标注静音到期）；静音会话未读徽标灰底但照常计数，mention 标记不受影响
- `ui/ContactsPanel.tsx`：「发消息」改走 `openConversation`（之前直接 setState，绕过已读/解归档）
- `ui/ChatScreen.tsx`：渲染 toast 栈，点击打开会话
- `App.tsx`：登出清空 `toasts`
- `styles.css`：`.conv-tabs / .conv-flag / .conv-menu-btn / .unread.muted / button.ctx-item / .ctx-err / .toasts / .toast`，沿用既有视觉变量

## 2. §8.3 契约落实对照

| 要求 | 落实 |
| --- | --- |
| 偏好按用户、跨设备同步 | `(conv, user)` 行；`send_to_user` 发本人全部连接；重连经目录刷新恢复 |
| 会话列表携带偏好 | `ConversationView.preference` 始终返回；无行=全默认 |
| 置顶排序 | 服务端 ORDER BY 与前端 `sortConversations` 同规则：pinned 块 pinned_at DESC → 活动 DESC → id |
| 归档解除规则 | 打开会话 → 前端 PUT archived:false（ConvList/ContactsPanel/Toast 同走 `openConversation`）；普通消息不动归档；@本人消息同事务解除并向被提及者推帧 |
| 静音不挡未读/mention | 未读与 has_mention 由既有路径照常累计（有测试断言）；静音仅抑制应用内提示；@我突破 |
| 无系统推送 | 未接浏览器 Notification API、未请求权限；最小出口=点击可打开会话的 toast |
| 权限服务端强制 | 非成员/被移出/解散 → 403；serde 形状错误 → 422；域校验 → 400 |
| 幂等 | UPSERT 状态覆写；重复 PUT 同值收敛；client_op_id 契约参数 |
| 不暴露他人偏好 | `preference_updated` 只发本人连接；GET 只查本人行（有测试断言泄露面） |

## 3. 实际执行的验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check`（server） | 通过（已 `cargo fmt` 收敛） |
| `cargo check`（server） | 通过，0 错误 |
| `cargo test --manifest-path server/Cargo.toml` | **29/29 通过**（含 3 个新偏好测试） |
| `npm run test`（vitest，app） | **9/9 通过**（含 4 个新偏好用例） |
| `npm run build`（tsc -b + vite build） | 通过，39 modules，无 TS 错误 |

新增服务端测试（`server/tests/it.rs`，均通过）：

- `prefs_crud_and_order`：默认行全 NULL、置顶排序（含 pinned_at 块内序）、永久/定时静音互斥切换、6 种非法 mute 形状 4xx、`none` 清双字段、部分更新不清其他字段、幂等重放、空 PUT、GET 偏好列表
- `prefs_permissions_and_fanout`：b 双连接均收 `preference_updated`、a 收不到他人偏好帧（读到 message 为止途中无泄露）、非成员 403、GET 不列他人偏好、被移出 403、解散后 403
- `prefs_archive_and_mute_semantics`：归档后普通消息不解档且未读照常累计；@本人 → `preference_updated`（archived_at=null）且列表确认解档；永久静音下未读数/has_mention 照常更新；重连后目录仍带偏好

新增前端测试（`app/src/store.test.ts`，vitest）：

- 置顶块 pinned_at DESC + 未置顶活动序 + id 兜底
- `applyPrefUpdate` 合并重排；未知会话不动作
- `applyLiveMessage` 重排后置顶仍在前（pin 不丢）
- `shouldNotify`：定时/永久静音抑制、过期静音放行、@我突破、本人消息与当前打开会话不提示

## 4. 未执行/未验证项（如实标注）

- **UI 交互未做组件级/e2e 验证**：菜单开合、tab 切换、toast 点击跳转无 DOM 测试；`tsc`/build/vitest 不构成 UI 行为验证。
- 「静音至明天」取本地下一午夜转 UTC——跨时区语义即「本地今天结束」，符合 §8.3 描述但未做多时区测试。
- 归档会话在归档 tab 内仍可搜索/打开；未实现「归档会话收到消息时是否在归档 tab 显示未读」的差异化样式（未读照常显示，符合 spec）。
- `vite build` 产物未做运行时冒烟（未起真实浏览器/Tauri 会话验证帧流）。
- e2e 脚本（`scripts/e2e.mjs`）未扩偏好场景。

## 5. 交接备注（给后续 Devin 阶段）

- 偏好接口契约：`PUT` 只传要改字段；`mute` 三选一互斥形状（`none`/`until+until`/`forever`），`until` 必须为合法未来 RFC3339。
- `Posted.unarchived` 只对「首个成功事务」非空——幂等回放路径返回空，推送逻辑依赖此保证不重复发帧。
- 提及触发的解档在消息事务内完成；若将来给 DM 也启用 mention 解析，解档会自然生效（当前 `parse_mentions` 仅群聊）。
- 前端「打开即解档」依赖 `openConversation` 统一入口——新增打开会话的路径（如通知栏/深链）必须走它而不是裸 `setState`。
- `ConversationPrefEntry` 与 `preference_updated` 帧同构（嵌套 `preference` 字段），勿改回扁平。
- 偏好行无行即默认；客户端 `preference?` 可空是兼容旧服务端，新服务端始终返回该字段。
- 投票（polls）仍未实现：schema、WS 帧、UI 全部待办，见 feature plan §3.6/§8.4。
- 工作区全部改动未提交（本任务 + Devin A + P1/P2）；`app/package-lock.json` 为 A 生成、pnpm 一致性留 D 集成。
