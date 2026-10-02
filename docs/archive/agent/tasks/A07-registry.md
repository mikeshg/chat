# A07 ACP 登记表：检测、一键安装、真 agent 冒烟

对应 Gitea issue xiefan/chat#8（先看 issue 里 A01 审查备注：Windows 上的 `.cmd` 垫片）。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)（Q15、Q16）、`docs/adr/001-decisions.md` §12、[../delivery/A01.md](../delivery/A01.md)、[../delivery/A06.md](../delivery/A06.md)、[../delivery/A13.md](../delivery/A13.md)。本票**没有迁移**。

## 背景

添加 Agent 时要能从 ACP 官方登记表里选运行时；目标宿主上没装的，主人确认后由宿主自动安装。登记表地址与格式（已查证）：

- 索引：`https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`，顶层 `{ version, agents: [...] }`。
- 条目：`id`、`name`、`version`、`description`、`icon`（SVG URL）、`distribution`，其中分发方式三选一或多种：
  - `npx`：`{ package, args? }`
  - `uvx`：`{ package, args? }`
  - `binary`：按平台键 `darwin-aarch64` / `darwin-x86_64` / `linux-aarch64` / `linux-x86_64` / `windows-aarch64` / `windows-x86_64`，每项 `{ archive, sha256?, cmd, args?, env? }`

同批并行：A03、A05、A08、A17。

## 一、Agent 运行时的新形状

- `agents.runtime` 允许两种形状：原来的 `{ command, args }`，以及 `{ "registry": { "id": "…", "version": "…" } }`。创建 / PATCH 时校验（登记表形状只校验字段格式，不联网）。
- 登记表形状由**宿主**在调用时解析成本机的启动命令（安装路径因机器而异，所以不存服务端）；宿主上没装或版本不在时，调用失败并给出明确错误 `registry_not_installed`。

## 二、宿主

- 拉取登记表并缓存到数据目录（有效期 24 小时，拉取失败用旧缓存；都没有就返回空列表和错误说明）。
- 本机可用性（每个条目给出 `ready` / `installable` / `unsupported` 和原因）：
  - `npx`：本机有 `node` 和 `npm` → 已装（见下）即 `ready`，否则 `installable`；没有 node → `unsupported`（原因「需要先安装 Node.js」）。
  - `uvx`：本机有 `uvx` → `ready`（由 uv 自己缓存），否则 `unsupported`（原因「需要先安装 uv」）。
  - `binary`：当前平台有对应键 → 已解压即 `ready`，否则 `installable`；没有对应键 → `unsupported`。
- 安装（主人确认后由服务端转发请求）：
  - `npx`：`npm install --prefix <数据目录>/agents/npm/<id>@<version> <package>@<version>`，启动时用该目录 `node_modules/.bin/` 下对应的命令；
  - `binary`：下载 `archive`，有 `sha256` 时校验，按扩展名解压 zip / tar.gz 到 `<数据目录>/agents/bin/<id>/<version>/`，启动 `cmd`（相对解压目录）；
  - 安装过程上报进度（阶段：下载 / 校验 / 解压 / 完成 / 失败 + 原因）。
- **Windows 启动**（A01 审查备注）：所有 agent 进程（包括自定义命令）启动前按 `PATH` + `PATHEXT` 解析命令；解析到 `.cmd` / `.bat` 时经 `cmd.exe /c` 启动。写单元测试（可以在测试里造一个临时 `.cmd` 文件）。
- 允许新增依赖（`agent-host`）：`zip`、`tar`、`flate2`、`sha2`（`reqwest` 已有）。

## 三、服务端（转发）

- `GET /v1/agent-hosts/{id}/registry` → 服务端给该宿主发 `{"type":"registry_list","request_id":"…"}`，等宿主回 `{"type":"registry_result","request_id","entries":[{id,name,version,description,icon,kinds,status,reason}],"error"?}`，20 秒超时（504）；宿主离线 409 `host_offline`；只能查自己的宿主。
- `POST /v1/agent-hosts/{id}/registry/{entry_id}/install` → 202；服务端发 `{"type":"registry_install","request_id","entry_id","version"}`；宿主上报 `{"type":"registry_install_progress","entry_id","stage","error"?}`，服务端原样转给主人的普通连接（帧名同上，加 `host_id`）。
- 请求 / 响应的配对用内存里的 `request_id → oneshot`，超时清理。

## 四、前端

- 「添加 Agent」表单的运行时一栏改成两个选项卡：「从登记表选择」（默认）和「自定义命令」。登记表选项卡：选定宿主后拉列表（图标、名称、版本、简介、状态）；`installable` 的条目显示「安装」按钮，点击后二次确认并显示进度，完成后可选；`unsupported` 显示原因、不可选。选中 `ready` 的条目时以登记表形状创建 Agent。
- 编辑 Agent（A06 的编辑器）里运行时显示为「登记表：<名称> <版本>」或自定义命令。

## 五、真 agent 冒烟（开关控制，默认不跑）

- `agent-host` 里一个测试，环境变量 `CHAT_REAL_AGENT_SMOKE=1` 时才执行：通过登记表解析并启动 Claude Code 的 ACP 适配器和 Codex 的 ACP 适配器（id 以登记表为准），各发一次「只回复 pong」和一次会触发权限请求的指令（例如「在当前目录创建文件 smoke.txt」），断言调用成功返回、正文非空、权限请求被宿主按当前规则处理（本票仍是拒绝，A12 接审批）。没设环境变量时直接跳过。
- 交付文档写清运行前提（本机已登录 Claude Code / Codex、需要 Node）和运行命令。**你不需要真的跑它**（没有登录态），写清楚即可。

## 测试

- `server/tests/it.rs` 末尾分节 `// ===== Agent A07 =====`：用一个**假的登记表**（测试里起一个本地 HTTP 服务提供 registry.json 和一个 zip 包；宿主的登记表地址可配置，测试里指向它）：列表返回三种状态；安装 binary 条目后状态变 `ready`，用它创建 Agent 并调用成功（zip 里放一个能跑假 agent 的脚本 / 可执行入口）；sha256 不符安装失败并上报原因；宿主离线 409；别人的宿主 404；登记表形状的 Agent 在未安装的宿主上调用 → `failed/registry_not_installed`。
- `agent-host` 测试：`PATHEXT` 解析与 `.cmd` 启动；缓存过期与拉取失败回退。
- Vitest：登记表条目状态 → 按钮 / 文案的纯函数。浏览器实测（无头 Playwright）：添加 Agent 时切到登记表选项卡、看到条目与状态、安装后创建成功。截图放 `server/target/wt/`。

## 不做

- 自动更新已装 agent 的版本；preview 渠道；卸载。
