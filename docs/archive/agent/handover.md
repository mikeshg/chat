# Agent 接入 — 接手文档

**更新于：2026-09-28 11:01（Asia/Shanghai）**

新会话先读本文件，再读 [spec.md](spec.md)（需求，来自 Gitea xiefan/chat#1）、`CONTEXT.md` 的「Agent」小节、`docs/adr/001-decisions.md` §12。

## 1. 现状

- 18 张票里 **A01–A09、A11–A18 已实现并合并到 `main`**（各票交付文档在 [delivery/](delivery/)，任务单在 [tasks/](tasks/)）。`main` 没有推送。
- 最终检查（`84cc702`）：server it 211 + migrations 3 + schedule_next 8、agent-host 53、vitest 232、前端 build、Tauri `cargo check` 全部通过；E2E phase1 139/139、phase2 18/18（A18 在槽位后端上跑）。
- Gitea 上仍打开：#1（spec，父 issue，不关）、#12 **A10 macOS / Linux 打包**（ready-for-human，需要在 Mac 上编译）、#19 A18（已完成，但依赖 #12，Gitea 不让关，等 A10 完成后一起关）。
- **2026-09-28 审查修复 A19–A22 已合并到 `main`**（`2d61440`）。来源是一份外部代码审查（基于 `706dbea`）的 7 条问题，主会话逐条核对属实后拆票；任务单开头写了每张票对应审查第几条。没有开 Gitea issue。
  - A19 宿主路径安全：附件落到 `in/<附件 id>/<清洗后的名字>`；workspace 档按真实路径判断（junction / 符号链接指向目录外不再自动放行）。
  - A20 只读档发消息审批：卡片展示完整正文（`request.detail`）和全部待上传文件的绝对路径；超出展示上限直接拒绝。
  - A21 退群 / 踢人 / 解散后重算进行中调用的接收者；开新话题改为代次（迁移 `0031`：`agent_contexts.epoch`、`agent_invocations.context_epoch`），重置前派发的调用完成时不再写回旧会话。
  - A22 同一调用可同时有多张 pending 审批，决定完最后一张才回到 `running`。
  - 合并后检查：server it 225 + migrations 3 + schedule_next 8、agent-host 64、vitest 235、前端 build、Tauri `cargo check` 全部通过。没有跑 E2E。

## 2. 还没做的

1. **A10**：macOS 已由用户在 Mac 上编包并确认可用（`pnpm tauri build --bundles app`，README 已写；DMG 那步在外接盘上失败，不影响 `.app`）。Linux 包还没在 Linux 上编过。
2. A19–A22 交付文档里记下的范围外问题，未排期：
   - 服务端不清洗上传文件名（宿主侧已防住，见 A19）；
   - MCP `send_message` 的 `file_paths` 相对路径按宿主进程当前目录解析，不是 Agent 工作目录（A20）；
   - 调用进行中拉人进群，新人收不到之后的流式帧（A21 只处理了移出）。

阶段验收（验收库全量检查、Windows 桌面实机、Claude Code 与 Codex 真 agent 冒烟）已完成，记录在 [acceptance.md](acceptance.md)，验收中修掉的三个问题也在里面。

## 3. 用户在需求讨论中的决定

见 [spec.md](spec.md) 与 `CONTEXT.md`。讨论规则：选项优先级「实现功能 → 性能 / 资源 → 开发难度」，全方位占优的不问，一次一个问题、用弹窗。合并后补的一处：私聊 / 群里也能「开新话题」（spec Q6，A02 当时只做了专属会话，`a34209a`）。

## 4. 编排工具（沿用 `scripts/devin/`）

- 建 worktree：`PROG=agent BASE=main SLOT_BASE=<已占槽位数> node scripts/devin/wt-setup.mjs A19,...`（`SLOT_BASE` 避开仍在跑的槽位，否则会 `docker rm -f` 掉别人的测试库）。
- 执行者：同时最多 2 个 Devin（`PROG=agent bash scripts/devin/run-devin.sh <票> new`），其余 Grok Build（`PROG=agent bash scripts/devin/run-grok.sh <票>`）。本轮 Grok 明显更快，质量也稳定；Devin 读代码常超过 10 分钟无输出，被看门狗误杀后自动续跑。
- 合并工具：`node scripts/devin/union.mjs`（两边都保留）、`SECTION_PREFIX=Agent node scripts/devin/append-section.mjs <分支> <票号> server/tests/it.rs`（注意它以 HEAD 为底重写整个文件，之后要重新应用对该文件的其他修改）。
- 任务单：`wt-setup.mjs` 从主仓库工作区找任务单文件名，但 worktree 从 `BASE` 的提交检出。任务单没提交时要手动复制进各 worktree（A19–A22 就是这样，合并前再删掉副本、在 `main` 上单独提交任务单）。
- 返工：Grok 交付后要改，用同一会话续跑：在 worktree 里注入 `.env.local` 后 `grok -r <session id> --prompt-file <返工说明绝对路径> -m grok-4.7-build-fast --reasoning-effort xhigh --output-format streaming-json` 加上 run-grok.sh 里那组 `--deny`。session id 在 `../chat-wt/_runtime/devin/<票>.resume.log`。

## 5. 本轮踩过的坑

- **WSL 保活**：派发前后台常驻 `wsl -d Debian -- sh -c "exec sleep 86400"`。
- **验收库是 tmpfs**：一天跑很多遍全量测试（每遍新建上百个独立库）会写满，表现为 `CREATE DATABASE` 失败 / `database does not exist`。`acc-env.mjs up` 重建即可；重建后第一次全量可能撞上 Postgres 初始化后的重启，再跑一次。
- **Grok 在前台起 `cargo run`** 会让它的命令永远不返回（A15 卡了 16 分钟）。找到它的 pwsh 子进程结束掉即可；run-grok.sh 的看门狗 30 分钟无输出也会处理。
- **并行票改同一处**：`agent-host/src/{acp,conn,runtime}.rs`、`server/src/agents.rs` 每张票都动，晚合并的分支冲突很重。经验：
  - 迁移里重建 `message_views` 必须以最新一版视图为底（A03 差点丢掉 A06 的列）；
  - 各模块自带的调用行 RETURNING 列表会漏新列 → 已统一用 `agents::INV_RETURNING`，`InvocationView` 的后加列用 `#[sqlx(default)]`（A12 审批曾因此被静默丢弃）；
  - 冲突块拼接时注意两边是否都缺同一个收尾的 `}`。
- **桌面验收**：WebView 内容不在 UIA 树里，只能按截图像素点；窗口被别的窗口完全挡住时 WebView 不绘制也不处理输入。托盘菜单是 Win32 弹出菜单，cua-driver 的桌面点击会先把它关掉，要右键打开后按窗口范围给 chat-app 发方向键和回车。托盘图标位置会随任务栏图标增减移动，右键前先截图确认。
- Git Bash 里给 node 脚本传 `/v1/...` 这类参数会被改写成 Windows 路径，前面加 `MSYS_NO_PATHCONV=1`。
- 桌面端 `app/src-tauri/gen/schemas/*.json` 构建后只有换行变化，提交前 `git checkout --` 还原（A09 那次是真改动：自启权限）。
- **两个分支都在 `it.rs` 末尾追加测试**时，git 会把两段结尾相同的几行（`))` / `.await` / `.unwrap();` / `}`）对齐，冲突块被切碎，按块拼接会拼错。先确认分支对该文件只有一个 `@@`（`git diff main...<分支> -- server/tests/it.rs`），再用 `append-section.mjs`，核对追加行数等于分支新增行数。
- **执行者倾向内存状态 + 进程内锁**：A21 第一版把上下文代次放进内存 DashMap、跨数据库提交持有 Mutex，重启即失效且表只增不删。审查时留意这类方案；能用数据库单条语句的原子性（条件 upsert）解决的，退回重做。

## 6. 资源状态

- worktree：只剩主仓库；各票分支 `agent-a*`（含 A19–A22）保留作记录。
- 测试容器：槽位容器和验收库 `chat-v2-acc` 都已删除；要跑全量检查时 `node scripts/devin/acc-env.mjs up` 重建。
- 运行日志：`../chat-wt/_runtime/devin/`（不进仓库，可删）。
