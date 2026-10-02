# Chat V2 接手文档

**更新于：2026-09-26 10:50（Asia/Shanghai）**

新会话先读本文件，再读 [plan.md](plan.md)（总计划）和 [devin-rules.md](devin-rules.md)（派给 Devin 的通用规则）。

## 1. 一句话现状

**V2 全部 4 个阶段已完成、合并、验收并提交**（各阶段验收见 acceptance-p1 … p4）。`v2` 分支没有合入 `main`、没有推送，是否合入 / 推送 / 部署由用户决定。后续如果继续开发，可沿用本文件的流程和 `scripts/devin/` 工具。

## 2. 用户的决定与偏好（原话要点）

- 范围（2026-09-25 选定，详见 plan.md 第 1 节）：做 A1、A2（后台开关 + 邀请）、A4（管理页备份：生成本地文件、下载、上传恢复）、B1–B5 全部、C 除 Webhook 外全部、D 前三个；**不做** A3（安全默认值，「离上线还远」）、A5（内置 HTTPS）、C4 Webhook。
- 流程：先总计划 → 分阶段 → 每阶段拆子任务、写详细任务单 → 同时派多个 Devin 在 **worktree 并行开发** → 阶段全部完成后主会话**合并验收 → 提交** → 下一阶段。
- **执行者分配（2026-09-26 用户澄清）**：同时最多 **2 个 Devin `swe-2-max`**（同时开 4 个会报容量不足），其余任务用 **Grok Build 命令行**（`C:\Users\xie_f\.grok\bin\grok.exe`，模型 `grok-4.7-build-fast`、`--reasoning-effort xhigh`），用 `scripts/devin/run-grok.sh <任务号> <提示文件绝对路径>` 跑。**不是** Devin 里的 grok 模型——第 2 阶段误用了 Devin 的 `grok-4-7-xhigh`，第 3 阶段因此把 Devin 当日额度用完。
- **每 10–20 分钟检查一次** Devin 进度（用户明确要求）。
- **cua-driver**：需要时先问；用户同意过在第 2 阶段验收时用它测 Tauri 桌面端（已完成）。以后再用前仍要先问。Devin 自己**禁止**使用 cua-driver，只能用无头 Playwright。
- 「不行就停下来，明天再试」——模型不可用时停，别空转。
- 语言：中文；提交信息英文、带 `Co-Authored-By` 行（见仓库既有提交）。

## 3. 分支与提交

| 分支 | 位置 | 说明 |
|---|---|---|
| `v2` | 本文件所在提交 | **集成分支**，基于 `v1-fixes-20260925@38fba28`（V1 成果，未合入 main） |
| `v2-p1-*`、`v2-p2-*` | 已合并进 `v2` | 各任务分支，保留作记录，可删 |
| `main` | `a849232` | 未动。**没有推送过任何东西**，也没开 PR |

`v2` 上的关键提交：

| 提交 | 内容 |
|---|---|
| `56a2306` | 总计划、Devin 规则、第 1 阶段任务单；`vite.config.ts` 支持 `CHAT_API` / `CHAT_WEB_PORT` |
| `7bef580` … `309f6b4` | 合并 P1-4、P1-1、P1-3（+1 个交叉修复）、P1-5、P1-2 |
| `b5088dd` | 第 1 阶段验收记录 [acceptance-p1.md](acceptance-p1.md) |
| `1b2616d` | **P2-0**：迁移 `0012_v2_media.sql` + 模型/TS 字段 + 第 2 阶段 4 份任务单 + devin-rules 更新 |
| `7e149cd` | 编排脚本 `scripts/devin/`、第 3/4 阶段草稿、本文件初版 |
| `f831451` … `cb34787` | 合并 P2-1、P2-2、P2-3、P2-4 |
| `4d2998f` | 第 2 阶段验收记录 [acceptance-p2.md](acceptance-p2.md)、复选框布局修复、合并脚本 |
| `662daca` | 第 3 阶段任务单 |
| `0cec2cd` | Grok Build 运行脚本、后端启停脚本、测试库恢复脚本、端口调整 |
| `b8bab0d` … `017ef2b` | 合并 P3-1、P3-3、P3-2 |
| `d08b1d4` | 第 3 阶段验收记录 [acceptance-p3.md](acceptance-p3.md)、`run-devin.sh` 额度用完即停 |
| `53e324f` | 第 4 阶段任务单 |
| `7259d2e`、合并 P4-1 的提交 | 合并 P4-2、P4-1 |
| 本文件所在提交 | 第 4 阶段 / V2 最终验收记录 [acceptance-p4.md](acceptance-p4.md)、README 补备份一项 |

## 4. 阶段进度

| 阶段 | 任务 | 状态 |
|---|---|---|
| 1 | P1-1 系统通知、P1-2 注册开关+邀请码、P1-3 草稿+暗色、P1-4 富文本、P1-5 改密+设备管理 | ✅ 验收见 acceptance-p1.md |
| 2 | P2-0 共用 schema、P2-1 缩略图+配额、P2-2 头像、P2-3 语音、P2-4 文件传输助手+转发 | ✅ 验收见 acceptance-p2.md（cargo 83+2、vitest 134、E2E 58/58 + 重启后 9/9、浏览器实测、Tauri 桌面实测） |
| 3 | P3-1 全局搜索+文件列表、P3-2 公告+置顶+会议链接、P3-3 ntfy 推送+心跳 | ✅ 验收见 acceptance-p3.md（cargo 104+3、vitest 154、E2E 58/58 + 重启后 9/9、浏览器实测含假 ntfy 推送） |
| 4 | P4-1 备份/恢复、P4-2 E2E 扩展+文档 | ✅ 验收见 acceptance-p4.md（cargo 111+3、vitest 158、扩展 E2E 105/105 + 重启后 13/13、备份→改数据→恢复实测、恢复后 phase2 13/13） |

仍未覆盖：Android APK 构建；真实麦克风录音（WebView2 权限没点允许）；本机开着「请勿打扰」，系统通知横幅没看到，只确认了通知中心收到。

## 5. 下一步

V2 已全部完成，没有待办开发项。可选的后续：

1. 用户决定是否把 `v2` 合入 `main`、推送、部署（`docker compose up -d` 即可，V2 没有新增环境变量）。
2. 真实环境补测：Android APK 构建与安装、真实手机 ntfy App 收推送、真实麦克风录音、大体量数据的备份恢复。
3. 若继续开发新阶段：沿用第 3–9 节的流程与工具。

## 6. 编排工具（`scripts/devin/`，已提交）

| 文件 | 作用 |
|---|---|
| `wt-setup.mjs` | 一个阶段的任务 → 分支 `v2-<任务>` + worktree `../chat-wt/<任务>` + tmpfs Postgres（槽位 1..N：库 45601+、后端 18101+、前端 15101+）+ `.env.local`（随机口令）+ `.devin/config.local.json`（Exec 白名单）+ pnpm 离线安装 + 生成启动提示 |
| `prompt.template.md` | Devin 启动提示模板（`{TASK}`、`{SPEC}`） |
| `run-devin.sh` | 跑一个 Devin：注入环境变量、看门狗、被拒自动续跑、容量不足退避、释放端口 |
| `continue.prompt.md` | 续跑时发给 Devin 的提示 |
| `acc-env.mjs` | 验收库 `chat-v2-acc`（45610），`up` 每次都是全新库 |
| `acc-server.mjs` | 验收后端 18110（托管 `app/dist`），`start` / `stop` |
| `union.mjs` | 合并冲突「两边都保留」（CRLF 兼容），只用于确认是纯追加的冲突；`--theirs-first` 把另一分支的块放前面（两边都往同一结构体末尾加字段、而 HEAD 还在后面接了新定义时用） |
| `append-section.mjs` | `node scripts/devin/append-section.mjs v2-p2-2 P2-2 server/tests/it.rs`：文件末尾追加冲突 = HEAD + 另一分支 `===== V2 <任务号>` 分节之后的整段 |

运行时文件（日志、提示、验收 env 与 pid）都在 `../chat-wt/_runtime/`，不进仓库。

## 7. 合并注意事项（第 1 阶段踩过的坑）

- **先审查再提交**：Devin 不提交；主会话在 worktree 里看 `git diff`、补跑检查、在任务分支上提交，再 `git merge --no-ff v2-<任务>`。
- **格式化噪音**：不改服务端的任务如果带了 `server/` 的纯 `cargo fmt` 改动，提交前 `git checkout -- server/` 还原。现在 `v2` 已经 fmt 干净，这个问题应该不会再出现。
- **Tauri 生成物**：`app/src-tauri/gen/schemas/*`、`Cargo.lock`、`pnpm-lock.yaml` 冲突时取一边，再 `pnpm install --offline` + `cd app/src-tauri && cargo check` 重新生成。
- **文件末尾追加冲突**（`styles.css`、`it.rs`）：两边都在 EOF 追加时 git 会把块切碎。先用 `git diff <阶段基点> <分支> -- <文件>` 确认该分支只有一个在文件尾的 `@@`，再用 `append-section.mjs`。**不要**在 Git Bash 里传以 `/` 开头的参数（会被当成路径改写，`//` 变 `/`）。
- **同一结构体被两边加字段**：看清 HEAD 那侧是不是在结构体后面还接了新定义，是的话用 `union.mjs --theirs-first`，合完用编译器确认字段落在对的结构体里。
- **工作区是 CRLF**（`core.autocrlf=true`）：用 node 脚本改文件时正则要兼容 `\r\n`；bash 单引号里的 `node -e` 脚本**不能含单引号**（第 1 次写 0012 迁移时 SQL 引号被吞掉过）。
- **交叉编译错误**：一个分支给共享类型加必填字段、另一个分支的测试构造了这个类型 → 合并后 `tsc` 报错。每合一个分支都跑 `npx tsc -b`，别只跑 vitest。
- **新迁移**要追加到 `server/tests/migrations.rs` 的迁移列表（已写进 devin-rules）。
- 第 3 阶段预期冲突点：`ChatView.tsx` 头部（P3-2 公告/置顶/会议按钮）与消息菜单（P3-2 置顶）、`ConvList.tsx`（P3-1 搜索框）、`GroupInfo.tsx`（P3-1 文件列表、P3-2 公告编辑）、`ws.rs`（P3-3 心跳与推送、P3-2 `pins_updated`）、`settings.rs` / `AdminSettings.tsx`（P3-2 会议地址、P3-3 推送配置）、`api.rs` 路由表、`it.rs`、`styles.css`。

## 8. 验收步骤（每阶段）

```bash
node scripts/devin/acc-env.mjs up
set -a; . ../chat-wt/_runtime/acc.env; set +a
cd server && cargo fmt --check && CARGO_TARGET_DIR=target/v2-acc cargo test --tests && CARGO_TARGET_DIR=target/v2-acc cargo build
cd ../app && npx tsc -b && npx vitest run && npm run build
(cd src-tauri && cargo check)
cd .. && node scripts/devin/acc-server.mjs start
curl -s -H 'content-type: application/json' -d '{"username":"accadmin","password":"accpass123"}' http://127.0.0.1:18110/v1/auth/register
E2E_ADMIN_USER=accadmin E2E_ADMIN_PASS=accpass123 BASE_URL=http://127.0.0.1:18110 node scripts/e2e.mjs
node scripts/devin/acc-server.mjs stop && node scripts/devin/acc-server.mjs start
BASE_URL=http://127.0.0.1:18110 node scripts/e2e.mjs phase2
```

- 全新库的第一个注册用户自动成为管理员，所以 E2E 前**先注册一个管理员**，否则「非管理员 403」两项会误报失败。
- 必须用 `CARGO_TARGET_DIR=target/v2-acc`：V1 交接时留下的后端（端口 18082，PID 见 `server/target/fixes-20260925/server.pid`）占着 `server/target/debug/chat-server.exe`。那是上一轮的资源，本轮没动。
- 浏览器实测用内置浏览器：`127.0.0.1:18110` 和 `localhost:18110` 两个 origin 可以同时登录两个账号。窗格被挡住时截图会超时，用 `javascript_tool` 操作页面、读 DOM 即可。
- 截图可以直接看 Devin 留在 `<worktree>/server/target/wt/*.png` 里的（删 worktree 前看）。

## 9. Devin 运行要点（踩过的坑）

- **非交互模式下，需要确认的命令会被自动拒绝，会话随之结束**。最常见的是读 `.env.local` / 用 psql 探库 → 解决：`run-devin.sh` 把 `.env.local` 注入进程环境，规则里也禁止读它。
- **Devin 起的后台 vite / 后端会让进程挂住不退出**（第 1 阶段 3 个任务因此空转了约 1 小时）→ 看门狗 + 规则里要求「用完关掉」。
- Devin 曾试图自己用 cua-driver（被拒绝了）→ 规则里已明确禁止。
- 权限模式用 `smart` + 白名单，没用 `dangerous`。
- **不要在包装脚本运行时编辑它**：bash 边执行边读文件，改了会读错位。要改就另存新文件。
- 会话名从 `%APPDATA%/devin/cli/logs/devin_*.log` 里按 worktree 路径匹配 `for session <name>` 取得，`-r <name>` 续跑。
- **WSL 空闲会自动关机，测试库容器随之消失**（Docker 跑在 WSL 的 Debian 里，没有前台 wsl 会话时虚拟机约一分钟后关闭）。第 1、2 阶段没出事，是因为 V1 遗留脚本一直挂着一个 wsl 前台进程；机器重启后它没了，第 3 阶段开工 15 分钟就丢了库。派发前先在后台常驻保活：`wsl -d Debian -- sh -c "exec sleep 86400"`；库丢了用 `node scripts/devin/wt-db-up.mjs P3-1,P3-2,P3-3` 按原口令、原端口重建。
- **测试库端口要避开 Windows 临时端口段 49152–65535**（会被随机进程占用，第 3 阶段 55602 就被厂商托盘程序占了），现在用 45601 起。

## 10. 当前资源状态

| 资源 | 状态 |
|---|---|
| 全部阶段的 worktree、测试容器 `chat-v2-s*`、验收容器 `chat-v2-acc`、验收后端、假 ntfy、Tauri 调试客户端、WSL 保活进程、内置浏览器标签页 | 已全部清理 |
| 运行记录 | `../chat-wt/_runtime/`（Devin / Grok 日志、验收日志、截图），不进仓库，可随时删除 |
| Devin 进程 | 本轮起的已全部结束。用户自己的 Devin Desktop、RustRover 里的 devin 在运行，**不要动** |
| V1 遗留：后端 18082、前端 15173、容器 `chat-fixes-20260925-6825`（55433） | 上一轮留下的，本轮没动；需要清理时先问用户（拆除命令见 `docs/archive/handover-20260925.md` 第 6 节） |
| 桌面测试留下的痕迹 | 用户的 Edge 里有一个 example.com 标签页；Windows 通知中心里有两条 Chat 测试通知 |
