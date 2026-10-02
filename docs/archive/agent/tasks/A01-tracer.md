# A01 最小端到端：在专属会话里调用 Agent 并收到回复

对应 Gitea issue xiefan/chat#2。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（整体需求，本任务只做其中一小段）、`CONTEXT.md` 的「Agent」小节、`docs/adr/001-decisions.md` §12。迁移号：**`0019_agents.sql`**（追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

这是 Agent 功能的第一张票，后面 17 张票都建立在它的数据模型、宿主协议和测试接缝上。目标是打通一条最窄的完整路径：桌面端成为 AgentHost → 主人添加一个「自定义命令」的 Agent → 在它的 AgentConversation 里发消息 → 宿主用 ACP 跑这个 agent → Agent 以自己的身份回复。**只做下面写到的；** 排队、流式输出、上下文延续、权限审批、@ 调用、附件、完整配置都是后续票，不要提前做。

## 一、数据（迁移 0019）

- `agent_hosts`：`id uuid PK`（服务端生成）、`user_id → users ON DELETE CASCADE`、`install_id uuid`（桌面端生成的永久标识）、`label text ≤60`、`platform text`（`windows`/`macos`/`linux`）、`created_at`、`last_seen_at`；`UNIQUE (user_id, install_id)`。同一台电脑换用户登录是两条记录。
- `agents`：`id`、`owner_id → users ON DELETE CASCADE`、`host_id → agent_hosts ON DELETE CASCADE`、`name text`（1–32 字符，只允许字母、数字、`_`、`-`，首字符不能是 `-`）、`runtime jsonb`（本票只有 `{"command": "...", "args": ["..."]}`）、`cwd text`（宿主机上的绝对路径，可为空串 = 由宿主决定）、`created_at`、`updated_at`；主人名下 `lower(name)` 唯一。
- `conversations.type` 允许 `'agent'`；加 `agent_id uuid → agents ON DELETE CASCADE`，`type='agent'` 时每个 Agent 恰好一个（部分唯一索引）。`owner_id` = 主人，唯一成员是主人（`role='owner'`），`name` 为 NULL。
- `messages.agent_id uuid NULL → agents`：非空表示这条消息由该 Agent 发出。`sender_id` 仍填主人（保持现有 NOT NULL 与连表不变）。**重建 `message_views`**，在现有列之外加 `agent_id`、`agent_name`（其余列与顺序照旧；看 0005 / 0007 / 0009 里的写法）。
- `agent_invocations`：`id`、`agent_id → agents ON DELETE CASCADE`、`conversation_id`、`trigger_message_id`（主人的那条消息）、`status text`（CHECK 取值一次写全：`queued`、`running`、`awaiting_approval`、`succeeded`、`failed`、`cancelled`、`timed_out`；本票只用 `running` / `succeeded` / `failed`）、`error text NULL`、`created_at`、`started_at`、`finished_at`。

## 二、服务端

### REST

- `GET /v1/agent-hosts` → 本人的宿主 `[{ id, label, platform, online, last_seen_at }]`。
- `GET /v1/agents` → 本人的 Agent `[{ id, name, host_id, host_online, runtime, cwd, conversation_id, created_at }]`。
- `POST /v1/agents`，body `{ name, host_id, runtime: { command, args }, cwd }` → 上面的单个对象。同一事务里建 AgentConversation。校验：宿主属于本人（否则 404）；名字规则（400 `bad_name`）；重名不区分大小写（409 `name_taken`）；`command` 非空、≤500 字符，`args` ≤50 个。成功后给本人所有普通连接推 `Invalidate { what: "conversations" }`。
- 其他人访问不到我的宿主和 Agent（列表里没有，按 id 操作 404）。

### AgentConversation

- 会话列表里 `kind='agent'`，`ConversationView` 加可选字段 `agent: { id, name, host_online }`。
- 和 `self` 会话一样：不能拉人、踢人、退出、改名、转让、解散、建投票（400）；@ 提及解析为空；不广播 typing。把服务端所有按会话 `type` 分支的地方都过一遍（`grep -n "'dm'\|\"dm\"\|'group'\|\"group\"\|'self'\|\"self\"" server/src/*.rs`）。管理后台统计 `dm_count` / `group_count` 不含 `agent`。

### Agent 发出的消息

- `MessageView` 加可选字段 `agent: { id, name }`（旧客户端忽略）。
- 在 `domain` 里给 Agent 发消息开一个入口（复用 `post_message` 的写入与广播，`PostMessage` 加可选 `agent_id`）：
  - **不**推进主人的 `last_read_seq`（f15bd7b 为「自己发的消息」加的那段要跳过），主人未读数因此增加；
  - `broadcast_posted` 里给发送者推 `ReadSync` 的那段也要跳过；
  - 离线推送按「别人发来的消息」处理，主人会收到。
- 正文超过 4000 字符时按顺序拆成多条 Agent 消息。

### 宿主连接

- 宿主用普通的一次性 ticket 连 WS，但带 `role=host`：`GET /v1/ws?ticket=…&role=host`。连上后第一帧由宿主发：`{"type":"host_hello","install_id":"…","label":"…","platform":"windows"}`；服务端 upsert `agent_hosts`（按 `user_id + install_id`，更新 label / platform / last_seen_at），回 `{"type":"host_ready","host_id":"…"}`。第一帧不是 `host_hello`（或 10 秒内没来）就断开。
- 宿主连接放在 `AppState` 里单独的表（`host_id → 发送端`），**不**算在线状态、不参与 ClientState / 推送判定、不接收普通广播帧。同一 `host_id` 的新连接顶掉旧连接。
- 宿主上线 / 下线时给主人的所有普通连接推 `{"type":"agent_host_status","host_id":"…","online":true}`，并更新 `last_seen_at`。

### 调用

- 主人在 AgentConversation 发出一条普通文字消息（WS `send_message` 或其他发文字的路径）→ 照常入库广播 → 建一条 AgentInvocation：
  - 宿主在线：`status='running'`，给宿主发 `{"type":"invocation_start","invocation_id":"…","agent":{"id":"…","name":"…","runtime":{…},"cwd":"…"},"prompt":"<消息正文>"}`；
  - 宿主离线：直接 `status='failed'`、`error='host_offline'`（A04 会改成排队）。
- 宿主回 `{"type":"invocation_finish","invocation_id":"…","ok":true,"text":"…"}` 或 `{"ok":false,"error":"…"}`：成功 → 以 Agent 身份发出回复，`succeeded`；失败 → `failed` 并记录原因，不发回复消息。只接受这条调用所在宿主发来的 finish，其余忽略。
- 宿主断线：它上面所有 `running` 的调用改为 `failed`、`error='host_disconnected'`。
- 调用状态每次变化，给会话成员的普通连接推 `{"type":"agent_invocation","invocation":{ id, agent_id, conversation_id, trigger_message_id, status, error }}`。
- `GET /v1/conversations/{id}/agent-invocations?limit=50` → 该会话最近的调用（刷新后恢复状态显示用），仅成员可读。
- 同一 Agent 的多个调用并行，互不等待。

## 三、宿主运行时（新 crate：仓库根目录 `agent-host/`）

独立的 Rust 库，不依赖 Tauri。允许的依赖：`tokio`、`tokio-tungstenite`（开 `rustls-tls-webpki-roots`，支持 `wss://`）、`futures-util`、`serde`、`serde_json`、`uuid`、`tracing`；示例程序另可用 `reqwest`（dev-dependency）。**不要**引入 ACP 的现成 crate，JSON-RPC 自己写。

对外接口（形状可以微调，交付文档里写清最终签名）：

```rust
pub struct HostConfig { pub server_url: String, pub install_id: Uuid, pub label: String, pub platform: String }
/// 每次（重）连前调用，拿一张新的一次性 WS ticket
pub trait TicketSource: Send + Sync + 'static {
    fn ticket(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
}
pub fn spawn(cfg: HostConfig, tickets: Arc<dyn TicketSource>) -> HostHandle; // HostHandle::shutdown().await
```

- 连接：拿 ticket → 连 `role=host` → 发 `host_hello`；断线后指数退避重连（1s 起，封顶 30s），每次重拿 ticket。
- Agent 抽象：一个 trait（开始运行、事件回调、结果），本票只有 ACP 一个实现；为 ADR §12 的「原生兜底」留出同一接口。
- ACP 实现：每个调用起一个子进程（`command` + `args`，工作目录 `cwd`，空串时用宿主的数据目录），stdio 上按行分隔的 JSON-RPC 2.0：
  1. `initialize`：`{ "protocolVersion": 1, "clientCapabilities": {} }`（不声明 fs / terminal，agent 就不会反向调用它们）；
  2. `session/new`：`{ "cwd": <绝对路径>, "mcpServers": [] }`；
  3. `session/prompt`：`{ "sessionId": …, "prompt": [{ "type": "text", "text": <prompt> }] }`；
  4. 收集 `session/update` 里 `agent_message_chunk` 的文本，拼成最终回复；`session/prompt` 返回后结束子进程，回 `invocation_finish`。
  - agent 发来 `session/request_permission`：本票一律选拒绝类选项（没有就回 `cancelled`），A12 再接审批。
  - 子进程启动失败、非零退出、JSON-RPC 错误 → `ok:false`，`error` 写清原因（截 500 字符）。
- **假 ACP agent**（测试接缝，后续票都会扩展它）：`agent-host/tests/fake-acp-agent.mjs`，用 node 运行，按指令文本行事：
  - `echo:<文本>` → 分 2 个以上 `agent_message_chunk` 回出 `<文本>`；
  - `long:<N>` → 回出 N 个字符；
  - `sleep:<毫秒>` → 等待后回 `done`；
  - `fail` → 对 `session/prompt` 回 JSON-RPC 错误；`crash` → 直接非零退出；
  - 其他文本 → 回 `ok`。
- **命令行示例** `agent-host/examples/host.rs`：`cargo run --example host -- --server http://127.0.0.1:$PORT --user <用户名> --password <口令> --install-id <uuid> --label <名字>`，用 REST 登录拿 access token、按需取 ticket、跑宿主。浏览器实测和以后的验收都用它，不用起 Tauri。

## 四、Tauri 桌面端

- `app/src-tauri` 依赖 `agent-host`（path）。只在桌面平台编译宿主相关代码（`#[cfg(desktop)]`），Android 不启动。
- `install_id`：首次启动生成，存在应用数据目录的一个文件里，之后一直复用。
- 命令：`agent_host_start { server_url, label }`、`agent_host_stop`、`agent_host_ticket { request_id, ticket | error }`。TicketSource 的桌面实现：Rust 发事件 `agent-host://need-ticket { request_id }`，前端用现有 API 层（自带 token 刷新）调 `GET /v1/ws/ticket`，再调 `agent_host_ticket` 回填。
- 前端：Tauri 桌面端登录后调 `agent_host_start`（label 默认用机器名，拿不到就用「桌面端」），退出登录时调 `agent_host_stop`。Web 和 Android 什么都不做。

## 五、前端

- 「我」页加一行入口，打开新组件 `AgentsPanel`：
  - 我的宿主列表（名称、平台、在线状态，实时跟随 `agent_host_status`）；
  - 我的 Agent 列表（名字、所在宿主、在线状态），点击进入它的 AgentConversation；
  - 「添加 Agent」表单：名字、宿主（下拉，只列我的宿主）、启动命令、参数（每行一个）、工作目录；显示 400 / 409 的错误。
- 会话列表和聊天页：`kind='agent'` 的标题是 Agent 名字，图标 🤖，在线点表示宿主在线；聊天页头部副标题显示宿主名和在线状态；没有群信息按钮。
- **消息归属**：现在好几处用 `m.sender_id === me` 判断「我发的」（`store.ts`、`ws.ts`、`notify.ts`、`ChatView.tsx`）。Agent 消息的 `sender_id` 也是我，要先看 `m.agent`：有 `agent` 就不是我发的（气泡在左、显示 Agent 名字和「Agent」标记、会增加未读、会触发系统通知、我不能编辑）。把这个判断收成一个纯函数，各处复用。
- 调用状态：主人那条消息下面显示一行状态——运行中显示「⏳ 正在处理」，失败显示「⚠ 失败：<原因>」（`host_offline` → 「Agent 所在电脑不在线」，`host_disconnected` → 「Agent 所在电脑断开了」）。刷新后用 `GET /conversations/{id}/agent-invocations` 恢复。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A01 =====`。`server/Cargo.toml` 的 `[dev-dependencies]` 加 `agent-host = { path = "../agent-host" }`，测试里直接用宿主运行时 + 假 agent（`command = "node"`，`args = [<fake-acp-agent.mjs 的绝对路径>]`），TicketSource 的测试实现用该用户的 access token 调 `/v1/ws/ticket`。

1. `POST /agents`：宿主不是自己的 404；名字不合法 400；重名（大小写不同）409；别人列表里看不到、按 id 访问 404。
2. AgentConversation：会话列表里有 `kind='agent'` 且带 `agent` 字段；别人读历史被拒；拉人 / 退出 / 改名 / 建投票 400。
3. 宿主连接：宿主上线后主人普通连接收到 `agent_host_status online=true`，`GET /agent-hosts` 显示在线；宿主连接**不**让好友收到 presence；关掉宿主后收到 `online=false`。
4. 同一个 `install_id` 重新登录后再连，仍是同一个 `host_id`；另一个用户用同一个 `install_id` 连，得到不同的 `host_id`。
5. 主路径：主人发 `echo:hello world` → 收到 `agent_invocation running` → 收到 Agent 消息 `hello world`（`agent` 字段正确）→ `succeeded`；会话列表里主人的 `unread_count` 为 1；主人没收到针对这条 Agent 消息的 `read_sync`。
6. `fail` / `crash` → `failed` 且 `error` 非空，没有 Agent 消息。
7. 宿主离线时发消息 → 立即 `failed`、`error='host_offline'`。
8. `sleep:10000` 运行中关掉宿主 → `failed`、`error='host_disconnected'`。
9. `long:9000` → 按顺序收到 3 条 Agent 消息，拼起来等于原文。
10. 连发两条 `sleep:1500` → 两个调用都在约 1.5 秒内完成（并行，不是 3 秒）。
11. `GET /conversations/{id}/agent-invocations` 返回上面这些调用的最终状态；非成员被拒。

`agent-host` 自己的测试：用假 agent 跑通 `echo`、`fail`、`crash` 三种结果（不需要服务端）。

Vitest：「这条消息是不是我发的」纯函数（有无 `agent` 两种情况）；`convTitle` 对 `agent` 会话的处理。

额外检查：`cd agent-host && cargo fmt && cargo test`；`cd app/src-tauri && cargo check`。

浏览器实测（无头 Playwright，真实执行）：起槽位后端 → 用命令行示例以用户 A 身份跑一个宿主 → 浏览器登录 A → 「我」页添加一个 Agent（命令 `node`，参数是假 agent 的绝对路径）→ 进入它的专属会话发 `echo:你好` → 看到 Agent 回复「你好」且带 Agent 标记、在左侧 → 停掉宿主后再发一条，看到「Agent 所在电脑不在线」。截图放 `server/target/wt/`。

## 不做

- 排队、超时、取消（A04）；流式输出和状态标签的三种样式（A03）；上下文延续（A02）；权限档位与审批（A12）；在其他会话里 @（A11）；附件（A05）；编辑 / 删除 Agent 和其余配置项（A06）；登记表（A07）；托盘与开机自启（A09）；注入工具（A13）。
- Tauri 桌面窗口的实机验证（主会话验收时做）。
