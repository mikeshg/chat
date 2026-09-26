# A03 三种输出方式 + 调用状态标签

对应 Gitea issue xiefan/chat#4。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（输出部分）、`CONTEXT.md`「Agent」小节、[../delivery/A01.md](../delivery/A01.md)。迁移号：**`0024_agent_steps.sql`**。

## 背景

A01 只在调用结束后发一条回复，过程中只有「⏳ 正在处理」。本票让过程可见，并支持三种输出方式。**个人默认和单个 Agent 覆盖的存储、接口、默认设置界面已经在 main 上做好了**（`agent_user_settings`、`agents.output_mode`、`GET/PUT /v1/users/me/agent-settings`、`PATCH /v1/agents/{id}`、`agents::effective_settings`、`AgentSettings.tsx`），你只做输出行为本身。单个 Agent 的覆盖编辑界面由 A06 做。

同批并行：A02、A04、A06、A13，都会碰 `agent-host/src/{runtime,acp,conn}.rs`、`server/src/agents.rs`、`ChatView.tsx`、`store.ts`。只加你需要的，不重排别人会碰的代码。A04 会给状态标签加「排队中 / 已取消 / 已超时」，所以状态标签要做成一个按 `status` 查表渲染的小组件，方便它往里加。

## 一、宿主：过程事件

- `runtime.rs` 的 `RunEvent` 加变体：`StepStart { id, title, kind }`、`StepEnd { id, ok }`。ACP 映射：`session/update` 的 `tool_call`（`toolCallId`、`title`、`kind`）→ `StepStart`；`tool_call_update` 的 `status` 变成 `completed` / `failed` → `StepEnd`。
- `conn.rs` 把事件转成宿主帧发给服务端：`{"type":"invocation_event","invocation_id":"…","event":{"type":"text","text":"…"}}`，步骤是 `{"type":"step_start","id","title","kind"}` / `{"type":"step_end","id","ok"}`。宿主端把连续的文本增量按 100ms 合并后再发。
- 假 agent 新增指令 `steps:<N>` → 依次上报 N 个步骤（每个 `tool_call` + `tool_call_update completed`，步骤之间穿插文本 chunk），最后回出 `done`；`stepfail` → 一个步骤以 `failed` 结束，然后正常回复。

## 二、服务端

- 迁移 0024：`messages.agent_steps jsonb NULL`（`[{ "title": "…", "kind": "…", "ok": true }]`）；**重建 `message_views`**，在末尾追加 `agent_steps`（只能末尾追加，参考 0019 的写法）。`MessageView` 加可选字段 `agent_steps`。
- 调用开始时用 `agents::effective_settings` 取生效的 `output_mode`，存在内存里跟这次调用走（不必落库）。
- 收到 `invocation_event` 时校验调用属于这条宿主连接且在 `running`，再按输出方式处理：
  - **stream（实时流式）**：给会话成员推临时帧 `{"type":"agent_stream","invocation_id","conversation_id","delta"?: "…","step"?: {...}}`，不写库；服务端按每 200ms 至多一帧合并。结束时照常写定稿消息，`agent_steps` 带上全部步骤。
  - **status（状态 + 最终结果）**：不推增量；步骤变化时推 `agent_invocation`，`InvocationView` 加可选字段 `current_step`（当前步骤标题，内存里的值，不落库）。结束写一条带 `agent_steps` 的结果消息。
  - **steps（每步一条消息）**：每个步骤结束时写一条 Agent 消息（内容如 `✓ 读取文件 src/main.rs`，失败用 `✗`），最后再写正文；正文消息的 `agent_steps` 为空。
- 调用结束后清掉内存里的状态。

## 三、前端

- `store.ts` 维护进行中调用的临时输出（按 `invocation_id`：累计文本、步骤列表），由 `agent_stream` 帧驱动；收到定稿消息或调用进入终态时清掉。把「帧 → 渲染状态」写成纯函数（便于 vitest）。
- **stream**：在触发消息下方显示一个正在生成的 Agent 气泡（标记为「生成中」），上方是可折叠的步骤列表（默认收起，显示「N 个步骤」），正文实时增长。
- **status**：状态标签显示「正在工作 · 已用 m:ss · 当前：<current_step>」，已用时长本地每秒刷新。
- 定稿消息上有 `agent_steps` 时，气泡上方显示可折叠的步骤记录。
- **状态标签组件**：按 `status` 查表渲染图标和文字（本票：`running` 正在处理、`succeeded` 不显示或显示已完成、`failed` 失败及原因），A04 会往表里加项。
- 刷新或换设备后，已完成调用只显示定稿，不重复出现增量。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A03 =====`：

1. stream：`steps:2` → 收到若干 `agent_stream`（有 delta、有 step），历史接口里**没有**中间内容，定稿消息 `agent_steps` 有 2 项且 `ok=true`。
2. status：设 Agent 覆盖为 `status` → 没有 `agent_stream` 帧；收到带 `current_step` 的 `agent_invocation`；结束一条结果消息。
3. steps：设个人默认为 `steps`、Agent 不覆盖 → 历史里先有 2 条 `✓ …` 消息再有正文。
4. `stepfail` → 步骤记录里该步 `ok=false`，调用仍然 `succeeded`。
5. 限频：一次调用产生的 `agent_stream` 帧数不超过「时长 / 200ms + 少量余量」。
6. 别的宿主伪造 `invocation_event` 被忽略。

Vitest：`agent_stream` 帧序列 → 渲染状态的纯函数；状态标签查表。浏览器实测（无头 Playwright）：三种方式各发一次 `steps:3`，截图生成中气泡、状态行、每步消息，放 `server/target/wt/`。

## 不做

- 取消、排队、超时的状态（A04）；子调用嵌套（A15）；审批状态（A12）。
