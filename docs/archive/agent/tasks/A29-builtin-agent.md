# A29 内置 agent 程序（ACP agent，基于 xiefan/agent）

先读 [../devin-rules.md](../devin-rules.md)、`CONTEXT.md` 的「Agent」小节、`docs/adr/001-decisions.md` §12（「将来的内置 agent 也按 ACP agent 实现」）、[A28](A28-builtin-endpoints.md) 的「接口契约」一节，再读底座库的 README / `CONTEXT.md` / `docs/adr/0002-agent-layer-public-interface.md`：`cargo` 拉下来后在 `~/.cargo/git/checkouts/agent-*/` 里，或看 http://192.168.6.210:3000/xiefan/agent 。本票没有迁移。

同批并行：A28（服务端配置 + 前端）。只通过 A28「接口契约」对接，契约不能改。你不碰 `server/`、`app/src/`。

## 背景

用户要一个 Chat 自带的编码 agent，底座是 `xiefan/agent`（LLM Provider + 运行循环 + 工具 / 钩子，**自身不带文件和 Shell 工具，也不会 ACP**）。宿主现在只会以 ACP client 身份驱动外部程序，所以内置 agent 做成**一个 ACP stdio agent 程序**，宿主像对待其他程序一样启动它；权限档位、审批、上下文续用、注入工具、附件、取消全部复用宿主现有的 ACP 路径（`acp.rs`、`permission.rs`），不在宿主里另开一条运行时。

## 一、依赖

`agent-host/Cargo.toml` 允许新增：

```toml
agent = { git = "http://192.168.6.210:3000/xiefan/agent.git", tag = "v0.1.0" }
schemars = "1"
async-trait = "0.1"
regex = "1"
```

`agent` 是 edition 2024，本机 rustc 1.97 可编译；它带 reqwest 0.13，与宿主的 0.12 并存即可，不要升级宿主的 reqwest。除此之外不加依赖。

## 二、程序形态

- 实现放库里：`agent_host::builtin::run_stdio()`（`src/builtin/` 目录，按需拆 `mod.rs` / `acp_server.rs` / `tools.rs` / `mcp_client.rs` / `store.rs`）。
- 三个入口，照 A13 的 `mcp_bridge` 办：
  1. `agent-host` 的 bin 目标 `chat-builtin-agent`（测试和命令行示例用）；
  2. Tauri 桌面端 `main.rs` 收到 `--builtin-agent` 时直接跑 `run_stdio()` 后退出（不起窗口）；
  3. `HostConfig` 加 `builtin_agent: Option<(String, Vec<String>)>`：怎么启动内置 agent。Tauri 设为「当前可执行文件 + `--builtin-agent`」，`examples/host.rs` 设为 `chat-builtin-agent` 的路径。
- `AgentSpec::from_json` 识别 `runtime.builtin == true`：记 `builtin: Option<Value>`（快照里的 `builtin` 字段，即端点数组）。`conn.rs` 的 `materialize` 对内置 Agent：command / args 取 `HostConfig.builtin_agent`（None → 调用失败，原因「这台电脑不支持内置 agent」），并往 env 里加：
  - `CHAT_BUILTIN_ENDPOINTS`：快照 `builtin.endpoints` 的 JSON 原文；
  - `CHAT_BUILTIN_STATE_DIR`：`<data_dir>/builtin`（会话存档目录）。
  
  探测（`agent_host::probe`）对内置 Agent 不启动子进程，直接返回空的模型 / 模式（添加时前端走 A28 的 models 接口）。

## 三、ACP 服务端（内置 agent 这一侧）

对照宿主 `acp.rs` 实际发出的请求实现，不做宿主用不到的方法：

| 方法 | 行为 |
|---|---|
| `initialize` | `protocolVersion: 1`；`agentCapabilities: {loadSession: true, promptCapabilities: {image: true}, mcpCapabilities: {http: true}}` |
| `session/new` | 新会话 id（uuid）；读 `cwd`、`mcpServers`；返回 `models: {currentModelId, availableModels: [{modelId: "<端点 id>/<模型>", name: "<端点名> · <模型>"}]}`，不返回 `modes`。端点为空 → JSON-RPC 错误「管理员还没有配置模型」 |
| `session/load` | 从 `CHAT_BUILTIN_STATE_DIR/<id>.json` 读 transcript 恢复；按 ACP 要求先把历史用 `session/update` 回放（user_message_chunk / agent_message_chunk），再回响应。文件不存在 → 错误（宿主会回退到 new） |
| `session/set_model` | 切换本会话之后的 Provider；未知 id → 错误 |
| `session/prompt` | 见下；结束回 `{stopReason}`：`end_turn` / `max_turn_requests` / `cancelled` / `refusal`（模型报错用 JSON-RPC 错误返回，带可读原因） |
| `session/cancel` | 取消当前 Run（`run.cancel()`），prompt 以 `cancelled` 结束 |

- **Provider**：按模型 id 找端点，`protocol` 为 `chat` → `OpenAiChat`，`responses` → `OpenAiResponses`；`echo_reasoning(true)` 对 chat 打开（DeepSeek 思考模式 + 工具需要）。模型 id 空或找不到 → 第一个端点第一个模型。
- **prompt 内容块**：text 拼成 user 消息；image 块（base64）底座 v0.1 不支持图片输入 → 在文本里写占位「[图片：<名字或 mime>，当前模型不可见]」；resource_link / resource 块把路径写进文本。宿主把 Agent 指令作为新会话第一个文本块发来，原样当 user 文本即可。
- **系统提示**：固定一段中文说明（你是在用户电脑上工作的编码助手、工作目录是 cwd、改文件前先读、用工具不要臆测文件内容、回答简洁），并附上 cwd 和操作系统。写成常量，不做配置。
- **事件映射**（`AgentEvent` → `session/update`）：`TextDelta` → `agent_message_chunk`；`ReasoningDelta` → `agent_thought_chunk`；`ToolStart` → `tool_call`（`toolCallId`、`title`、`kind`、`locations`、`status: pending`）；`ToolEnd` → `tool_call_update`（`status: completed | failed`，`content` 放截断后的结果文本）。宿主据此出步骤记录。
- **存档**：每次 `RunEnd` 后把 `agent.transcript()` 写到 `<state_dir>/<会话 id>.json`（先写临时文件再 rename）。`turn_limit(50)`。

## 四、工具

全部路径参数：相对路径按 cwd 解析，绝对路径照用。输出超过 30000 字符截断并注明。

| 工具 | kind | 说明 |
|---|---|---|
| `read_file` | read | `path`、可选 `offset` / `limit`（行）；带行号输出；二进制文件报错 |
| `list_dir` | read | `path`、可选 `depth`（默认 1，最大 5）；跳过 `.git`、`node_modules`、`target` |
| `grep` | search | `pattern`（regex）、可选 `path`、`glob`（只支持 `*.ext` 这种后缀过滤）；输出 `文件:行号: 内容`，最多 200 条 |
| `write_file` | edit | `path`、`content`；自动建父目录 |
| `edit_file` | edit | `path`、`old`、`new`、可选 `replace_all`；`old` 不唯一且没 `replace_all` 时报错 |
| `shell` | execute | `command`、可选 `timeout_secs`（默认 120，最大 600）；Windows 用 `cmd /C`，其他 `sh -c`；cwd 为工作目录；合并 stdout/stderr + 退出码；超时杀进程树 |

- **权限**：在 `Hooks::before_tool` 里，**每个**工具调用都先向宿主发 `session/request_permission`（`toolCall: {toolCallId, title, kind, locations}`，`options`: `allow_once` / `allow_always` / `reject_once`）。宿主按档位自动放行或转审批（`permission.rs` 已实现，不要在内置 agent 里重复判断档位）。结果是 reject 或 `cancelled` → `BeforeTool::Block`，理由「用户拒绝了这次操作」。`allow_always` 在本会话内记住（同 kind + 同路径不再问）。
- **locations**：文件类工具填解析后的绝对路径；`shell` 填 cwd。
- `edit` / `write_file` / `shell` 标 `exclusive()`；读类可以并发。
- **注入的 MCP 工具**：`session/new` / `session/load` 的 `mcpServers` 里 `type: "http"` 的每一项：`initialize` → `tools/list`，把每个工具包成底座的 `Tool`（名字 `<服务名>__<工具名>`，参数 schema 原样），调用走 `tools/call`，kind 记 `other`，同样经过权限请求（宿主对 chat 注入工具本来就放行的，按宿主行为走）。MCP 客户端用宿主已有的 reqwest 0.12 写，JSON-RPC over POST，不做 SSE。连不上某个服务 → 跳过它并在 stderr 记一条，不让会话失败。

## 五、测试

`agent-host/tests/builtin.rs`，不连真实模型：

- 用底座的 `ScriptedProvider` 不够（它在子进程外），所以在测试里起一个**本地假 OpenAI Chat Completions 端点**（axum，按脚本返回 SSE），`CHAT_BUILTIN_ENDPOINTS` 指向它，用 `env!("CARGO_BIN_EXE_chat-builtin-agent")` 起子进程，通过宿主现有的 `AcpRuntime` 驱动（和 `tests/acp.rs` 驱动假 agent 的方式一样）。
- 覆盖：
  1. 纯文本回复：宿主拿到的 `RunOutput.text` 等于脚本文本；`session/new` 报出的模型列表正确。
  2. 工具链：脚本先让模型调 `write_file` 再 `read_file`，档位 `workspace`、路径在 cwd 内 → 自动放行，文件真的写出来，步骤记录两条 ok。
  3. 档位 `read_only` 下 `write_file` → 走审批通道；测试里拒绝 → 文件不存在，模型收到拒绝结果后正常结束。
  4. `shell` 在 `read_only` 下同样需要审批；`full` 下直接执行，输出含退出码。
  5. 续上下文：第一次调用后用返回的 `session_id` 再调一次 → 宿主走 `session/load`，假端点收到的第二次请求里带着第一轮的 user / assistant 消息。
  6. 取消：假端点慢速吐字时置 cancel → 结束为已取消，且 5 秒内返回。
  7. 注入 MCP：起一个假 http MCP 服务（或复用 `mcp.rs` 的本地服务）→ 模型调 `chat__read_conversation_history` 能拿到结果。
- 工具单测（`src/builtin/tools.rs` 里 `#[cfg(test)]`）：`edit_file` 唯一性、`read_file` 行号与截断、`grep` 后缀过滤、`list_dir` 跳过目录。

额外检查：`cd app/src-tauri && cargo check`；用 `examples/host.rs` 连你槽位的后端 + 一个真实可用端点（若 `.env.local` 里没有 `OPENAI_*` 变量就跳过并在交付文档写明「未做真实模型验证」）。
