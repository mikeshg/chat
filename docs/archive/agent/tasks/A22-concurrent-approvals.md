# A22 同一调用的多个并发审批

来源：2026-09-28 代码审查（基于 `706dbea`）第 5 条，已由主会话核对属实。先读 [../devin-rules.md](../devin-rules.md) 和 `CONTEXT.md` 的「Agent」小节（Approval）。本票**预计不需要迁移**；确实需要时用 `0032`。

## 问题

宿主的 MCP 服务是 axum，请求并发处理：同一次调用里并行的两个 `send_message`，或 ACP `session/request_permission` 等待期间又来一个 MCP 发信，都会各自发 `approval_request`。

`server/src/approvals.rs` 的 `handle_request`：第一个请求把调用改成 `awaiting_approval`；第二个请求读到状态不是 `running` 就直接 `return`——不建审批、不发卡片、不回宿主。宿主那边第二个等待（`agent-host/src/mcp.rs` 的 `await_send_approval`）只能等到调用被取消或超时。第一个批准后 `decide` 把调用改回 `running`，也不会补发第二个。

## 要求

1. 调用处于 `running` 或 `awaiting_approval` 时收到的审批请求都要建审批、发卡片；调用状态为 `awaiting_approval`。
2. `decide` 决定一个审批后：该调用还有其他 `pending` 审批 → 保持 `awaiting_approval`；没有了 → 改回 `running`。决定结果总是转给宿主（`approval_decision`）。
3. 并发安全：「插入新审批」和「决定最后一个审批后改回 running」必须在同一把调用行锁下互斥。否则会出现卡片 `pending` 而调用已是 `running`、再也批不了（`decide` 会返回 `not_pending`）。在交付文档里写清楚加锁顺序（`decide` 目前是 `FOR UPDATE OF ap, i`）。
4. 取消 / 超时 / 宿主断开时 `expire_invocation` 把全部 pending 审批一起过期（现状如此，确认多审批下仍成立）。
5. grep 全部 `awaiting_approval`（`agents.rs`、`agent_tools.rs`、`approvals.rs`、`host_ws.rs`），确认其他地方对「一个调用多张 pending 卡」没有隐含的单审批假设。

## 测试（`it.rs`）

- 假宿主对同一调用连发两个 `approval_request` → 专属会话出现两张卡，调用 `awaiting_approval`；决定第一张 → 宿主收到第一个 `approval_decision`，调用仍 `awaiting_approval`；决定第二张 → 宿主收到第二个，调用 `running`。
- 两个 pending 时取消调用 → 两张卡都 `expired`。
- 能写的话加一个并发用例：决定最后一张与新请求同时到达，结束后不存在「调用 running 但有 pending 卡」。

## 检查

devin-rules §4 全部四项（本票不改 `agent-host/` 就跳过宿主两项，交付文档写明）。

## 不做

- 审批卡展示内容（A20）；宿主端串行化审批（服务端支持多审批后不需要）。
