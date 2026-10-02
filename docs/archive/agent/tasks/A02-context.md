# A02 AgentContext：同一会话接着聊，可开新话题

对应 Gitea issue xiefan/chat#3。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)、`CONTEXT.md` 的「Agent」小节（AgentContext 词条）、[../delivery/A01.md](../delivery/A01.md)（A01 的实际实现与协议）。迁移号：**`0022_agent_contexts.sql`**。

## 背景

A01 每次调用都起一个全新的 ACP 会话，Agent 不记得上一句。本票让同一个会话里的调用接上之前的 ACP 会话；主人可以「开新话题」清空。

同批并行的票：A03（输出方式）、A04（排队/取消）、A06（完整配置）、A13（注入工具）。它们也会改 `agent-host/src/{runtime,acp,conn}.rs`、`server/src/agents.rs`、`ChatView.tsx`。**只加你需要的字段和分支，不要重排或改写别人会碰的代码。**

## 一、数据（迁移 0022）

`agent_contexts`：`agent_id → agents ON DELETE CASCADE`、`context_key text`、`runtime_session_id text`、`updated_at`；主键 `(agent_id, context_key)`。本票的 `context_key` = 发起调用的会话 id（字符串）。A14 以后会用调用方自选的上下文号，所以键用 text、不要写死成 uuid 外键。

## 二、协议

- `invocation_start` 加字段 `context: { key, session_id }`：`session_id` 是 `agent_contexts` 里存的运行时会话 id，没有就是 `null`。
- `invocation_finish` 加可选字段 `session_id`：本次实际用的运行时会话 id。服务端收到成功的 finish 时 upsert `agent_contexts`（失败的调用不更新）。

## 三、宿主（agent-host）

- `RunRequest` 加 `resume_session: Option<String>`；`RunOutput` 加 `session_id: Option<String>`（`runtime.rs`）。
- ACP：`initialize` 响应里读 agent 能力：
  - `resume_session` 有值且 agent 声明 `sessionCapabilities.resume` → `session/resume`（参数 `sessionId`、`cwd`、`mcpServers`）；
  - 否则声明了 `loadSession: true` → `session/load`。**load 会先把历史用 `session/update` 回放一遍**，回放期间（`session/load` 响应返回之前）收到的 `agent_message_chunk` 不能算进回复正文、不能发 `RunEvent`；
  - 都不支持，或 resume / load 返回错误 → 退回 `session/new`，不报错；
  - 最终用的会话 id 写进 `RunOutput.session_id`。
- 每次调用仍然起一个新进程（A01 的做法不变），只是换成恢复会话。
- 假 agent（`agent-host/tests/fake-acp-agent.mjs`）要能跨进程保留会话：把会话状态存到临时目录下按 sessionId 命名的文件里。新增：
  - 启动参数 `--caps=resume|load|none`（默认 `resume`）控制 `initialize` 里声明的能力；
  - 指令 `recall` → 回出本会话之前收到过的全部指令（用 `|` 连接），新会话回 `(empty)`；
  - `load` 模式下，恢复时要先用 `session/update` 回放历史（带 `agent_message_chunk`），用来验证回放不会混进回复。

## 四、服务端

- 派发时按 `(agent_id, 会话 id)` 查出 `session_id` 放进 `invocation_start`。
- `POST /v1/conversations/{id}/agent-context/reset`：只有该会话的成员能调用（本票只有 AgentConversation，成员就是主人），删掉这个会话对应的 `agent_contexts` 行，返回 204。非 agent 会话 400。

## 五、前端

- AgentConversation 头部加「开新话题」按钮 → 调用重置接口，成功后 toast「已开始新话题」，并在消息流里插一条本地的分隔提示「—— 新话题 ——」（只在本设备当次显示即可，不落库）。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A02 =====`，复用 A01 的 helper（`spawn_host`、`create_agent`、`until_frame`、`send_ws`）；创建 Agent 时参数加上 `--caps=...`（需要的话给 `create_agent` 加一个带参数的变体，不要改原函数签名）。

1. 延续：同一 AgentConversation 先发 `echo:one`，再发 `recall` → 回复包含 `echo:one`。
2. 重置：reset 后再发 `recall` → `(empty)`；非成员调用 reset 被拒；对非 agent 会话 400。
3. `--caps=load`：延续同样成立，且回复里**不**包含回放出来的旧回复正文。
4. `--caps=none`：第二次调用正常成功（退回新会话），`recall` 为 `(empty)`。
5. 两个不同 Agent 的上下文互不影响。
6. 失败的调用（`fail`）不覆盖已有的 `session_id`：之后 `recall` 仍能接上。

`agent-host` 自己的测试：resume / load / none 三种能力下 `RunOutput.session_id` 与回放过滤正确。

## 不做

- 调用方自选上下文号（A14）；在群/私聊里的独立上下文（A11 复用本票机制）。
- 长驻 agent 进程 / 进程池。
