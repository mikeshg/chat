# Chat V2 接手文档

**写于：2026-09-25 21:30（Asia/Shanghai）　下一次接手：新会话（用户明天开）**

新会话先读本文件，再读 [plan.md](plan.md)（总计划）和 [devin-rules.md](devin-rules.md)（派给 Devin 的通用规则）。

## 1. 一句话现状

第 1 阶段（5 个任务）已完成、合并、验收并提交；第 2 阶段的共用 schema（P2-0）已提交，4 份任务单已写好，但 4 个 Devin 刚启动就遇到 **swe-2-max 容量不足**（服务端报 `capacity issues`，可重试），一行代码都没写就按用户要求**全部停下**，明天重试。

## 2. 用户的决定与偏好（原话要点）

- 范围（2026-09-25 选定，详见 plan.md 第 1 节）：做 A1、A2（后台开关 + 邀请）、A4（管理页备份：生成本地文件、下载、上传恢复）、B1–B5 全部、C 除 Webhook 外全部、D 前三个；**不做** A3（安全默认值，「离上线还远」）、A5（内置 HTTPS）、C4 Webhook。
- 流程：先总计划 → 分阶段 → 每阶段拆子任务、写详细任务单 → 同时派多个 **Devin swe-2-max** 在 **worktree 并行开发** → 阶段全部完成后主会话**合并验收 → 提交** → 下一阶段。
- **每 10–20 分钟检查一次** Devin 进度（用户明确要求）。
- **cua-driver**：需要时先问；用户已同意在**第 2 阶段验收时**用它测 Tauri 桌面端（系统通知弹出、任务栏闪烁、外链用系统浏览器打开、麦克风权限弹窗）。Devin 自己**禁止**使用 cua-driver，只能用无头 Playwright。
- 「不行就停下来，明天再试」——模型不可用时停，别空转。
- 语言：中文；提交信息英文、带 `Co-Authored-By` 行（见仓库既有提交）。

## 3. 分支与提交

| 分支 | 位置 | 说明 |
|---|---|---|
| `v2` | `1b2616d` + 本文件所在提交 | **集成分支**，基于 `v1-fixes-20260925@38fba28`（V1 成果，未合入 main） |
| `v2-p1-1` … `v2-p1-5` | 已合并进 `v2` | 第 1 阶段各任务分支，保留作记录，可删 |
| `main` | `a849232` | 未动。**没有推送过任何东西**，也没开 PR |

`v2` 上的关键提交：

| 提交 | 内容 |
|---|---|
| `56a2306` | 总计划、Devin 规则、第 1 阶段任务单；`vite.config.ts` 支持 `CHAT_API` / `CHAT_WEB_PORT` |
| `7bef580` … `309f6b4` | 合并 P1-4、P1-1、P1-3（+1 个交叉修复）、P1-5、P1-2 |
| `b5088dd` | 第 1 阶段验收记录 [acceptance-p1.md](acceptance-p1.md) |
| `1b2616d` | **P2-0**：迁移 `0012_v2_media.sql` + 模型/TS 字段 + 第 2 阶段 4 份任务单 + devin-rules 更新 |

## 4. 阶段进度

| 阶段 | 任务 | 状态 |
|---|---|---|
| 1 | P1-1 系统通知、P1-2 注册开关+邀请码、P1-3 草稿+暗色、P1-4 富文本、P1-5 改密+设备管理 | ✅ 已提交；验收见 acceptance-p1.md（cargo 59+1、vitest 114、E2E 58/58 + 重启后 9/9、浏览器实测） |
| 2 | P2-0 共用 schema | ✅ 已提交（cargo 59+1 全过） |
| 2 | P2-1 缩略图+配额、P2-2 头像、P2-3 语音、P2-4 文件传输助手+转发 | ⏸ 任务单已写（`tasks/P2-*.md`），**未开始**（模型容量不足） |
| 3 | P3-1 全局搜索+文件列表、P3-2 公告+置顶+会议链接、P3-3 ntfy 推送+心跳 | 📝 只有设计草稿 [phase3-4-draft.md](phase3-4-draft.md)，第 2 阶段合并后再写正式任务单 |
| 4 | P4-1 备份/恢复、P4-2 E2E 扩展+文档 | 📝 同上 |

第 1 阶段遗留未测（写在 acceptance-p1.md 第 5 节）：Tauri 桌面真实窗口行为、Android APK 构建。计划在第 2 阶段验收时用 cua-driver 一起补。

## 5. 明天从哪开始（按顺序）

1. **确认模型可用**（几秒钟）：

   ```bash
   devin -p "只回复 ok" --model swe-2-max --respect-workspace-trust false
   ```

   还报 `capacity issues` → 告诉用户，问是等还是换 `swe-2-high`（`run-devin.sh` 支持环境变量 `DEVIN_MODEL=swe-2-high`）。**不要自行换模型**。
2. **建第 2 阶段 worktree**（仓库根目录，约 1 分钟）：

   ```bash
   node scripts/v2/wt-setup.mjs P2-1,P2-2,P2-3,P2-4
   ```

3. **派发 4 个 Devin**：每个任务用 Bash 工具 `run_in_background: true` 单独起一条：

   ```bash
   bash scripts/v2/run-devin.sh P2-1 new
   ```

4. **每 15 分钟检查**：后台起 `sleep 900` 当计时器，到点看 `../chat-wt/_runtime/devin/<任务>.run*.log` 的末尾、`<任务>.resume.log`、各 worktree 的 `git status --short | wc -l`。
5. **逐个审查 → 在任务分支提交 → 合并进 `v2`**（流程和注意事项见第 7 节）。
6. **验收**：见第 8 节；这一阶段要**用 cua-driver 补测 Tauri 桌面端**（用户已同意）。
7. 写 `acceptance-p2.md` 提交；清理 worktree 与容器；按 phase3-4-draft.md 写第 3 阶段任务单，继续。

## 6. 编排工具（`scripts/v2/`，已提交）

| 文件 | 作用 |
|---|---|
| `wt-setup.mjs` | 一个阶段的任务 → 分支 `v2-<任务>` + worktree `../chat-wt/<任务>` + tmpfs Postgres（槽位 1..N：库 55601+、后端 18101+、前端 15101+）+ `.env.local`（随机口令）+ `.devin/config.local.json`（Exec 白名单）+ pnpm 离线安装 + 生成启动提示 |
| `prompt.template.md` | Devin 启动提示模板（`{TASK}`、`{SPEC}`） |
| `run-devin.sh` | 跑一个 Devin：注入环境变量、看门狗、被拒自动续跑、容量不足退避、释放端口 |
| `continue.prompt.md` | 续跑时发给 Devin 的提示 |
| `acc-env.mjs` | 验收库 `chat-v2-acc`（55610），`up` 每次都是全新库 |
| `acc-server.mjs` | 验收后端 18110（托管 `app/dist`），`start` / `stop` |
| `union.mjs` | 合并冲突「两边都保留」（CRLF 兼容），只用于确认是纯追加的冲突 |

运行时文件（日志、提示、验收 env 与 pid）都在 `../chat-wt/_runtime/`，不进仓库。

## 7. 合并注意事项（第 1 阶段踩过的坑）

- **先审查再提交**：Devin 不提交；主会话在 worktree 里看 `git diff`、补跑检查、在任务分支上提交，再 `git merge --no-ff v2-<任务>`。
- **格式化噪音**：不改服务端的任务如果带了 `server/` 的纯 `cargo fmt` 改动，提交前 `git checkout -- server/` 还原。现在 `v2` 已经 fmt 干净，这个问题应该不会再出现。
- **Tauri 生成物**：`app/src-tauri/gen/schemas/*`、`Cargo.lock`、`pnpm-lock.yaml` 冲突时取一边，再 `pnpm install --offline` + `cd app/src-tauri && cargo check` 重新生成。
- **文件末尾追加冲突**（`styles.css`、`it.rs`）：两边都在 EOF 追加时 git 会把块切碎。可靠做法：取 HEAD 版本，再把另一分支里 `/* ===== V2 <任务> ===== */` 或 `// ===== V2 <任务> =====` 之后的整段追加到末尾（第 1 阶段 P1-2 就是这么合的）。
- **工作区是 CRLF**（`core.autocrlf=true`）：用 node 脚本改文件时正则要兼容 `\r\n`；bash 单引号里的 `node -e` 脚本**不能含单引号**（第 1 次写 0012 迁移时 SQL 引号被吞掉过）。
- **交叉编译错误**：一个分支给共享类型加必填字段、另一个分支的测试构造了这个类型 → 合并后 `tsc` 报错。每合一个分支都跑 `npx tsc -b`，别只跑 vitest。
- **新迁移**要追加到 `server/tests/migrations.rs` 的迁移列表（已写进 devin-rules）。
- 第 2 阶段预期冲突点：`ChatView.tsx`（4 个任务都会碰：头像、图片气泡、语音、菜单）、`types.ts`、`api.ts`、`styles.css`、`it.rs`、`api.rs` 路由表、`server/Cargo.toml`（P2-1 加 `image`）。

## 8. 验收步骤（每阶段）

```bash
node scripts/v2/acc-env.mjs up
set -a; . ../chat-wt/_runtime/acc.env; set +a
cd server && cargo fmt --check && CARGO_TARGET_DIR=target/v2-acc cargo test --tests && CARGO_TARGET_DIR=target/v2-acc cargo build
cd ../app && npx tsc -b && npx vitest run && npm run build
(cd src-tauri && cargo check)
cd .. && node scripts/v2/acc-server.mjs start
curl -s -H 'content-type: application/json' -d '{"username":"accadmin","password":"accpass123"}' http://127.0.0.1:18110/v1/auth/register
E2E_ADMIN_USER=accadmin E2E_ADMIN_PASS=accpass123 BASE_URL=http://127.0.0.1:18110 node scripts/e2e.mjs
node scripts/v2/acc-server.mjs stop && node scripts/v2/acc-server.mjs start
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

## 10. 当前资源状态

| 资源 | 状态 |
|---|---|
| 第 1、2 阶段 worktree、测试容器 `chat-v2-s*`、验收容器 `chat-v2-acc`、验收后端 | 已全部清理 |
| 第 2 阶段分支 `v2-p2-*` | 已删除（都是空的，明天由 `wt-setup.mjs` 重建） |
| Devin 进程 | 本轮起的已全部结束。用户自己的 Devin Desktop、RustRover 里的 devin 在运行，**不要动** |
| V1 遗留：后端 18082、前端 15173、容器 `chat-fixes-20260925-6825`（55433） | 上一轮留下的，本轮没动；需要清理时先问用户（拆除命令见 `docs/design/handover-20260925.md` 第 6 节） |
| 内置浏览器 | 开着两个标签页（验收用，后端已停，可直接关） |
