# A05 附件进出

对应 Gitea issue xiefan/chat#6。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q18）、`CONTEXT.md`「Agent」小节、[../delivery/A01.md](../delivery/A01.md)、[../delivery/A13.md](../delivery/A13.md)（调用凭据与工具接口的鉴权方式，本票复用）。本票**没有迁移**。

## 背景

主人要能把图片和文件交给 Agent，Agent 产出的文件要作为附件发回会话。现有客户端发附件是「先上传、再发一条带 `attachment_id` 的消息」，附件消息和文字指令是两条消息。

同批并行：A03（输出方式）、A08、A09、A11（会改 `maybe_invoke` 让群/私聊也能触发——你只改 AgentConversation 这条路径上的附件收集，两边合并由主会话处理）。

## 一、产品规则（本票定）

- AgentConversation 里主人发的**附件消息不触发调用**，而是作为「待交付附件」挂在会话上；主人下一条**文字**消息触发调用时，把「上一次调用的触发消息之后、这条文字之前」主人发的附件（最多 10 个）一起交给 Agent。
- 附件消息下显示一行提示「将随下一条指令一起发给 Agent」；被某次调用带走后提示消失（可由调用记录推导）。
- 本票只做 AgentConversation；群 / 私聊里的附件不交给 Agent。

## 二、服务端

- 派发（AgentConversation 的文字触发）时按上面规则收集附件，`invocation_start` 加字段 `attachments: [{ id, name, mime, size }]`（宿主用现有的 `GET /v1/files/{id}` 下载，能力 URL 不需要鉴权）。`InvocationView` 加可选字段 `attachment_ids`，供前端判断哪些附件已被带走（可存一列，也可按规则现算，自选并在交付文档说明；若加列用迁移号 `0026_agent_attachments.sql`）。
- 产出文件上传：`POST /v1/agent-tools/files`（multipart 字段 `file`），鉴权沿用 A13 的 `Authorization: Invocation <tools_token>`。复用现有上传的校验、配额计算和缩略图逻辑（`upload_file` 那条路径，提取公用函数，不要复制一份）：上传者 = 主人，所属会话 = 发起调用的会话；超配额返回 413 `quota_exceeded`。返回 `{ attachment_id }`。
- `invocation_finish` 加可选字段 `files: [attachment_id, …]` 和 `files_skipped: <数量>`：服务端先发正文消息，再按顺序为每个附件发一条 Agent 附件消息（`kind` 按 mime 取 image / file，沿用普通附件消息的规则）；`files_skipped > 0` 时在正文末尾追加「（N 个文件因存储空间不足未发送）」。只接受属于这次调用的附件 id（上传时记下调用 id，finish 里校验）。

## 三、宿主

- 每个调用一个临时目录 `<data_dir>/inv-<id>/`，里面 `in/`（下载的附件）和 `out/`（agent 产出的文件）；调用结束后删除整个目录。
- 输入：按 `attachments` 下载到 `in/`。`RunRequest`（`runtime.rs`）加 `attachments: Vec<LocalFile { path, name, mime }>`、`outbox: Option<PathBuf>`。ACP：`initialize` 响应里 `agentCapabilities.promptCapabilities.image` 为真时，图片作为 `{"type":"image","mimeType","data":<base64>}` 内容块；其余（以及不支持图片时的图片）作为 `{"type":"resource_link","uri":"file:///…","name","mimeType","size"}`。这些块放在用户指令文本块**之前**（附加指令块仍在最前，见 A06）。恢复会话的路径同样带附件。
- 输出：子进程环境变量 `CHAT_AGENT_OUTBOX=<out 目录>`，并在指令后追加一个文本块告诉 agent「如需把文件发给用户，把文件保存到 <out 目录>」。调用成功后，把 `out/` 里的普通文件（最多 10 个，按文件名排序）逐个用 `tools_token` 上传；413 计入 `files_skipped`，其他错误记日志并跳过；上传完成后再发 `invocation_finish`（凭据只在调用进行中有效，所以必须先传再结束）。
- 假 agent：指令 `inspect` → 回出收到的每个非文本块（`image:<mime>:<字节数>` / `link:<name>:<文件是否存在>`）；`makefile:<名字>:<内容>` → 在 `CHAT_AGENT_OUTBOX` 下写文件后回 `ok`；`makebig:<名字>:<字节数>` → 写指定大小的文件（配额测试用）；启动参数 `--no-image` 让 `initialize` 不声明图片能力。

## 四、前端

- AgentConversation 里附件消息下的「将随下一条指令一起发给 Agent」提示（被带走后不再显示）。
- Agent 发回的附件消息沿用现有图片 / 文件气泡，归属按 `isOwnMessage`（左侧、Agent 标记）。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A05 =====`：

1. 发一张 PNG + 一个文本文件（附件消息都不产生调用），再发 `inspect` → 回复里有 `image:image/png:<字节数>` 和 `link:<文件名>:true`；该调用带走了这两个附件。
2. `--no-image` 的 agent：图片以 `link:` 形式出现。
3. 下一条 `inspect` 不再带上次的附件。
4. `makefile:report.txt:hello` → 先收到正文，再收到一条 Agent 的文件消息，下载内容为 `hello`；主人存储用量增加。
5. 配额很小时 `makebig:big.bin:<超额>` → 调用仍 `succeeded`，正文带「未发送」说明，没有附件消息。
6. 调用结束后用同一 `tools_token` 上传 → 401；用别的调用的附件 id 写进 finish → 被忽略。

`agent-host` 自己的测试：图片 / resource_link 块构造、outbox 收集与数量上限。浏览器实测（无头 Playwright）：专属会话里发一张图片看到提示，再发 `inspect` 看到 Agent 回复；发 `makefile:a.txt:hi` 看到 Agent 的文件卡片。截图放 `server/target/wt/`。

## 不做

- 群 / 私聊里的附件交给 Agent；语音消息交给 Agent；Agent 通过发消息工具发附件（A16）。
