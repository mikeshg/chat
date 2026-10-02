# A21 进行中调用：成员变更撤销接收者 + 开新话题不被旧调用撤销

来源：2026-09-28 代码审查（基于 `706dbea`）第 1、6 条，已由主会话核对属实。先读 [../devin-rules.md](../devin-rules.md) 和 `CONTEXT.md` 的「Agent」小节（AgentContext、Invocation）。**迁移编号：`0031`**（需要时用；用不到就不建）。

## 一、退群 / 被踢 / 解散后不再收到进行中调用的流式输出（审查第 1 条）

`server/src/agents.rs` 的 `register_run` 把派发时的成员列表（含 `with_root_members` 加进来的根会话成员）存进 `InvRun.member_ids`，之后 `push_stream`、`push_current_step` 一直发给这份快照。`server/src/api.rs` 的 `leave_conversation`、`kick_member`、解散群（`DELETE FROM memberships WHERE conversation_id = $1`）都只通知刷新会话列表，不碰 `st.inv_runs`。被移出的人只要在线，就继续收到那次调用剩下的文本和步骤帧。

要求：

- 成员被移出会话后，所有 `conversation_id` 或根会话（`inv.root_conversation_id`）是该会话的进行中调用，接收者要重算为「调用会话当前成员 ∪ 根会话当前成员」。仍在另一个会话里的人不能被误删。
- 不要在 `push_stream` 里每帧查库（流式每 200ms 一帧）。在成员变更处理完、事务提交后更新 `inv_runs` 即可。
- grep 所有删除 `memberships` 的路径（至少退群、踢人、解散、最后一人退群删会话），都要接上。
- 测试（`it.rs`）：群里主人 @ 自己的 Agent，用假宿主 WS 连接推 `invocation_event` 文本；踢掉 B 后再推一段 → B 收不到这段，A 能收到；B 退群的情况同样覆盖。已有 A03 的假宿主流式测试可参考。

## 二、「开新话题」不被旧调用完成后的写回撤销（审查第 6 条）

`reset_context` 只 `DELETE FROM agent_contexts`；`handle_finish` 在调用成功时无条件 upsert `runtime_session_id`；界面上的「开新话题」按钮在调用运行中也能点（`app/src/ui/ChatView.tsx`，只在 `topicBusy || agentClosed` 时禁用）。于是：运行中点开新话题 → 旧调用完成时把旧会话 id 写回 → 下一次调用又续上旧话题。

要求：

- 重置之前派发的调用，完成时不得写回该上下文；重置之后派发的调用照常写回（多个调用之间仍是后完成者覆盖，行为不变）。
- 专属会话与私聊 / 群（只清请求者自己的 Agent）两条重置路径都要覆盖；定时调用的 `schedule:<id>` 上下文不受重置影响。
- 实现方式自定（例如上下文代次 / 重置时间戳，在派发读取运行时会话时记下，完成时比对），注意派发读会话、重置、完成写回三者的并发顺序，在交付文档里说明为什么没有竞态。
- 界面不改（运行中允许开新话题是有意的）。
- 测试（`it.rs`）：调用 1 运行中 → 重置 → 调用 1 成功完成并带 `session_id` → 调用 2 派发时 `invocation_start` 不带旧会话 id；重置后派发的调用完成写回正常，下一次能续上。

## 检查

devin-rules §4 全部四项（本票不改 `agent-host/` 就跳过宿主两项，交付文档写明）。

## 不做

- 并发审批（A22）；发消息审批披露（A20）。
