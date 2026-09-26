# V2 第 3 阶段验收记录

**日期：2026-09-26　分支：`v2`　执行：主会话（合并 + 验收）**

## 1. 合并内容

| 任务 | 分支 | 执行者 | 内容 | 交付文档 |
|---|---|---|---|---|
| P3-1 | `v2-p3-1` | Devin swe-2-max | 全局搜索 + 会话「文件与图片」列表（迁移 0016） | [delivery/P3-1.md](delivery/P3-1.md) |
| P3-2 | `v2-p3-2` | Devin grok（前半）→ Grok Build CLI `grok-4.7-build-fast` xhigh（接手完成） | 群公告 + 置顶消息 + 会议链接（迁移 0017） | [delivery/P3-2.md](delivery/P3-2.md) |
| P3-3 | `v2-p3-3` | Devin swe-2-max | ntfy 离线推送 + WS 心跳超时（迁移 0018） | [delivery/P3-3.md](delivery/P3-3.md) |

P3-2 起初误用了 Devin 里的 grok 模型，做到一半把 Devin 当日额度用完；用户澄清应使用 Grok Build 命令行后，由 `scripts/v2/run-grok.sh` 接手，审阅已有改动后补完剩余部分，一次跑完。合并顺序：P3-1 → P3-3 → P3-2。

## 2. 合并时的处理

| 位置 | 处理 |
|---|---|
| `server/src/settings.rs` | P3-2（会议地址）与 P3-3（推送配置）改同一批位置，7 个冲突块逐块处理：两处 `if` 断在中间，补上缺的右括号；`SettingsPatch` 那块 P3-2 字段在前 |
| `AdminSettings.tsx` | 两个保存函数被 git 交错切开：推送相关函数保留 HEAD 版本，`saveMeeting` 从 P3-2 分支原样取出插入；两个设置区块依次排列 |
| `ChatView.tsx` 头部按钮 | 用 P3-1 的版本（私聊和文件传输助手也能打开「文件与图片」），带上 P3-2 的头部按钮样式类 |
| `server/tests/migrations.rs` | 迁移列表按 0016、0017、0018 排序 |
| `styles.css`、`it.rs` | 末尾追加冲突，用 `append-section.mjs` 合并 |

## 3. 自动化验证（全新隔离库 45610、后端 18110）

| 检查 | 结果 |
|---|---|
| `cargo fmt --check` | 通过 |
| `cargo test --tests` | it **104 passed** / migrations **3 passed** / 0 failed |
| `npx vitest run` | **154 passed** / 0 failed |
| `npm run build`（tsc + vite） | 通过 |
| `app/src-tauri` `cargo check` | 通过 |
| E2E phase1（先注册专用管理员） | **58 passed** / 0 failed |
| 真实重启后端后 E2E phase2 | **9 passed** / 0 failed |

## 4. 浏览器实测（内置浏览器，两个 origin 各登录一个账号）

| 功能 | 步骤 | 结果 |
|---|---|---|
| P3-1 全局搜索 | 私聊、群各发一条含「季度报告」的消息，再发一条后删除；Bob 在会话列表搜索框搜「季度报告」 | 搜到 2 条（群、私聊各 1），关键词 `<mark>` 高亮，被删的那条不出现 |
| P3-1 墓碑修复 | 搜「消息已删除」 | 「没有匹配的聊天记录」 |
| P3-1 跳转 | 搜「截止时间」并点结果 | 打开群聊，那条消息闪烁高亮 |
| P3-2 公告 | 群主设两行公告；私聊设公告；Bob 改公告 | 200；私聊 400；成员 403。Bob 的群聊头部出现「📢 本周五前提交季度报告　新公告」，点开后「新公告」消失、已读记入 `chat_ann_seen:<群id>` |
| P3-2 置顶 | Bob 置顶一条消息、重复置顶、置顶已删除的消息 | 204 / 204（幂等）/ 400；群主那边实时出现「📌 1 条置顶 \| 群里讨论季度报告的截止时间」 |
| P3-2 会议 | Bob 点「📹 发起会议」 | 发出「📹 发起了视频会议，点击加入：https://meet.jit.si/chat-c8xm6kg5kc0r」，链接可点击、`target=_blank` |
| P3-3 推送配置 | 管理员设 ntfy 地址（本地假服务）+ 令牌 + 对外地址；再设 `file:///etc/passwd` | 200；读取设置时令牌不回显，只返回 `push_ntfy_token_set: true`；非法地址 400 |
| P3-3 离线推送 | Bob 开启推送（得到随机主题订阅地址），他的标签页失焦；管理员发私信 | 假 ntfy 收到 1 条 JSON 发布：主题为 Bob 的随机主题、标题 `accadmin`、正文「你收到一条新消息」（预览默认关，未带出内容）、`Authorization: Bearer <令牌>`、`click` 为对外地址 |
| `server-info` | 查看 | 同时包含 `meeting_base_url` 与 `push_available`（两个任务的设置合并完整） |

心跳超时（无回应连接在超时后被断开并广播离线）由 P3-3 的集成测试覆盖，浏览器里不便构造。

## 5. 过程中的环境问题（已写入 handover）

- 会话中断期间机器重启，WSL 空闲会自动关机，测试库容器随之消失。已改为常驻一个 WSL 保活进程，并新增 `scripts/v2/wt-db-up.mjs` 按原口令重建。
- 测试库原用 55601 起的端口，在 Windows 临时端口段内，被厂商托盘程序占用；已改到 45601 起。
- Devin 在非交互模式下起后端（`cargo run`）会被拒绝；新增 `scripts/v2/wt-server.mjs`，由 node 启停。
- `run-devin.sh` 遇到「当日额度用完」时改为直接停止，不再空转重试。

## 6. 未覆盖项

- 真实手机上的 ntfy App 接收：只验证了服务端发出的请求格式正确（本地假 ntfy）。
- 心跳超时只有集成测试证据。
- Tauri 桌面端这一阶段没有重测（第 3 阶段的改动都在网页层和服务端）。
