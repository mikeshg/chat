# P3-2 群公告 + 置顶消息 + 视频会议链接

先读 [../devin-rules.md](../devin-rules.md)。迁移号：**`0017_announcements_pins.sql`**（要追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

- 小团队群里总有要长期看到的内容（规范、链接、值班表），现在只能靠翻聊天记录。
- 远程沟通需要随手开个会，但本项目不自研音视频，用 Jitsi 这类现成服务生成会议链接即可。第 1 阶段 P1-4 已经让消息里的链接可点击，Tauri 端会用系统浏览器打开。
- 第 1 阶段 P1-2 建了 `server_settings`（`server/src/settings.rs` 的 `ServerSettings` 结构体 + `/v1/server-info` + 管理后台 `AdminSettings.tsx`），本任务往里加一个键。同阶段 P3-3 也会往 `ServerSettings` 加推送相关的键，各加各的字段即可。

## 数据

```sql
ALTER TABLE conversations
    ADD COLUMN announcement    text CHECK (announcement IS NULL OR char_length(announcement) BETWEEN 1 AND 2000),
    ADD COLUMN announcement_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN announcement_at timestamptz;

CREATE TABLE conversation_pins (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id      uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    pinned_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    pinned_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, message_id)
);
```

## 规则

### 群公告

- 只有群聊有公告；私聊、文件传输助手没有（接口返回 400）。
- 只有群主能设置、修改、清空；空字符串或 null 表示清空（三列都置 NULL）。
- 内容 1–2000 字符（按字符数，与消息长度口径一致）。
- 变更后给全体成员发 `invalidate { what: "conversations" }`（沿用改群名的做法），成员刷新目录拿到新公告。

### 置顶消息

- 所有会话类型都能置顶（群、私聊、文件传输助手）；**任何成员**都能置顶、取消置顶（小团队互信；群主不需要额外权限）。
- 每个会话最多 20 条，超出返回 400「最多置顶 20 条」。
- 只能置顶本会话、未删除的消息；已置顶再置顶幂等（204 / 返回现状）。
- 消息被软删除后，列表接口不再返回它（查询时过滤 `deleted_at IS NULL`；也可以在软删除事务里顺手删 pin 行，二选一，写清楚）。
- 私聊解除好友后变只读：置顶 / 取消置顶也被拒绝（与发消息同一规则）。
- 变更后给全体成员发新 WS 帧 `pins_updated { conversation_id }`，客户端收到后重新拉置顶列表。

### 会议链接

- `server_settings` 新键 `meeting_base_url`：默认 `https://meet.jit.si`；只接受 `http://` 或 `https://` 开头、≤ 200 字符、不含空白的地址；设为空字符串表示关闭会议按钮。
- `/v1/server-info` 增加 `meeting_base_url`（公开信息，客户端据此决定是否显示按钮）。

## 接口

- `PUT /v1/conversations/{id}/announcement`，body `{ "text": string | null }` → 返回 `{ text, updated_by_name, updated_at }`（清空时全为 null）。
- `ConversationView` 增加可选 `announcement: { text, updated_by_name, updated_at } | null`（会话列表 `GET /conversations` 里带上；旧客户端忽略）。
- `GET /v1/conversations/{id}/pins` → `[{ message: MessageView, pinned_by_name, pinned_at }]`，按 `pinned_at DESC`。
- `PUT /v1/conversations/{id}/pins/{message_id}` → 204；`DELETE` 同路径 → 204（不存在也 204）。
- `GET/PUT /v1/admin/settings` 增加 `meeting_base_url`（非法值 400）。
- 所有接口：非成员 403。

## 客户端

- **公告条**（聊天页头部下方，新组件 `app/src/ui/AnnouncementBar.tsx`，`ChatView.tsx` 只加挂载）：
  - 有公告时显示 📢 + 第一行（截断），点击展开全文，再点收起；显示「由 X 更新于 …」。
  - 公告更新时间晚于本设备记录的「已读时间」时加「新公告」高亮；展开即记为已读（`localStorage` 键 `chat_ann_seen:<convId>`，读写包 try/catch）。
  - 群主在展开状态下有「编辑」按钮：多行输入框 + 保存 / 清空 / 取消，有 busy / 错误状态。
  - 群信息面板（`GroupInfo.tsx`）里也显示公告，群主可编辑（复用同一个编辑组件）。
- **置顶条**（公告条下方，新组件 `app/src/ui/PinnedBar.tsx`）：
  - 有置顶时显示「📌 N 条置顶」+ 最新一条的摘要；点击打开置顶列表面板，每条可「跳转」（复用 `jumpToRef`）和「取消置顶」。
  - 收到 `pins_updated` 帧，或打开会话时，拉一次 `/pins`。
- **消息菜单**（hover / 右键 / 长按）加「置顶」/「取消置顶」；投票和已删除消息也可以置顶吗？投票可以，已删除消息不行（不显示菜单项）。
- **会议按钮**：`server-info` 的 `meeting_base_url` 非空时，私聊和群聊头部显示 📹「发起会议」。点击后二次确认，然后发送一条普通文本消息：

  ```
  📹 发起了视频会议，点击加入：{meeting_base_url}/chat-{12 位随机小写字母和数字}
  ```

  （走现有发送链路；链接由 P1-4 的富文本渲染变成可点击。）文件传输助手里不显示这个按钮。
- **管理后台**：`AdminSettings.tsx` 增加「视频会议地址」输入 + 保存（留空 = 关闭）。
- 同阶段 P3-1 会在群信息面板里加「文件与图片」页签，你只改公告相关部分。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P3-2 =====`：

1. 群主设公告 → 成员的会话列表里有 `announcement`，成员收到 `invalidate`；普通成员设公告 403；私聊设公告 400；清空后为 null；2001 字符 400。
2. 成员置顶一条消息 → `/pins` 返回它，其他成员收到 `pins_updated`；重复置顶幂等；取消置顶后消失。
3. 置顶别的会话的消息 → 400 或 404；非成员访问 `/pins` 403。
4. 第 21 条置顶 400。
5. 置顶的消息被删除后，`/pins` 不再返回它。
6. 解除好友后的私聊：置顶被拒绝。
7. `meeting_base_url`（**用 `spawn_named_db`**，改全局设置）：默认值出现在 `server-info`；管理员改成自建地址生效；`ftp://x`、含空格、超长 → 400；非管理员 403；设为空字符串后 `server-info` 返回空。

Vitest：会议链接生成函数（格式、随机段长度与字符集）；公告「是否新公告」判断函数。

浏览器实测（真实执行，截图放 `server/target/wt/`）：群主设公告，另一成员打开群看到「新公告」高亮，展开后消失；置顶一条消息、从置顶面板跳转到它；点「发起会议」发出会议链接消息且链接可点击。

## 不做

- 公告 @所有人 推送、公告历史版本、置顶排序拖拽。
- 自研音视频、会议内嵌 iframe。
