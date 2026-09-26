# A18 E2E 覆盖 + 文档收尾

对应 Gitea issue xiefan/chat#19。先读 [../devin-rules.md](../devin-rules.md)、[../spec.md](../spec.md)、`CONTEXT.md` 的「Agent」小节，以及 `docs/design/agent/delivery/` 下 A01–A17 的全部交付文档（A10 由主人在 Mac 上做，可能还没有）。本票**没有迁移**，不改业务代码（发现 bug 记进交付文档「发现的其他问题」，不顺手改）。

## 背景

Agent 功能各票已全部合并。本票补上面向部署的冒烟覆盖和用户文档，并核对领域文档与实现一致。最终验收记录由主会话在亲自验收后写，不在本票范围。

## 一、E2E 冒烟（`scripts/e2e.mjs`）

在现有 phase1 末尾追加一节 Agent（沿用脚本的 `ok()` / `api()` / `Ws` 工具与风格），**只覆盖不需要宿主进程的部分**：

1. 管理员确认 Agent 功能开着（`GET /admin/settings` 的 `agent_enabled`），必要时打开。
2. 普通用户 A：`GET /agent-hosts` 为空；用一个伪造的宿主 id 建 Agent → 404。
3. 用 WS `role=host` 握手登记一台宿主（发 `host_hello`，等 `host_ready`），建 Agent `e2ebot`（自定义命令即可，不会真的运行）→ 200；AgentConversation 出现在会话列表；`PATCH` 改名与覆盖项生效；重名 409。
4. A 和 B 在同一个群：B 发 `@e2ebot hi` → A 的调用历史里没有新调用（非主人不触发）；A 发 `@e2ebot hi` 且宿主连接在线 → 出现一条调用（宿主不回也没关系，断开宿主后它变为 `failed/host_disconnected`）。
5. 个人设置：`PUT /users/me/agent-settings` 改输出方式与深度上限，读回一致。
6. AgentAccess：管理员把 B 设为禁止 → B 的 `GET /agents` 403、`/users/me` 的 `agent_access=false`；恢复为跟随默认。
7. 删除 `e2ebot` → 专属会话只读（发消息被拒）。

phase2（重启后持久化）追加：Agent、宿主记录、个人设置在重启后仍在。

## 二、README

- 「功能」列表加 Agent 一段（简洁几行）：本地 agent（ACP：Claude Code、Codex 等，登记表一键安装）、在专属会话或任意会话里 `@` 调用、三种输出方式、权限档位与审批、离线排队、Agent 互调、定时调用、管理员开关。
- 新增「Agent」一节：桌面端即宿主（登录后自动上线、托盘常驻、开机自启）；添加 Agent 的两种方式；权限档位含义；真 agent 冒烟测试的运行方法（见 A07 交付文档）；已知限制（环境变量 / 密钥明文存服务器；macOS / Linux 包需在对应系统上编译）。
- 「架构」里补 `agent-host/` 一行。

## 三、CONTEXT.md 核对

逐条对照「Agent」小节的词条与边界规则和实际实现，不一致的地方以实现为准修正文档（只改说法，不写实现细节）；在交付文档里列出改了哪些句子和原因。

## 检查

devin-rules §4 的全部检查照常执行；另外起槽位后端，按脚本说明跑一次 `node scripts/e2e.mjs`（先注册管理员，见脚本前提）和一次 phase2（重启后端后），在交付文档里贴通过数。

## 不做

- 验收记录（主会话写）；桌面实机验证；A10 的打包。
