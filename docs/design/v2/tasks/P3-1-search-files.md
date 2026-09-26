# P3-1 全局搜索 + 会话「文件与图片」列表

先读 [../devin-rules.md](../devin-rules.md)。迁移号：**`0016_search_trgm.sql`**（要追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

- 现在只能在单个会话里搜索（`GET /v1/conversations/{id}/messages/search?q=`，`domain.rs` `search_messages`）。它查的是 `message_views`，而视图里墓碑消息的 `content` 是占位文字「消息已删除」，所以搜「消息」会命中已删除的消息——**这是现有 bug，本任务一并修掉**。
- 想找「上周谁发的那个 PDF」只能一个个会话翻。
- 在视图的 `CASE WHEN ... END AS content` 上做 `ILIKE` 用不了 `messages.content` 上的任何索引，消息多了会慢。

## 数据

```sql
CREATE EXTENSION IF NOT EXISTS pg_trgm;
-- 只索引未删除消息的正文
CREATE INDEX idx_messages_content_trgm ON messages USING gin (content gin_trgm_ops) WHERE deleted_at IS NULL;
```

`pg_trgm` 在 PG13+ 是可信扩展，库 owner 就能创建（docker compose 里的 `chat` 用户是超级用户，没问题）。

## 规则

- 只搜**当前是成员**的会话（退群 / 被移出后不能再搜到那个会话的历史，与现有访问规则一致）。
- 不返回已删除消息，不按墓碑占位文字匹配。
- 查询方式：先直接查 `messages` 表拿候选（`deleted_at IS NULL AND content ILIKE ... ESCAPE '\'`，加上筛选条件、成员 JOIN、排序、LIMIT），再用已有的 `domain::messages_by_ids`（或同类按 id 批量读 `message_views` 的函数）取完整视图并按 viewer 个性化。沿用现有的 `%`、`_`、`\` 转义写法。
- 现有会话内搜索 `search_messages` 改用同样的查询方式（修掉墓碑命中），接口与返回形状不变。

## 接口

`GET /v1/search`，参数都可选但至少要有 `q` 或一个筛选条件：

| 参数 | 说明 |
|---|---|
| `q` | 关键词，去首尾空白后 1–100 字符；为空时按筛选条件浏览 |
| `conversation_id` | 只搜某个会话（必须是成员，否则 403） |
| `sender_id` | 只看某人发的 |
| `kind` | `text` / `image` / `file` / `audio`（按 `messages.kind`） |
| `from` / `to` | RFC3339，按 `created_at` 过滤（含 from、不含 to） |
| `before` | 翻页游标：上一页最后一条的 `created_at`（RFC3339）加 `before_id`（同毫秒时用 id 兜底） |
| `limit` | 默认 30，1–100 |

返回 `{ "messages": [MessageView...], "has_more": bool }`，按 `created_at DESC, id DESC`。客户端用 `conversation_id` 对照本地会话目录显示会话名（服务端不再拼标题）。

参数全空 → 400；`q` 超长 → 400。

## 客户端

- **会话列表顶部的搜索框**（`ConvList.tsx` 已有搜索输入的话复用，没有就加；搜索逻辑放新组件 `app/src/ui/SearchPanel.tsx`）：
  - 输入时（防抖约 300ms）：上半部分「会话」按会话标题本地过滤（`convTitle`），下半部分「聊天记录」调 `/v1/search?q=`。
  - 结果行：会话名 + 发送者 + 高亮关键词的摘要 + 时间；图片 / 文件 / 语音显示对应占位文字。
  - 类型筛选：全部 / 图片 / 文件 / 语音（chip 按钮）；「更多筛选」里有发送者（从当前会话目录里出现过的人和好友里选）和日期范围（原生 `<input type="date">`）。
  - 滚到底加载更多（`before` 游标）。
  - 点结果：打开对应会话并定位到那条消息。`ChatView.tsx` 已有 `jumpToRef(id, seq)`；在 store 里加一个 `pendingJump: { convId, messageId, seq } | null`，打开会话后由 ChatView 消费并清空。
  - Esc 或清空输入框回到普通会话列表。
- **「文件与图片」**：群信息面板（`GroupInfo.tsx`）和私聊 / 文件传输助手的信息入口里加一个页签（放新组件 `app/src/ui/ConvMedia.tsx`）：
  - 「图片」：缩略图网格（有 `has_thumb` 用 `?thumb=1`），点开用现有预览弹窗。
  - 「文件」：文件名、大小、发送者、时间，点击下载（`files.url(id, { dl: true })`）。
  - 「语音」：列表 + 播放（复用 `AudioBubble`）。
  - 数据都来自 `/v1/search?conversation_id=&kind=`，滚动加载更多。
- 同阶段 P3-2 会改聊天页头部和群信息面板的公告部分，你只加页签和面板，不改公告相关的代码。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P3-1 =====`：

1. 两个会话各有含关键词的消息 → 全局搜索都能搜到，按时间倒序。
2. 不是成员的会话（别人的群、已退出的群）搜不到。
3. 已删除的消息搜不到；搜「消息已删除」这几个字不命中墓碑。会话内搜索接口同样修好（回归用例）。
4. `kind=image` + `conversation_id` 只返回该会话的图片；`sender_id` 过滤生效；`from`/`to` 过滤生效。
5. 翻页：造 5 条、`limit=2`，三页拿全、不重不漏，`has_more` 正确。
6. `%`、`_` 作为字面量搜索（不当通配符）。
7. 参数全空 400；`q` 101 字符 400；`conversation_id` 不是成员 403。
8. `migrations.rs` 里新迁移能在已有数据上执行（索引建立）。

Vitest：摘要里关键词高亮的切分函数（大小写不敏感、特殊字符）；结果分组 / 会话标题对照函数。

浏览器实测（真实执行，截图放 `server/target/wt/`）：两个会话里发不同消息，全局搜索都能搜到；点结果跳到会话并高亮那条消息；群信息里的「文件与图片」能看到图片网格和文件列表。

## 不做

- 分词 / 中文全文检索（trigram 对中文按字符工作，够用）、搜索建议、搜索历史。
- 搜索联系人 / 用户（现有加好友搜索保持不变）。
