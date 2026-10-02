# A06 完整 Agent 配置与删除

对应 Gitea issue xiefan/chat#7。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q26 配置清单）、`CONTEXT.md`「Agent」小节、[../delivery/A01.md](../delivery/A01.md)。迁移号：**`0023_agent_config.sql`**。

## 背景

A01 只能在添加时填名字、宿主、命令、工作目录，之后改不了也删不掉。本票补齐其余配置项、编辑和删除。**`PATCH /v1/agents/{id}` 已经在 main 上有骨架**（只支持 `output_mode`、`queue_timeout_secs` 两个覆盖项，字段缺省 = 不改、显式 null = 清除），你往 `PatchAgentReq` 里加字段，并做编辑界面（**包括这两个覆盖项的控件**，选项沿用 `AgentSettings.tsx` 导出的 `OUTPUT_MODE_OPTIONS` / `QUEUE_TIMEOUT_OPTIONS`，另加「跟随默认」）。

同批并行：A02、A03、A04、A13，都会碰 `agent-host/src/{runtime,acp,conn}.rs`、`server/src/agents.rs`。只加你需要的。

## 一、数据（迁移 0023）

`agents` 加：`extra_dirs text[] NOT NULL DEFAULT '{}'`、`model text NOT NULL DEFAULT ''`、`mode text NOT NULL DEFAULT ''`（空串 = 用 agent 默认）、`instructions text NOT NULL DEFAULT ''`（≤ 8000 字符）、`env jsonb NOT NULL DEFAULT '{}'`（字符串到字符串，≤ 50 项，键 ≤ 100、值 ≤ 4000 字符）、`avatar_version integer NOT NULL DEFAULT 0`、`reported_options jsonb NOT NULL DEFAULT '{}'`（宿主上报的可选模型/模式）、`deleted_at timestamptz NULL`。名字唯一索引改成只约束未删除的 Agent（部分唯一索引）。

## 二、服务端

- `PATCH /v1/agents/{id}` 增加可改字段：`name`（规则同创建、未删除范围内唯一，409 `name_taken`）、`runtime`、`cwd`、`extra_dirs`（≤ 10 个，每个 ≤ 500 字符）、`model`、`mode`、`instructions`、`env`。请求里带 `host_id` → 400 `host_immutable`。已删除的 Agent 不能改（404）。
- `AgentView` 增加上述字段和 `reported_options`、`avatar_version`、`deleted`。**环境变量只在主人自己的 `GET /agents` / `PATCH` 响应里返回**（本来就只作用于本人资源，保持不变）。
- 头像：`PUT /v1/agents/{id}/avatar`（PNG / JPEG / WebP，复用用户头像的校验与存储方式，成功 `avatar_version + 1`）、`GET /v1/agents/{id}/avatar?v=`（与用户头像同样的缓存头）。`MessageView.agent` 与 `ConversationView.agent` 带上 `avatar_version`，客户端据此显示头像。
- `DELETE /v1/agents/{id}`：软删除（`deleted_at = now()`）；之后在它的 AgentConversation 发消息 → 400 `agent_deleted`（服务端强制，不触发调用）；`GET /agents` 不再列出；AgentConversation 仍在会话列表里，`ConversationView.agent` 带 `deleted: true`。删除时该 Agent 进行中的调用改成 `failed`、`error='agent_deleted'`，并通知宿主取消（发 `invocation_cancel`，宿主已有处理）。
- `invocation_start` 的 `agent` 快照增加 `extra_dirs`、`model`、`mode`、`instructions`、`env`。配置改动在下一次调用生效；离线宿主上线后自然拿到最新值（快照随调用下发）。
- 可选项探测：`POST /v1/agents/{id}/probe` → 宿主在线时给宿主发 `{"type":"agent_probe","agent":{…快照…}}`，返回 202；宿主离线 409 `host_offline`。宿主上报 `{"type":"agent_options","agent_id":"…","modes":[{"id","name"}],"models":[{"id","name"}]}` 后服务端存进 `reported_options`，并给主人推 `Invalidate { what: "agents" }`（前端收到后重拉 Agent 列表）。每次正常调用后宿主也会上报一次（见下）。

## 三、宿主

- `RunRequest`（`runtime.rs`）加 `env`、`instructions`、`model`、`mode`、`extra_dirs`；`AgentSpec::from_json` 解析这些字段（缺省为空）。
- ACP：
  - `env` 作为子进程的环境变量（在继承的环境上追加 / 覆盖）；
  - `extra_dirs`：agent 在 `initialize` 响应里声明 `sessionCapabilities.additionalDirectories` 时，放进 `session/new` 的 `additionalDirectories`；否则忽略；
  - `session/new` 响应里的 `modes.availableModes` / `models.availableModels`（若有）通过 `agent_options` 帧上报；`mode` 非空且在可选范围内 → `session/set_mode`；`model` 非空且 agent 给了模型列表 → `session/set_model`；不在范围内就忽略并照常运行；
  - `instructions` 非空：**只在新建会话时**作为 prompt 的第一个文本块放在用户指令前（A02 同时在做会话恢复，恢复的会话不要重复加；你这边把它写在「新建会话」那条分支里）。
- `agent_probe`：只做 `initialize` + `session/new`，上报 `agent_options` 后结束子进程。
- 假 agent：`session/new` 响应带两个 mode、两个 model；支持 `session/set_mode` / `session/set_model`；新指令 `whoami` → 回出当前 mode、model、环境变量 `FAKE_ECHO` 的值和收到的第一个文本块（验证 instructions）。

## 四、前端

- Agent 管理面板里每个 Agent 有「编辑」：名字、头像上传、命令与参数、工作目录、额外目录（每行一个）、模型 / 模式下拉（选项来自 `reported_options`，为空时显示「运行一次或点“读取可选项”后可选」并提供读取按钮调 probe）、附加指令（多行）、环境变量（键值对增删行）、输出方式覆盖、排队超时覆盖、宿主（只读显示）。保存调 `PATCH`，显示 400 / 409 错误。
- 「删除 Agent」按钮，二次确认后调 `DELETE`；删除后的 AgentConversation 显示「该 Agent 已删除」，输入框禁用。
- Agent 头像：会话列表、聊天页头部、Agent 消息上有头像时显示图片，没有时沿用 🤖。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== Agent A06 =====`：

1. PATCH 各字段生效并在 `GET /agents` 可见；非法值 400（名字、env 超限、instructions 超长、extra_dirs 超限）；带 `host_id` 400；重名 409；别人 404。
2. 改完配置后发 `whoami` → 回复里是新的 mode / model / 环境变量 / instructions（第一个文本块）。
3. probe：在线 202 → 很快 `GET /agents` 的 `reported_options` 有两个 mode；宿主离线 409。
4. 删除：发消息 400 `agent_deleted` 且不产生调用；列表不再出现；会话列表里 `agent.deleted=true`；删除后可以用同名再建一个新 Agent；删除时运行中的调用变 `failed/agent_deleted`。
5. 头像：上传后 `avatar_version` 变化，`GET` 能取回；别人不能上传。

Vitest：编辑表单的输入整形（参数、额外目录、环境变量行 → 请求体）写成纯函数测试。浏览器实测（无头 Playwright）：编辑一个 Agent 的附加指令和环境变量后发 `whoami` 看到新值；删除后输入框禁用。截图放 `server/target/wt/`。

## 不做

- 换宿主（产品决定不支持）；登记表选择运行时（A07）；权限档位（A12）。
