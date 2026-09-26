# A15 子调用在原会话里嵌套显示

对应 Gitea issue xiefan/chat#16。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q13）、[../delivery/A03.md](../delivery/A03.md)（流式、状态表）、[../delivery/A14.md](../delivery/A14.md)（子调用的字段与帧，重点看帧样例）。本票**没有迁移**。

## 背景

A14 让 Agent 互相调用，子调用的状态帧和流式帧已经推给根会话成员，`InvocationView` 带 `parent_invocation_id`、`root_conversation_id`、`agent_name`。但前端还不会显示：子调用的 `conversation_id` 是被调方的专属会话，现在的状态行只按「本会话的触发消息」挂载。本票在原会话里，把子调用显示成父调用下可展开的嵌套卡片。

同批并行：A16（会话工具）。

## 一、服务端（小改）

- `GET /v1/conversations/{id}/agent-invocations` 同时返回 `root_conversation_id = 该会话` 的子调用（成员校验不变），刷新后能恢复嵌套。
- `InvocationView` 加可选字段 `result_preview`：子调用（有 `parent_invocation_id`）进入 `succeeded` 时带回复正文前 2000 字；其余调用和非终态为空（不要把 10 万字的 `result_text` 放进帧里）。

## 二、前端

- store：新增「父调用 id → 子调用列表」索引，由 `agent_invocation` 帧和历史接口共同维护（按调用 id 覆盖，保持派出顺序）。子调用的流式输出沿用 A03 的 `agentRuns`（按子调用的 `conversation_id` + `invocation_id` 查）。「帧 / 历史 → 调用树」写成纯函数。
- 渲染：在原会话里父调用的状态行 / 生成中气泡下面，每个子调用一张卡片，默认收起，标题行是「↳ 🤖 <agent_name> · <状态标签>」（状态标签复用 A03 的查表组件，含已用时长和当前步骤）；展开后显示：
  - 进行中且是 stream 模式：子调用的实时输出（复用 A03 的 `LiveRunBubble`）；
  - 已完成：`result_preview`（超长时显示「…」）；失败 / 取消：原因；
  - 子调用自己的子调用：递归嵌套（缩进一级，最多显示 5 层，更深的显示「还有更深的调用」）。
- 三种输出方式下都能看到子调用卡片（steps 模式没有实时输出，只显示状态和结果）。
- 取消按钮不在子卡片上（取消父调用会级联）。

## 测试

- `server/tests/it.rs` 末尾分节 `// ===== Agent A15 =====`：群里 @a 发 `call:b:echo:hi` → 群成员拉 `GET /conversations/{群}/agent-invocations` 能看到子调用（`parent_invocation_id` 指向父调用）；子调用成功帧带 `result_preview = "hi"`；非子调用的 `result_preview` 为空。
- Vitest：调用树构建（乱序到达、同一调用多次更新、多层嵌套、深度截断）。
- 浏览器实测（无头 Playwright，两个宿主或同一宿主两个 Agent）：群里 @a 发 `call:b:sleep:3000`（或 `call:b:steps:2`），群成员在父调用下看到「↳ 🤖 b · 正在处理」的卡片，完成后展开看到结果；截图放 `server/target/wt/`。

## 不做

- 在被调方专属会话里反向显示「谁调用了我」的链接（A14 已有记录消息）；调用树的独立页面。
