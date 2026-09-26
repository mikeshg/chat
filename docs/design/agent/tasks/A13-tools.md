# A13 工具注入通道 + 读取当前会话历史

对应 Gitea issue xiefan/chat#11。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（注入工具部分）、`CONTEXT.md`「Agent」小节、`docs/adr/001-decisions.md` §12、[../delivery/A01.md](../delivery/A01.md)。本票**没有迁移**。

## 背景

spec 要求宿主给每个 Agent 注入一组聊天工具（互调、读历史、搜索、发消息、定时调用）。本票搭通道并实现第一个工具「读取发起调用的会话的历史」；A14 / A16 / A17 在同一通道上加工具。通道的安全边界是本票的重点：工具只能以**主人本人的权限范围**访问服务端，调用结束即失效。

同批并行：A02、A03、A04、A06，都会碰 `agent-host/src/{runtime,acp,conn}.rs`、`server/src/agents.rs`。只加你需要的。

## 一、服务端

- **调用专用凭据**：派发时签发一个 JWT（用现有的 `jwt_secret`），claims 至少含 `kind: "invocation"`、调用 id、主人 id、Agent id、深度（本票恒为 0），随 `invocation_start` 下发（字段 `tools_token`）。
- 校验（提成一个 extractor 或函数，后续工具复用）：`Authorization: Invocation <token>`；签名与 `kind` 正确；调用存在且状态为 `running` 或 `awaiting_approval`；主人账号未被禁用。任何一条不满足 → 401。普通用户 token 不能当调用凭据用，调用凭据也不能访问普通 REST 接口（`require_user` 要拒绝 `kind: "invocation"` 的 token，写测试证明）。
- 第一个工具接口：`GET /v1/agent-tools/history?before_seq=&limit=`（limit 默认 50、上限 200）→ 发起调用的那个会话的消息分页（复用现有历史分页的读模型），额外校验主人仍是该会话成员。响应里每条消息带发送者展示名、是否 Agent、seq、时间、正文（附件给占位描述）。

## 二、宿主：本地 MCP 服务

- 宿主进程内起一个只监听 `127.0.0.1` 随机端口的 HTTP MCP 服务（MCP Streamable HTTP：`POST` JSON-RPC，按请求直接返回 JSON 响应即可，不需要 SSE 推送）。每个调用分配一个随机路径 id（`/mcp/<id>`），内存里映射到该调用的 `tools_token`；调用结束（成功、失败、取消）立刻删除映射，之后访问 404。**凭据不出现在 URL 里。**
- MCP 方法：`initialize`、`notifications/initialized`、`tools/list`、`tools/call`。本票的工具：`read_conversation_history`（参数 `before_seq?`、`limit?`；把服务端返回的消息格式化成易读文本，例如 `[seq 12] 张三: …`）。工具定义和分发写成一张表，后续票往里加。
- 交给 agent：`runtime.rs` 的 `RunRequest` 加 `mcp_servers`（宿主构造好的 ACP mcpServers 数组），`acp.rs` 在 `session/new`（以及 A02 的恢复路径会用同一个值，你只管 `session/new`）里带上：
  - agent 在 `initialize` 响应里声明 `agentCapabilities.mcpCapabilities.http` → `{"type":"http","name":"chat","url":"http://127.0.0.1:<port>/mcp/<id>","headers":[]}`；
  - 否则（只支持 stdio）→ 用桥接命令：`{"name":"chat","command":<桥接程序>,"args":[…,"<url>"],"env":[]}`。桥接程序把 stdin 上的 JSON-RPC 行转发成对该 URL 的 `POST`，把响应写回 stdout（通知类消息没有响应）。
  - `HostConfig` 加 `mcp_bridge: Option<(String, Vec<String>)>`：嵌入方告诉宿主「怎么启动桥接」。没有桥接又不支持 http → 不注入工具，调用照常运行（记一条 warn）。
- 桥接实现放在库里（例如 `agent_host::mcp_bridge::run(url)`），再提供三个入口：`agent-host` 的一个 bin 目标 `agent-mcp-bridge`（给 `agent-host` 自己的测试用）；Tauri 桌面端 `main` 收到 `--agent-mcp-bridge <url>` 时直接跑桥接后退出（不起窗口），`HostConfig.mcp_bridge` 设为「当前可执行文件 + 该参数」；命令行示例 `examples/host.rs` 同理。
- 允许新增的依赖：`agent-host` 可加 `axum`（与服务端同为 0.8，用于本地 MCP 服务）和 `reqwest`（rustls、json，用于调用服务端）作为正式依赖。
- 假 agent：新增启动参数 `--mcp=http|stdio`（控制 `initialize` 里是否声明 http 能力）；指令 `tool:read_history` → 按收到的 mcpServers 连接（http 直接 `fetch`，stdio 起子进程），`tools/call read_conversation_history`，把工具结果原样回出。

## 三、测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A13 =====`：

1. 主人在 AgentConversation 先发两条普通消息，再发 `tool:read_history`（`--mcp=http`）→ 回复里包含前两条消息的正文。
2. 调用结束后，用同一个 `tools_token` 直接请求 `/v1/agent-tools/history` → 401。
3. 普通 access token 请求 `/v1/agent-tools/history` → 401；调用凭据请求普通接口（如 `GET /v1/conversations`）→ 401。
4. 伪造 / 过期 / 签名错的凭据 → 401；主人被禁用后凭据 → 401。
5. 凭据只能读发起调用的会话：本票接口没有会话参数，另外用主人不在的会话 id 构造不出越权（在测试里说明并断言返回的都是发起会话的消息）。

`agent-host` 自己的测试：本地 MCP 服务 `tools/list` / `tools/call`（用一个假的服务端 HTTP 接口或直接对映射表断言）；调用结束后路径 404；`agent-mcp-bridge` bin 在 stdio 上转发一轮 `initialize` + `tools/list`（`env!("CARGO_BIN_EXE_agent-mcp-bridge")`）。

额外检查：`cd app/src-tauri && cargo check`（`main` 的桥接入口）。

## 不做

- 其余工具：调用 Agent（A14）、搜索 / 读其他会话 / 发消息（A16）、定时调用（A17）。
- 工具调用的权限档位与审批（A12 / A16）。
