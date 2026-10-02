# P2-4 文件传输助手 + 消息转发 / 收藏

先读 [../devin-rules.md](../devin-rules.md) 和 [P2-0-media-schema.md](P2-0-media-schema.md)（`conversations.type` 已允许 `'self'` 且每人至多一个、`messages.forwarded_from` 与视图列、`MessageView.forwarded_from` 已由主会话建好，**不要再改 `message_views`**）。迁移号：**`0015_self_conversations.sql`**（只做数据回填；要追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

个人使用最高频的场景之一是「在自己的手机和电脑之间传文件、存备忘」（微信文件传输助手、Telegram Saved Messages）。多设备实时同步已有，只缺一个「只有自己」的会话。另外消息不能转发，想把群里的链接 / 文件发给别人只能手动复制或重新上传。

## 一、文件传输助手（`self` 会话）

### 规则

- 每个用户恰好一个 `type='self'` 的会话：`owner_id` = 本人，唯一成员是本人（`memberships.role='owner'`），`name` 为 NULL（客户端显示「文件传输助手」）。
- 创建时机：
  - 迁移 `0015` 为所有现有用户回填（`INSERT ... SELECT`，靠 P2-0 的唯一索引保证幂等）。
  - 注册事务内为新用户创建。
  - 写一个 `domain::ensure_self_conversation(executor, uid)`（`INSERT ... ON CONFLICT DO NOTHING` 后再查），注册和迁移遗漏兜底都用它。
- 可做：发文字、图片、文件、语音（同阶段 P2-3）、回复、编辑、删除、reaction、搜索、置顶 / 静音 / 归档偏好。
- 不可做（服务端返回 400）：拉人、踢人、退出、改名、转让、解散、建投票。
- 发送不走好友校验（`dm_writable` 只对 `dm`）；@提及解析返回空；typing 不广播；presence 无关。
- 会话列表：`kind='self'`、`peer` 为 NULL、`member_count=1`。把服务端里所有按 `type` 分支的地方都过一遍（`grep -n "'dm'\|\"dm\"\|'group'\|\"group\"" server/src/*.rs`），确认 `self` 走的是正确的分支。
- 管理后台统计：`dm_count`、`group_count` 不含 `self`。

## 二、转发

### 接口

`POST /v1/messages/{id}/forward`，body `{ "conversation_ids": [uuid, ...], "client_op_id": uuid }`：

- `conversation_ids` 1–10 个、去重；`client_op_id` 必填（用于幂等）。
- 源消息：请求者必须是源会话成员；已删除 → 400 `消息已删除`；`kind='poll'` → 400 `投票不能转发`。
- 对每个目标会话**独立**处理，互不影响，返回：

  ```json
  { "results": [ { "conversation_id": "...", "ok": true, "message": { MessageView } },
                 { "conversation_id": "...", "ok": false, "error": "not_friends", "message_text": "..." } ] }
  ```

- 每个目标复用 `domain::post_message`（成员校验、私聊只读、seq、幂等全部沿用），参数：
  - `content`：源消息正文（附件占位文本照抄）。
  - `client_msg_id`：`fwd:<client_op_id>:<目标会话id>`，同一请求重试不会重复发。
  - `forwarded_from`：源消息发送者的展示名快照（在 `PostMessage` 里加这个可选字段并写入 `messages.forwarded_from`）。
  - `reply_to_id`：不带。
  - 附件：在目标会话里**复制一行** `attachments`：新 `id`，`conv_id`=目标，`uploader_id` **保持原上传者**，`file_name/mime/size/storage_path/thumb_path/width/height/duration_ms` 原样复制（共享物理文件，不拷贝字节）。`post_message` 现在要求「附件属于本人」，转发要绕开这条校验——给 `PostMessage` 加一个仅服务端内部使用的「已校验附件」入口，**不能**让普通 `send_message` 借此发送别人的附件。
- 现有软删除逻辑只在最后一个引用消失时删物理文件，所以删除源消息不影响转发出去的副本（写测试证明）。
- 广播：每条新消息照常推给目标会话成员。把 `ws.rs` `send_message` 和 `api.rs` `create_poll` 里重复的「新消息广播 + 取消归档偏好推送」提成一个 `pub async fn broadcast_posted(st, conversation_id, &Posted)`（放 `ws.rs`），三处共用。第 3 阶段会在这里挂离线推送，所以所有新消息都要经过它。
- 整个请求 `rate_limit` 限流（键 `fwd:<uid>`，沿用 20 次 / 5 分钟即可）。

### 收藏

「收藏」= 转发到自己的 `self` 会话。客户端用同一个接口，不另建表。

## 客户端

- `app/src/store.ts` `convTitle`：`kind==='self'` 返回「文件传输助手」。
- 会话列表：`self` 会话用专用图标（例如 📁 或 🗂，和首字母方块同尺寸）；不显示在线点。
- 聊天页头部：`self` 会话不显示成员数 / 在线状态 / 群信息按钮，副标题「在你的设备间同步消息与文件」。
- 消息菜单（hover / 右键 / 长按）新增「转发」「收藏」；`poll` 和已删除消息不显示。
- 新组件 `app/src/ui/ForwardModal.tsx`：搜索框 + 会话多选列表（文件传输助手置顶，其余按会话列表顺序，排除只读私聊——客户端不知道好友状态就不排除，靠服务端逐条返回错误），最多选 10 个；确认后调用接口，按结果显示「已转发到 N 个会话」或逐条失败原因；busy / 错误状态齐全。同一次提交的重试复用同一个 `client_op_id`。
- 「收藏」直接调用接口转发到自己的 `self` 会话，成功 toast「已收藏到文件传输助手」。
- 被转发的消息气泡顶部显示灰色小字「转发自 {forwarded_from}」。
- 同阶段 P2-2 改头像组件、P2-1 改图片气泡、P2-3 改输入框；你只动菜单、头部、会话标题这几处。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P2-4 =====`：

1. 新注册用户会话列表里有且只有一个 `kind='self'`；迁移回填：`server/tests/migrations.rs` 里补一个用例或断言（先在旧 schema 插用户，跑到最新迁移后有 `self` 会话）。
2. `self` 会话发消息：本人两个 WS 连接都收到；别人拿这个会话 id 访问历史 403 / 404。
3. `self` 会话拉人、退出、改名、解散、建投票都返回 400。
4. 转发文字到一个 DM 和一个群：两条消息 `forwarded_from` 正确、`seq` 各自递增、目标成员收到 WS 广播。
5. 转发图片：目标会话里下载成功；删除源消息后目标副本仍可下载；再删除副本后物理文件被清理（轮询等待）。
6. 转发到已删好友的 DM：该条 `ok=false, error="not_friends"`，其他目标成功。
7. 同一 `client_op_id` 重试：不产生重复消息，返回同样的消息。
8. 转发投票 400；转发已删除消息 400；转发不是自己所在会话的消息 403；普通 WS `send_message` 仍不能发别人的 `attachment_id`。

Vitest：转发目标列表的排序 / 过滤函数；`convTitle` 对 `self` 的处理。

浏览器实测（真实执行）：同一账号两个浏览器上下文，在 A 的文件传输助手里发文件，B 实时收到；把群里一条图片消息「收藏」，文件传输助手里出现带「转发自」的图片；转发一条消息到另一个好友，对方收到。截图放 `server/target/wt/`。

## 不做

- 多选消息批量转发、合并转发（聊天记录卡片）。
- 独立的「收藏夹」页面和标签分类。
