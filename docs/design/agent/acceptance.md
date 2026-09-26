# Agent 接入 — 阶段验收记录

**2026-09-26，Windows 11（机器名 14X），主会话亲自验收。** 验收库 `chat-v2-acc`（45610），后端 `127.0.0.1:18110`，代码为当天 `main`。

## 1. 验收库全量检查

步骤见 V2 handover 第 8 节（全新库，先注册管理员再跑 E2E）。

| 检查 | 结果 |
|---|---|
| server `cargo fmt --check` + `cargo test --tests` | 通过：it 211 + migrations 3 + schedule_next 8 |
| agent-host `cargo test` | 通过：55（含验收中新加的两个单测） |
| 前端 `tsc -b` + vitest + build | 通过：vitest 232 |
| Tauri `cargo check` | 通过 |
| E2E phase1 | 139 passed, 0 failed |
| E2E phase2（重启后端后） | 18 passed, 0 failed |

## 2. Windows 桌面实机

用 cua-driver 操作 `npx tauri build --debug --no-bundle` 编出的 `chat-app.exe`，数据用 API 和 Windows 通知库交叉核对。

| 项目 | 结果 | 怎么核对的 |
|---|---|---|
| 登录后宿主自动上线 | 通过 | `GET /agent-hosts`：`online=true` |
| 宿主默认名 | 通过 | `label=14X`（= `COMPUTERNAME`），`platform=windows` |
| 重启应用后自动上线、宿主标识不变 | 通过 | 不重新登录，同一个宿主 id 重新 `online=true` |
| 关窗隐藏到托盘，宿主继续在线 | 通过 | 窗口从屏幕消失、进程在、宿主仍在线 |
| 首次隐藏通知 | 通过 | 通知库里有「Chat 已最小化到托盘，Agent 会继续运行」。未打包的 debug 版挂在 PowerShell 名下，NSIS 安装包用自己的应用标识（没验） |
| 托盘图标、菜单 | 通过 | 托盘里有 Chat 图标，右键菜单「显示窗口」「退出」 |
| 左键单击托盘图标 / 菜单「显示窗口」唤回窗口 | 通过 | 两种方式都让窗口重新出现 |
| 托盘「退出」先停宿主 | 通过 | 带着一个 `sleep:120000` 的调用退出：chat-app 退出，假 agent 子进程被结束，宿主立刻离线，调用记为 `failed / host_disconnected` |
| 开机自启开关 | 通过 | 默认关；勾上后 `HKCU\...\Run` 出现 `Chat = ...chat-app.exe`，取消后删除 |
| Agent 专属会话 | 通过 | 列表里出现，头像带在线点，头部显示「14X (windows) · 在线」 |
| 审批卡片 | 通过 | 假 agent `perm:edit:C:/outside/x.txt`：卡片显示请求、类型、路径和三个选项，原处显示「等待主人批准 · 去审批 · 取消」；点「本次允许」后卡片变为「已选择：本次允许」，agent 回 `allowed:allow` |
| 嵌套卡片 | 通过 | `call:b:steps:2`：a 的会话里出现「↳ b · 完成」，点开显示 b 的正文；b 的专属会话里有「被 @a 调用：steps:2」和可展开的 2 个步骤 |

## 3. 真 agent 冒烟

| 适配器 | 结果 |
|---|---|
| Codex（`codex-acp` 1.13.1，npx 安装） | 通过：pong 有正文；往工作目录外写文件触发权限请求，拒绝后文件没写出来 |
| Claude Code（`claude-acp` 0.81.2，npx 安装） | 没跑完：安装成功，调用时报 `OAuth session expired`。本机 Claude Code 登录已过期，重新登录后再跑 `CHAT_REAL_AGENT_SMOKE=claude` |

## 4. 验收中发现并修掉的问题

1. **登记表 npx 包名重复拼版本**（`f3d5b05`）：`claude-acp` 的 package 字段自带版本，拼出 `...@0.81.2@0.81.2`，npm 拒绝安装。改为保留自带的版本。
2. **Codex 的审批绕过了权限档位**（`54fc462`）：codex-acp 默认的 `agent` 模式由 Codex 自己的 auto-review 批准请求，档位根本收不到。主人没选模式时，改为切到「凡事先问」的模式（`_meta.kind=standard`）。冒烟随之改成往工作目录外写文件。
3. **Agent 会话头部头像被拉成整条横幅**（`9a13d2a`）：标题块的 `flex: 1` 写成了 `:first-child`，专属会话里第一个子元素是头像。改为按类名。

## 5. 已知限制和遗留

- Codex 的沙箱在任何模式下都允许写它自己的工作目录，所以「只读」档位管不住 Codex 对工作目录的修改（README 已写明）。目录外的写入和联网照样走审批。
- Claude Code 真 agent 冒烟待本机重新登录后补跑。
- A10（macOS / Linux 打包）由主人在 Mac 上做；#19 等 #12 完成后一起关。
- 工具备注：cua-driver 的桌面点击会先激活窗口，把 Win32 弹出菜单关掉，托盘菜单项点不中。验收时改为右键打开菜单后，按窗口范围给 chat-app 发方向键和回车。
