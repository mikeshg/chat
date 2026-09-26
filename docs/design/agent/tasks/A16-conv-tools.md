# A16 搜索 / 读取其他会话 + 以 Agent 身份发消息

对应 Gitea issue xiefan/chat#17。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q25）、`CONTEXT.md`「Agent」小节、[../delivery/A13.md](../delivery/A13.md)（工具通道、调用凭据）、[../delivery/A12.md](../delivery/A12.md)（档位与审批）、[../delivery/A05.md](../delivery/A05.md)（产出文件上传）。本票**没有迁移**。

## 背景

Agent 要能搜索、读取主人参与的其他会话，并以自己的身份往这些会话发消息（可带附件）。发消息受权限档位管。

同批并行：A14（互调，也在往 MCP 工具表里加工具，只加你自己的条目，合并由主会话处理）。

## 一、服务端（`Authorization: Invocation <token>`，范围 = 主人本人）

- `GET /v1/agent-tools/conversations` → 主人参与的会话列表：`[{ id, kind, title, member_count, last_seq, readonly }]`（`title` 用客户端同样的规则：群名 / 私聊对方昵称 / 文件传输助手 / Agent 名），按最近活跃排序，最多 200。
- `GET /v1/agent-tools/search?q=&limit=` → 复用现有全局搜索（主人可见范围），返回 `[{ conversation_id, conversation_title, seq, sender, is_agent, created_at, text }]`。
- `GET /v1/agent-tools/conversations/{id}/messages?before_seq=&limit=` → 该会话历史（复用 A13 读历史的格式化字段），主人不是成员 403。
- `POST /v1/agent-tools/messages`，body `{ conversation_id, text, attachment_ids? }` → 以**当前调用的 Agent** 身份发到该会话：主人必须是成员；只读私聊按现有规则拒绝；`attachment_ids` 必须是本次调用上传、且上传时指定了同一目标会话的附件。返回 `{ message_id }`。
- A05 的 `POST /v1/agent-tools/files` 增加可选查询参数 `conversation_id`（缺省 = 发起调用的会话）：附件归属该会话（主人必须是成员），这样发到其他会话时附件校验能通过。

## 二、宿主

- MCP 工具表加四项：`list_conversations`、`search_messages`（`query`、`limit?`）、`read_conversation`（`conversation_id`、`before_seq?`、`limit?`）、`send_message`（`conversation_id`、`text`、`file_paths?`：宿主本地文件路径，先上传再发）。输出格式化成易读文本。
- `send_message` 受档位管：`read_only` → 先走审批（复用 A12 的 `approval_request` 帧，`tool_call` 为 `{ title: "发消息到「<会话标题>」：<正文前 80 字>", kind: "send_message", locations: [] }`，选项 `allow_once` / `reject_once`），批准才发，拒绝返回工具错误「主人拒绝了发送」；`workspace` / `full` 直接发。MCP 服务处理这次调用时需要拿到该调用的档位和审批通道（把它们随调用的映射一起登记）。
- 工具描述写清：只能访问主人参与的会话；发消息会以 Agent 身份出现。
- 假 agent：指令 `tool:list`、`tool:search:<关键词>`、`tool:read:<会话id>`、`tool:send:<会话id>:<正文>`、`tool:sendfile:<会话id>:<文件名>:<内容>`（先在临时目录写文件再用 `file_paths` 发）→ 回出工具结果文本。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A16 =====`：

1. `tool:list` 列出主人参与的群 / 私聊 / 专属会话，不含主人不在的群。
2. `tool:search:关键词` 命中主人在群里看得到的消息；搜不到主人不在的群里的消息。
3. `tool:read:<群id>` 读到历史；读主人不在的会话 403（工具返回错误文本）。
4. 档位 `workspace`：`tool:send:<群id>:hello` → 群里出现 Agent 发的 `hello`（`agent` 字段是当前 Agent），群成员收到并计未读。
5. 档位 `read_only`：同样的发送先产生审批卡片（在该 Agent 的专属会话），批准后群里出现消息；拒绝则没有消息、回复里是拒绝提示。
6. `tool:sendfile:<群id>:a.txt:hi` → 群里出现 Agent 的文件消息，下载内容为 `hi`，计入主人配额。
7. 发到只读私聊被拒；用别的调用的附件 id 被拒；调用结束后凭据 401。

浏览器实测可省略，交付文档里贴一次工具调用的请求 / 响应样例。

## 不做

- 编辑 / 删除 Agent 已发的消息的工具；给非主人参与的会话发消息（产品决定不做）。
