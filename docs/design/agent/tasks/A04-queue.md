# A04 宿主离线排队、超时、取消

对应 Gitea issue xiefan/chat#5。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)、`CONTEXT.md`「Agent」小节、[../delivery/A01.md](../delivery/A01.md)。迁移号：**`0021_agent_queue.sql`**。

## 背景

A01 在宿主离线时直接 `failed/host_offline`。本票改成排队、上线后执行、超时自动取消，并让主人随时能取消调用。**排队超时的个人默认和单个 Agent 覆盖已经在 main 上做好了**（`agent_user_settings.queue_timeout_secs`、`agents.queue_timeout_secs`，0 = 永不；`agents::effective_settings` 取生效值；默认设置界面 `AgentSettings.tsx`），你只做排队行为本身。

同批并行：A02、A03、A06、A13。A03 会把状态标签做成按 `status` 查表的组件，你往表里加「排队中 / 已取消 / 已超时」；两边改到同一处时保持查表结构，合并由主会话处理。

## 一、数据（迁移 0021）

`agent_invocations` 加 `queue_deadline timestamptz NULL`（排队截止时间，NULL = 永不超时）。

## 二、服务端

- 派发时宿主离线：`status='queued'`，`queue_deadline = now() + 生效超时`（生效超时为 0 时为 NULL）。推 `agent_invocation`。
- 宿主上线（`host_ready` 之后）：按 `created_at` 顺序把该宿主上所有 Agent 的 `queued` 调用逐个改成 `running`（`started_at = now()`）并下发 `invocation_start`（prompt 取触发消息的正文）。
- 超时清扫：后台任务定期把 `queued` 且 `queue_deadline < now()` 的调用改成 `timed_out` 并推帧。扫描间隔放在 `AppState` 里一个可调的原子量（参考 `ws_ping_interval_ms` 的做法），默认 30 秒，测试里调小。
- 取消：`POST /v1/agent-invocations/{id}/cancel`，只有该调用所属 Agent 的主人能调（别人 404）：
  - `queued` → `cancelled`；
  - `running` / `awaiting_approval` → `cancelled`，并给宿主发 `{"type":"invocation_cancel","invocation_id":"…"}`；
  - 已是终态 → 409 `already_finished`；
  - 取消之后宿主再发来的 `invocation_finish` / 其他事件一律忽略，不发回复。
- 宿主断线：只把 `running` 和 `awaiting_approval` 的调用改成 `failed/host_disconnected`；`queued` 的保持排队（A01 目前会把所有未结束的都改掉，要收窄）。
- 同一 Agent 的多个调用照旧并行。

## 三、宿主

- 收到 `invocation_cancel`：先给 agent 发 ACP 通知 `session/cancel`（`{"sessionId": …}`），最多等 5 秒让 `session/prompt` 返回，然后结束子进程；这次调用**不**再发 `invocation_finish`。现在 `conn.rs` 收到取消会直接 abort 任务，要改成上面的顺序（在 `acp.rs` 里给运行中的调用留一个取消入口，例如 `RunRequest` 里带一个取消信号）。
- 假 agent：收到 `session/cancel` 时让进行中的 `sleep:` 立刻以 `stopReason: "cancelled"` 返回；新增启动参数 `--ignore-cancel`，用来测试 5 秒后强制结束。

## 四、前端

- 状态标签加：`queued` →「⏸ 排队中：Agent 所在电脑不在线」、`cancelled` →「已取消」、`timed_out` →「排队超时，已取消」。
- 主人可见的「取消」按钮：`queued` 和 `running`（以及 `awaiting_approval`）时显示，点击调取消接口；409 时静默刷新状态。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A04 =====`：

1. 宿主离线发消息 → `queued`；宿主上线后 → `running` → 收到回复 → `succeeded`。
2. 离线时连发两条 → 上线后按顺序各得到一条回复。
3. Agent 覆盖超时为 2 秒、清扫间隔调小 → `timed_out`；之后宿主上线不再执行它。
4. 个人默认 0（永不）→ 等待超过清扫间隔后仍是 `queued`。
5. 取消 `queued` → `cancelled`，上线后不执行；取消 `running`（`sleep:10000`）→ `cancelled`，没有回复消息；取消已完成的 → 409；别人取消 → 404。
6. `--ignore-cancel` 的 agent：取消后 5 秒左右子进程被结束（宿主不再上报该调用）。
7. 运行中断开宿主：`running` → `failed/host_disconnected`，同时排队中的另一条调用保持 `queued`，宿主重连后被执行。

浏览器实测（无头 Playwright）：宿主离线时发一条看到「排队中」和取消按钮；启动宿主后自动得到回复；再发 `sleep:20000` 点取消看到「已取消」。截图放 `server/target/wt/`。

## 不做

- 审批状态的展示与流程（A12）；子调用级联取消（A14）；排队顺序以外的调度策略。
