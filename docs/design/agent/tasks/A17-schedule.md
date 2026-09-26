# A17 定时调用

对应 Gitea issue xiefan/chat#18。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q25）、`CONTEXT.md` 的 ScheduledInvocation 词条、[../delivery/A04.md](../delivery/A04.md)（排队 / 超时）、[../delivery/A13.md](../delivery/A13.md)（调用凭据、工具通道）、[../delivery/A11.md](../delivery/A11.md)（在群 / 私聊里的调用与回复）。迁移号：**`0027_agent_schedules.sql`**。

## 背景

主人可以在界面上、Agent 也可以通过工具创建定时调用；到点由服务端自动调用 Agent，结果发到指定会话（例如「每天 9 点让 research 汇总新闻发到这里」）。

同批并行：A03、A05、A07、A08（A08 在做「使用权」检查函数；你在触发时先检查主人账号未禁用、Agent 未删除，使用权检查由主会话合并时接上，交付文档里标出挂点）。

## 一、数据（迁移 0027）

- `agent_schedules`：`id`、`agent_id → agents ON DELETE CASCADE`、`owner_id → users ON DELETE CASCADE`、`name text`（1–40 字符）、`prompt text`（1–4000 字符）、`target_conversation_id → conversations ON DELETE CASCADE`、`spec jsonb`（时间表，见下）、`timezone text`（IANA 名，如 `Asia/Shanghai`）、`enabled boolean`、`next_run_at timestamptz NULL`、`last_run_at timestamptz NULL`、`created_by text`（`user` / `agent`）、`created_at`。每个主人最多 50 条。
- `agent_invocations`：`trigger_message_id` 改为可空，加 `schedule_id → agent_schedules ON DELETE SET NULL`；二者至少有一个（CHECK）。

## 二、时间表（`spec`）

四种：`{"type":"once","at":"<RFC3339>"}`、`{"type":"daily","time":"09:00"}`、`{"type":"weekly","days":[1,3,5],"time":"09:00"}`（1 = 周一）、`{"type":"interval","minutes":N}`（N ≥ 5）。按 `timezone` 计算下次运行时间（允许新增依赖 `chrono-tz`）；`once` 运行后自动停用。写成纯函数「spec + 时区 + 当前时间 → 下次运行时间」并充分单测（含夏令时切换日、跨周）。

## 三、服务端

- REST（只作用于本人）：`GET /v1/agents/{id}/schedules`、`POST /v1/agents/{id}/schedules`（`name`、`prompt`、`target_conversation_id`，缺省为该 Agent 的专属会话；主人必须是目标会话成员；只读私聊不行）、`PATCH /v1/agent-schedules/{id}`（改各字段、`enabled`）、`DELETE /v1/agent-schedules/{id}`。
- 调度：后台任务按可调间隔（`AppState` 原子量，默认 30 秒，测试调小）扫描 `enabled AND next_run_at <= now()` 的条目：先把 `next_run_at` 推进到下一次（防止重复触发，用条件更新保证同一时刻只触发一次），再为它建调用。错过多次（服务停机）只补触发一次，不回填。
- 触发时检查：主人未禁用、Agent 未删除、主人仍是目标会话成员；不满足就停用该条目并记录原因。
- 调用：走现有派发路径（宿主离线按 A04 排队与超时），`schedule_id` = 该条目、`trigger_message_id` 为空；`context_key` = `schedule:<id>`（每个定时任务自己的上下文，A02 机制）；指令 = `prompt`。Agent 回复发到目标会话，正文开头加一行「⏰ 定时任务「<name>」」。
- 注入工具（A13 通道，`Invocation` 凭据，范围 = 主人本人）：`create_schedule`（目标会话缺省为发起调用的会话）、`list_schedules`、`delete_schedule`；工具创建的条目 `created_by='agent'`。在宿主的 MCP 工具表里加这三项，服务端对应 `/v1/agent-tools/schedules` 接口。

## 四、前端

- Agent 编辑器（A06）里加「定时任务」一节：列表（名称、时间表的中文描述、下次运行时间、启用开关、删除）；新建表单（名称、指令、目标会话下拉：专属会话 + 我所在的会话、时间表类型与参数、时区默认取浏览器时区）。
- 时间表的中文描述（如「每周一、三、五 09:00」）写成纯函数。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A17 =====`（调度间隔调小；「当前时间」可以通过把 `next_run_at` 直接设成过去来模拟）：

1. 创建 / 列表 / 修改 / 删除；别人的 404；目标会话不是成员 400；超过 50 条 400。
2. 到点触发：目标会话里出现带「⏰ 定时任务」前缀的 Agent 回复；`once` 触发后停用；`daily` 的 `next_run_at` 推进到明天同一时间（按时区）。
3. 宿主离线时到点 → 调用 `queued`，上线后执行。
4. Agent 删除后不再触发；主人离开目标群后触发时停用。
5. 同一条目在一次扫描里只触发一次（并发两次扫描也只建一个调用）。
6. 工具：假 agent 通过 MCP 调 `create_schedule` 后列表里出现 `created_by='agent'` 的条目。

纯函数单测（Rust）：下次运行时间计算（每种类型、时区、夏令时）。Vitest：时间表中文描述。浏览器实测（无头 Playwright）：在 Agent 编辑器里建一个每天的定时任务，看到下次运行时间；截图放 `server/target/wt/`。

## 不做

- cron 表达式；失败重试策略；定时任务的执行历史页面（调用记录已有）。
