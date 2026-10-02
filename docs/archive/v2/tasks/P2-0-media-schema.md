# P2-0 第 2 阶段共用 schema（主会话执行，不派 Devin）

第 2 阶段的 4 个任务都要往 `message_views` 视图加列。`CREATE OR REPLACE VIEW` 只能在末尾追加列、不能删列，如果每个并行分支各写一份视图定义，按顺序合并时后一个迁移会「删掉」前一个加的列而直接失败。所以由主会话在派发前统一提交这一份迁移和对应的模型字段，4 个任务只写业务逻辑。

## 迁移 `0012_v2_media.sql`

- `attachments` 增加 `width`、`height`（图片像素，P2-1 写入）、`thumb_path`（缩略图存储名，P2-1 写入）、`duration_ms`（语音时长，P2-3 写入，0–3,600,000）。
- `users` 增加 `avatar_path`（头像存储名，P2-2 写入）、`avatar_version integer NOT NULL DEFAULT 0`（0 = 无头像，每次更换 +1）。
- `conversations.type` 允许 `'self'`（文件传输助手，P2-4），并加部分唯一索引：每个用户最多一个 `self` 会话（按 `owner_id`）。
- `messages` 增加 `forwarded_from text`（转发时原发送者展示名的快照，≤ 50 字，P2-4 写入）。
- `message_views` 末尾追加：`att_width`、`att_height`、`att_has_thumb`、`att_duration_ms`（墓碑消息均为 NULL）、`sender_avatar_version`、`sender_has_avatar`、`forwarded_from`。

## 模型字段（服务端 + 客户端类型）

| 结构 | 新字段 |
|---|---|
| `AttachmentView` | `width: Option<i32>`、`height: Option<i32>`、`has_thumb: bool`、`duration_ms: Option<i32>` |
| `MessageView` | `sender_avatar_version: i32`、`sender_has_avatar: bool`、`forwarded_from: Option<String>` |
| `UserRow` / `UserPublic` | `avatar_version: i32`、`has_avatar: bool`（`UserRow` 另有 `avatar_path`，不对外） |
| `ConversationView.kind` | 取值增加 `"self"` |

TS 侧对应字段全部可选，旧服务端不返回时按缺省处理。

所有构造 `UserPublic` 的查询（好友、好友申请、群成员、会话 peer、WS `user_public`）和会话列表手拼的 `last_message` 都已带上新字段；`server/tests/migrations.rs` 的迁移列表已追加到 0012（以后每个新迁移都要追加进去）。

## 验收

`cargo test --tests` 全绿（含 `migrations.rs` 升级夹具）、`npm run build` 全绿。
