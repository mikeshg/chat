# A28 内置 agent：管理员配置模型端点 + 「内置」运行时

先读 [../devin-rules.md](../devin-rules.md)、`CONTEXT.md` 的「Agent」小节、`docs/adr/001-decisions.md` §12。本票迁移编号：**无**（配置放 `server_settings`，runtime 是 `agents.runtime` 里的新形状）。

同批并行：A29（宿主里的内置 agent 程序）。两票只通过本单「接口契约」一节对接，**契约不能改**；A29 不碰 `server/`、`app/src/`，你不碰 `agent-host/`、`app/src-tauri/`。

## 背景

Chat 要有一个自带的 AgentRuntime（「内置 agent」），不用用户另装 Codex / Claude Code。它的底座是 Rust 库 `xiefan/agent`，直连 OpenAI 兼容端点。用户已定：

1. **模型端点由管理员统一配置**：多个端点，每个端点可配多个模型。
2. **API key 随调用下发**到运行 Agent 的电脑（用户已知悉：有 Agent 使用权的人能在自己电脑上拿到 key）。
3. 在添加 Agent 时，「内置」作为一个程序选项出现；模型从管理员配置的列表里选。

## 接口契约（与 A29 共用）

### Agent 的 runtime 形状

```json
{"builtin": true}
```

`normalize_runtime` 接受第三种形状（`AgentRuntimeReq::Builtin { builtin: bool }`，`builtin` 必须为 `true`，否则 `bad_runtime`）。cwd 规则同其他形状。

### 端点配置（server_settings 键 `builtin_endpoints`，值为 JSON 数组）

```json
[
  {
    "id": "8f0c…",              // 服务端生成的 uuid，改名 / 改地址不变
    "name": "DeepSeek",         // 1–40 字符
    "protocol": "chat",         // chat = Chat Completions；responses = OpenAI Responses
    "base_url": "https://api.deepseek.com/v1",   // http/https，去掉末尾 /
    "api_key": "sk-…",          // 可空（本地 vLLM 等）；≤500 字符
    "models": ["deepseek-chat", "deepseek-reasoner"]   // 1–50 个，每个 1–200 字符，去重
  }
]
```

最多 20 个端点。

### 下发给宿主（invocation_start / agent_probe 的 agent 快照）

仅当 `runtime.builtin == true` 时，`agent_snapshot` 多一个字段：

```json
"builtin": {"endpoints": [ …上面的数组原样，含 api_key… ]}
```

Agent 的 `model` 字段取值格式为 `<端点 id>/<模型名>`（按**第一个** `/` 切分，模型名里可以再有 `/`）。空串 = 用第一个端点的第一个模型。

## 一、服务端

- `settings.rs`：`ServerSettings` 加 `builtin_endpoints`。**key 只写不读**：`GET /v1/admin/settings` 返回每个端点时去掉 `api_key`、加 `api_key_set: bool`；`PUT` 时某端点没带 `api_key` 字段 = 保留原 key（按 id 匹配），带空串 = 清空。新端点（无 id）由服务端生成 id。校验失败 400（`bad_builtin_endpoints`），规则见上。沿用现有 PUT 的写法，别另起接口。
- `agents.rs`：`normalize_runtime` 接受 `{"builtin": true}`；`agent_snapshot` 按上面契约加 `builtin`（每次调用从 `server_settings` 现读，改配置下一次调用生效）。
- 新接口 `GET /v1/agent-builtin/models`（有 Agent 使用权的用户）：`[{"id": "<端点 id>/<模型>", "name": "<端点名> · <模型>"}]`，**不含** base_url 和 key。无使用权 403（沿用 `agent_access` 的判断）。
- 管理员删掉某端点后，已有 Agent 的 model 指向不存在的端点：不报错，按「空串」处理由宿主选第一个（服务端不改 agents 表）。

## 二、前端

- 后台设置：新组件 `AdminBuiltinEndpoints.tsx`，在 `AdminSettings.tsx`（或 `AdminPanel.tsx`，看现有设置挂在哪）加一行挂载。列表 + 行内编辑：名称、协议（下拉 chat / responses）、地址、key（密码框，已设置时占位「已设置，留空不改」，旁边「清除」）、模型（逗号或换行分隔的文本框）。增 / 删 / 保存。
- 添加 Agent（`AgentAdd.tsx`）：程序列表顶部加一张固定卡片「内置」（说明：「Chat 自带，使用管理员配置的模型」），搜索时也始终显示（同自定义命令卡片）。选中后**不走 `probeLocal`**，模型选项改为 `GET /v1/agent-builtin/models`；列表为空时显示「管理员还没有配置模型」并禁用创建。创建时 runtime = `{"builtin": true}`。模式选项：无。
- Agent 详情里「程序」显示为「内置」；工作区对话里的模型下拉，内置 Agent 同样用这个接口的列表。找到现在读模型选项的地方（`reported_options` / `probeLocal`）按 runtime 分支，只加分支不重构。
- `types.ts` 的 `AgentRuntime` 加 `{ builtin: true }`。

## 三、测试

`server/tests/it.rs` 末尾 `// ===== Agent A28 =====`，改全局设置的用 `spawn_named_db`：

1. 管理员 PUT 两个端点 → GET 不含 `api_key`、`api_key_set` 正确；再 PUT 不带 key → 原 key 保留（从调用快照断言）；带空串 → 清空。
2. 非法配置 400：协议非法、地址非 http(s)、模型为空、超过 20 个端点。普通用户 PUT 403。
3. 创建 `{"builtin": true}` 的 Agent 成功；`{"builtin": false}` 400。
4. 用假宿主收 `invocation_start`：内置 Agent 的快照带 `builtin.endpoints`（含 key）；命令形状的 Agent 快照**没有** `builtin` 字段。
5. `GET /v1/agent-builtin/models`：返回 id / name、不含 key；无使用权用户 403。

前端：`AgentAdd` 选「内置」时调用 models 接口、不调用 `probeLocal`（vitest，mock 掉两者）。

浏览器实测（无头 Playwright）：管理员配一个端点两个模型 → 桌面端之外无法添加 Agent，所以只验证后台设置页的增删改和 key 占位；截图附在交付文档。
