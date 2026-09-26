# A14 Agent 互调

对应 Gitea issue xiefan/chat#15。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q12–Q14 与互调连带规则）、`CONTEXT.md`「Agent」小节、[../delivery/A02.md](../delivery/A02.md)（上下文）、[../delivery/A04.md](../delivery/A04.md)（排队 / 取消）、[../delivery/A12.md](../delivery/A12.md)（审批）、[../delivery/A13.md](../delivery/A13.md)（调用凭据、工具通道）、[../delivery/A17.md](../delivery/A17.md)（调用来源触发器、`deliver_invocation`）。迁移号：**`0029_agent_calls.sql`**。

## 背景

Agent 要能通过注入工具调用主人的其他 Agent（包括其他宿主上的），同步等结果或拿调用号异步查询，可以并行派出多个。子调用在原会话里的嵌套显示是 A15，本票只要把关联信息和帧准备好。

同批并行：A05（附件）收尾中、A16 稍后开始。

## 一、数据（迁移 0029）

- `agent_invocations` 加 `parent_invocation_id → agent_invocations ON DELETE SET NULL`、`depth integer NOT NULL DEFAULT 0`、`root_conversation_id uuid NULL`（根调用所在会话，子调用继承）、`result_text text NULL`（成功时的完整回复正文，截 100k 字符）。A17 的调用来源触发器改为「触发消息 / 定时任务 / 父调用 至少一个」。
- `agent_user_settings` 加 `max_call_depth integer NOT NULL DEFAULT 3`（1–10）。

## 二、服务端

- 所有调用成功结束时写 `result_text`。
- 调用凭据（A13）的 `depth` 取调用行的 `depth`（现在恒为 0，要改成按行签发）。
- 工具接口（`Authorization: Invocation <token>`，范围 = 主人本人）：
  - `POST /v1/agent-tools/invocations`，body `{ agent_name, prompt, context_id? }`：
    - 被调方按名字在主人未删除的 Agent 里找（不区分大小写），找不到 404；不能调用自己所在的这个调用链里正在运行的同一个 Agent？——**允许**（深度上限兜底）。
    - `depth = 父调用 depth + 1`，超过主人的 `max_call_depth` → 400 `depth_exceeded`。
    - 上下文：`context_id` 缺省则新生成一个（uuid），上下文键 = `call:<context_id>`（A02 机制，按被调方 Agent 区分）；返回给调用方。
    - 在**被调方的 AgentConversation** 里以被调方 Agent 身份发一条记录消息「📨 被 @<调用方名字> 调用：<prompt 前 200 字>」，作为子调用的 `trigger_message_id`；子调用 `conversation_id` = 被调方专属会话、`parent_invocation_id` = 父调用、`root_conversation_id` = 父调用的根会话（父调用没有则为父调用的会话）。
    - 走现有 `deliver_invocation`（离线排队、权限档位、审批进被调方专属会话都自然生效）。
    - 返回 `{ invocation_id, context_id, status }`。
  - `GET /v1/agent-tools/invocations/{id}?wait=<秒>`：只能查自己（当前调用）派出的子调用（`parent_invocation_id` = 当前调用）；`wait` 0–600，阻塞到终态或超时（实现用短轮询或内存通知都可以）；返回 `{ invocation_id, status, result_text?, error? }`。
- 状态帧：子调用的 `agent_invocation` 帧除了推给被调方会话成员，也推给**根会话**成员；`InvocationView` 加 `parent_invocation_id`、`root_conversation_id`、`agent_name`（前端嵌套显示用）。A03 的流式帧（`agent_stream`）对子调用同样推给根会话成员（登记运行时状态时把根会话成员并进 `member_ids`）。
- 取消级联：取消一个调用时（主人取消、使用权收回、删除 Agent 等所有取消路径），用递归查询把所有未结束的后代调用一并改为 `cancelled`、推帧、通知各自宿主取消。
- 个人设置接口与 `AgentSettings.tsx` 增加 `max_call_depth`。

## 三、宿主

- 在 A13 的 MCP 工具表里加三个工具（对应上面的接口）：
  - `call_agent`：参数 `name`、`prompt`、`wait`（默认 true）、`context_id`（可选）。`wait=true` 时创建后阻塞等待最多 600 秒（分段调用 `?wait=`），返回结果正文、状态、`context_id`；超时则返回「仍在运行」和调用号，提示用 `wait_invocation`。`wait=false` 立即返回调用号和 `context_id`。
  - `get_invocation`：参数 `invocation_id`，立即返回当前状态（有结果时带正文）。
  - `wait_invocation`：参数 `invocation_id`、`timeout_secs`（默认 300，最大 600）。
- 工具描述写清楚：只能调用主人的其他 Agent；并行派活用 `wait=false` 再逐个 `wait_invocation`。
- 假 agent：指令 `call:<名字>:<指令>` → 同步调用并回出结果正文；`callasync:<名字1>,<名字2>:<指令>` → 并行派出后逐个等待，回出各自结果（用 `|` 连接）；`callctx:<名字>:<context_id|new>:<指令>` → 带 / 不带上下文号调用并回出 `ctx=<context_id> result=<正文>`。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A14 =====`（两个宿主：同一主人，Agent `a` 在宿主 1、Agent `b` 在宿主 2）：

1. 主人对 `a` 发 `call:b:echo:hi` → `a` 的回复正文是 `hi`；`b` 的专属会话里有「📨 被 @a 调用」记录和 `b` 的回复；子调用 `parent_invocation_id` 正确。
2. `callasync:b,c:echo:x` → 两个子调用并行（总耗时明显小于串行，参照 A01 并行用例的做法用 `sleep:` 验证），回复包含两份结果。
3. 上下文号：`callctx:b:new:echo:one` 拿到 ctx，再 `callctx:b:<ctx>:recall` → 包含 `echo:one`；不带 ctx 的调用 `recall` 为 `(empty)`。
4. 深度：`max_call_depth=1` 时 `a` 调 `b`、`b` 再调 `c` → 第二层 400 `depth_exceeded`（`b` 的回复里能看到工具错误）。
5. 被调方 `b` 权限档位只读、请求写文件 → 审批卡片出现在 `b` 的专属会话；主人允许后 `a` 拿到结果。
6. `b` 的宿主离线 → 子调用 `queued`；`a` 等待中；宿主 2 上线后执行，`a` 拿到结果。
7. 取消父调用 → 子调用级联 `cancelled`。
8. 查询别的调用派出的子调用 → 404；调别人的 Agent / 不存在的名字 → 404。
9. 根会话成员收到子调用的 `agent_invocation` 帧（带 `parent_invocation_id`、`agent_name`）。

浏览器实测可省略（嵌套显示是 A15），改为在交付文档里贴一次完整的帧序列样例。

## 不做

- 原会话里的嵌套卡片 UI（A15）；调用别人的 Agent（产品决定不做）。
