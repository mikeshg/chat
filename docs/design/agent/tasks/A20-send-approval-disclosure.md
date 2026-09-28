# A20 发消息审批完整披露正文与附件

来源：2026-09-28 代码审查（基于 `706dbea`）第 4 条，已由主会话核对属实。先读 [../devin-rules.md](../devin-rules.md) 和 `CONTEXT.md` 的「Agent」小节（Approval、PermissionTier）。本票**没有迁移**。

## 问题

只读档（`read_only`）下，Agent 通过 MCP 工具 `send_message` 发消息要先经主人审批，这是只读档唯一的外发关口。但审批卡没有展示实际要发出去的内容：

- `agent-host/src/mcp.rs` 的 `send_message` 生成的标题只有目标会话名和 `preview80(text)`（正文前 80 字），`await_send_approval` 的 `locations` 固定为 `[]`；
- 主人批准后才读取并上传 `file_paths` 里的全部本机文件；
- 服务端 `server/src/approvals.rs` 的 `sanitize_request` 把 `title` 截到 300 字，`locations` 最多 20 条、每条 500 字。

结果：「批准发一句话」和「同时外发 80 字以后的正文 + 任意本机文件」在卡片上看不出区别。

## 要求

1. 审批卡必须展示**完整正文**和**全部待上传文件的路径**，并且展示的内容就是批准后实际发送的内容。
   - 路径展示成实际会被读取的绝对路径（用 `std::path::absolute`，与 `upload_local` 读取时的解析基准一致）。
   - 附件路径放进审批请求的 `locations`（`ApprovalCard.tsx` 已会显示 `路径：…`）。
   - 正文放进审批请求的新字段 `detail`：`sanitize_request` 保留它（上限 4000 字，与消息正文上限一致），`ApprovalCard.tsx` 在卡片里完整展示（保留换行，长文可折叠但不能丢内容）。旧客户端忽略该字段即可。
2. 展示不下就不发审批：`file_paths` 超过服务端 `locations` 上限条数、单条路径超过长度上限、正文超过 4000 字 → 直接给 Agent 返回错误，不上传、不发审批。
3. 批准后只上传审批时展示的那一份路径清单（同一个 `Vec`，不重新解析参数）。
4. `workspace` / `full` 档直接发送的行为不变。

## 测试

- agent-host：审批请求的 `tool_call` 带完整 `detail` 和绝对路径 `locations`；超限直接报错且没有发出审批通知。`agent-host/tests/mcp.rs` 的 `send_message_read_only_reject_does_not_post` 有审批通道的测试写法，照着来。
- server `it.rs`：宿主上报带 `detail` 的 `approval_request` → 卡片消息的 request 里 `detail` 完整保留（超 4000 字被截断）。
- vitest：`ApprovalCard` 展示 `detail` 全文与路径。

## 检查

devin-rules §4 全部四项。

## 不做

- `file_paths` 相对路径的解析基准（现在是宿主进程当前目录，不是 Agent 工作目录；另议，发现的话记进交付文档）。
- 并发审批（A22）。
