# A23 宿主掉线后补发结果 + 取消先于启动

来源：2026-09-28 第二轮代码审查（基于 `5317e8d`）第 1、2 条，主会话已核对属实。**用户决定：宿主掉线不终止 Agent，结果在重连后补发。** 先读 [../devin-rules.md](../devin-rules.md)、`CONTEXT.md` 的「Agent」小节（AgentHost、Invocation、Approval）。**迁移编号：`0032`**（需要时用）。

两件事放在一张票里，是因为它们都在改「调用的生命周期不绑在一条宿主连接上」，而且互相影响（例如宿主离线期间取消的调用，要在重连时补发取消）。

## 现状

1. **宿主**（`agent-host/src/conn.rs`）：`connect_once` 断线时只 `writer.abort()` 和 `s.approvals.clear()`。进行中的调用任务拿着旧连接的 `out` 发送端，之后的事件、`invocation_finish`、`approval_request` 都发进已经关掉的通道，全部丢失；`approvals.clear()` 让等审批的调用直接失败。
2. **服务端**（`server/src/host_ws.rs`）：宿主断开时 `fail_running` 立刻把 `running` / `awaiting_approval` 置为 `failed/host_disconnected` 并过期审批；之后 `handle_finish` 只受理进行中的调用。
3. **服务端重启**：`inv_runs` 全丢，启动时不处理进行中的调用；完成帧丢失的调用会一直停在 `running`。运行中的调用没有任何超时。
4. **取消先于启动**：
   - 立即派发：调用行先以 `running` 落库，`deliver_invocation`（`server/src/agents.rs`）再经过多次 DB await 才发 `invocation_start`。这期间取消会先发 `invocation_cancel`，宿主读循环对还没登记的 id 直接忽略，随后照常执行 start。另外取消里的 `inv_runs.remove` 可能早于 `register_run`，留下泄漏的运行时条目，已取消的调用还会继续推流式帧。
   - 排队转运行：`cancel_invocation` 先读到 `queued`，`flush_queued` 随即把它改成 `running` 并派发，取消的 UPDATE 仍然成功，但因为读到的旧状态是 `queued` 而不发 `invocation_cancel`。
   - 宿主离线时的取消帧直接丢失。

## 要求

### 一、掉线不终止，重连后补发

1. 宿主：调用任务不绑定某一条连接。出站帧走一条跨连接的通道：在线时立即发送；离线时 `invocation_finish`、`approval_request` 暂存，下次 `host_ready` 之后补发（同一调用的顺序不变），`invocation_event` 离线期间可以丢弃（finish 里有完整正文和步骤）。
2. 宿主：断线不再清空等待中的审批，等重连后的决定，或者调用被取消。
3. 宿主：`host_hello` 增加可选字段，带上还在进行中的调用 id（含 finish 已生成但还没送达的）和等待中的审批 `request_id`。旧宿主不带这些字段，服务端按空处理。
4. 服务端：宿主断开时不再立刻判失败，进行中的调用保持原状态，审批保持 `pending`；记下宿主离线的时间作为宽限期起点。宿主离线期间主人做出的审批决定，要在宿主重连后送达（例如 `host_ready` 时按宿主报告的 `request_id` 补发已决定的结果）。
5. 服务端：`host_ready` 时对这台宿主名下 `running` / `awaiting_approval` 的调用做核对：
   - 宿主报告在跑 → 保持；`inv_runs` 里没有（服务端重启过）就重建，保证之后的事件和 finish 能被受理；
   - 宿主没报告 → `failed/host_disconnected`，过期审批，和现在的 `fail_running` 一样推帧、级联；
   - 宿主报告在跑、但库里已是终态（离线期间被取消、超时等）→ 给宿主发 `invocation_cancel`。
6. 宽限期：宿主 10 分钟内没重连，进行中的调用判 `failed/host_disconnected`。服务端重启后也要成立（内存状态没了，起点要能从数据库推出来）。宽限期做成 `AppState` 里可调的原子量（仿 `queue_sweep_interval_ms`），测试调小；清扫可以复用 `spawn_queue_sweeper` 的循环。
7. 界面不用改：宿主离线期间调用仍显示运行中，宿主离线状态已有 `agent_host_status` 推送。

### 二、取消先于启动

8. 宿主：收到还没登记的调用的 `invocation_cancel`，记为「已取消」（有上限，例如 10 分钟或最近 256 个）；之后同一 id 的 `invocation_start` 直接丢弃，不起 Agent、不回 finish。
9. 服务端：`cancel_invocation` 不能靠 UPDATE 之前读到的旧状态决定是否通知宿主。调用可能已经派发时一律发 `invocation_cancel`（宿主有第 8 条兜底，多发无害）。宿主离线时由第 5 条在重连时补发。
10. 被取消的调用不能残留 `inv_runs` 条目，取消之后不再推它的流式帧。

## 测试

- agent-host：用 `tokio_tungstenite` 起一个本地假 WS 服务端（依赖里已有）配合 `tests/fake-acp-agent.mjs`：
  - 调用进行中断线 → Agent 进程不被杀；重连后新连接上收到 `host_hello`（带在跑的 id）和该调用的 `invocation_finish`；
  - 先收 `invocation_cancel` 再收 `invocation_start` → 不起 Agent、不回 finish；
  - 等审批时断线 → 重连后收到决定，调用继续完成。
- server `it.rs`（复用 `spawn_raw_host` / `RawHostWs`）：
  - 运行中断线 → 状态仍为 `running`；重连并报告该 id → 补发的 finish 被受理，调用 `succeeded`、回复消息落库；
  - 重连但不报告 → `failed/host_disconnected`；
  - 离线期间取消 → 重连报告该 id 时收到 `invocation_cancel`；
  - 离线期间决定审批 → 重连后收到 `approval_decision`；
  - 宽限期到期 → `failed/host_disconnected`；
  - 排队转运行与取消并发 → 宿主要么收不到 start，要么收到 start 之后收到 cancel。

## 检查

devin-rules §4 全部四项。

## 不做

- 离线期间的流式增量不补发。
- 产出文件上传（HTTP）因网络失败不重试，记进已知限制。
- 撤销设备与宿主握手复核（A24）。
