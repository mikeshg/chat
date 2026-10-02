# A12 PermissionTier + Approval

对应 Gitea issue xiefan/chat#14。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q8、Q10）、`CONTEXT.md` 的 PermissionTier / Approval 词条与「审批」规则、[../delivery/A01.md](../delivery/A01.md)、[../delivery/A03.md](../delivery/A03.md)（状态表）、[../delivery/A04.md](../delivery/A04.md)（取消）、[../delivery/A11.md](../delivery/A11.md)（群里调用）。迁移号：**`0028_agent_approvals.sql`**。

## 背景

现在宿主对 ACP 的 `session/request_permission` 一律选拒绝。本票加权限档位：档位内自动放行；超出的在该 Agent 的 AgentConversation 里发审批卡片，主人在任意客户端决定，agent 等待。

同批并行：A05（附件）、A07（登记表）、A17（定时调用）。

## 一、数据（迁移 0028）

- `agents.permission_tier text NOT NULL DEFAULT 'read_only'`，取值 `read_only` / `workspace` / `full`（新建 Agent 默认只读，主人可改）。
- `agent_approvals`：`id`、`invocation_id → agent_invocations ON DELETE CASCADE`、`message_id → messages`（卡片消息）、`request jsonb`（`title`、`kind`、`locations`、原始 toolCall 的精简摘要）、`options jsonb`（agent 给的选项 `[{optionId,name,kind}]`）、`status`（`pending` / `decided` / `expired`）、`decision text NULL`（选中的 optionId）、`decided_at`、`created_at`。

## 二、宿主

- `invocation_start` 的 agent 快照加 `permission_tier`（服务端下发），`RunRequest` 带上。
- 收到 `session/request_permission` 时按档位判断（ACP 的 `toolCall.kind` 取值 read / edit / delete / move / search / execute / think / fetch / switch_mode / other）：
  - `read_only`：read、search、think、fetch 自动允许；其余走审批。
  - `workspace`：在只读基础上，edit / delete / move 且所有 `locations` 都在工作目录或额外目录内 → 自动允许；否则走审批。
  - `full`：全部自动允许。
  - 自动允许 = 选 agent 给的 `allow_once` 选项（没有就选第一个 allow 类选项）。
- 走审批：给服务端发 `{"type":"approval_request","invocation_id","request_id","tool_call":{title,kind,locations},"options":[…]}`，然后等服务端回 `{"type":"approval_decision","request_id","option_id"}`，再把选择回给 agent。等待期间调用被取消 → 回 `cancelled`（A04 的取消流程照常结束进程）。
- 假 agent：指令 `perm:<kind>:<路径>` → 发一个 `session/request_permission`（toolCall 带该 kind 与 locations，选项 `allow`（allow_once）/ `always`（allow_always）/ `reject`（reject_once）），收到结果后回出 `allowed:<optionId>` 或 `rejected`。

## 三、服务端

- 收到 `approval_request`（校验调用属于这条宿主连接且在 `running`）：
  1. 调用状态改为 `awaiting_approval`，推帧（原会话成员都看得到这个状态）；
  2. 在该 Agent 的 **AgentConversation** 里以 Agent 身份发一条审批卡片消息（`kind='approval'`，正文是可读摘要，如「请求执行：写入 src/main.rs」），`MessageView` 加可选字段 `approval: { id, status, request, options, decision }`。它是 Agent 发出的消息，所以主人会有未读和通知；
  3. 调用发生在群 / 私聊时，原会话里**不**出现命令内容，只有状态「等待主人批准」。
- `POST /v1/agent-approvals/{id}/decide`，body `{ "option_id": "…" }`：只有 Agent 主人（别人 404）；只能决定 `pending` 的（否则 409）；`option_id` 必须是选项之一（400）。成功后：审批变 `decided`，卡片消息推 `message_updated`（`approval.status/decision` 变化），调用回到 `running`，给宿主发 `approval_decision`。
- 调用被取消 / 失败 / 宿主断线时，未决审批改为 `expired` 并更新卡片。
- 状态表加 `awaiting_approval` →「⏸ 等待主人批准」（前端 A03 的 `invocationStatusView`）。
- `PATCH /v1/agents/{id}` 支持 `permission_tier`，`AgentView` 带上。

## 四、前端

- 审批卡片（AgentConversation 里 `kind='approval'` 的消息）：显示请求标题、类型、涉及路径，按 agent 给的选项渲染按钮（`allow_once` 显示「本次允许」、`allow_always`「总是允许」、`reject_once`「拒绝」，其余用 agent 给的 name）；决定后显示结果；`expired` 显示「已失效」。
- 创建 Agent 表单与 Agent 编辑器加权限档位选择（只读 / 可改工作目录 / 完全放开，附一句说明）。
- 状态行 `awaiting_approval` 在专属会话里附「去审批」可跳到卡片（同会话滚动定位即可）；群里只显示文字。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A12 =====`：

1. 只读档位：`perm:read:/x` 自动允许（回复 `allowed:allow`，没有审批卡片）；`perm:edit:<工作目录内>` 生成审批卡片，调用变 `awaiting_approval`。
2. 主人决定 `allow` → 回复 `allowed:allow`、调用 `succeeded`、卡片 `decided`；决定 `reject` → `rejected`。
3. `workspace` 档位：工作目录内 edit 自动允许；目录外 edit 走审批；execute 走审批。`full`：全部自动允许。
4. 群里调用需要审批：卡片只出现在 AgentConversation；群成员收到的只有 `awaiting_approval` 状态帧，群历史里没有命令内容。
5. 别人决定 404；重复决定 409；非法 option 400。
6. 等待审批时取消调用 → 审批 `expired`，卡片更新，没有回复。
7. 审批卡片计入主人未读。

Vitest：选项 → 按钮文案；档位说明。浏览器实测（无头 Playwright）：专属会话里发 `perm:edit:…` 看到卡片，点「本次允许」后看到 Agent 回复；群里调用时群成员只看到「等待主人批准」。截图放 `server/target/wt/`。

## 不做

- 用 ACP 会话模式表达档位（各 agent 模式命名不统一，先不做，交付文档记一笔）；发消息工具的档位控制（A16）；「总是允许」的持久化规则（选项原样交给 agent，由 agent 自己记住）。
