# V2 Devin 通用执行规则（每个任务必读）

你在一个独立的 git worktree 里开发一个子任务。同一时间还有其他 Devin 在别的 worktree 开发同阶段的其他任务，完成后由主会话统一合并。先读完本文件和你的任务单，再读 `CONTEXT.md`（领域词汇与边界规则）和 `docs/adr/001-decisions.md`。

## 1. 环境

- 系统：Windows 11，优先用 Git Bash；PowerShell 的嵌套引号容易被截断，复杂命令写成脚本文件再执行。
- worktree 根目录的 `.env.local` 里是**只属于你**的数据库和端口，这些变量**已经注入你的进程环境**，直接用 `$DATABASE_URL` 等即可。**不要**读取、打印或 source `.env.local`，也不要用 psql 等单独探测数据库（非交互模式会被拒绝并中断你的会话），要确认数据库可用就直接跑 `cargo test`。

  - `DATABASE_URL`：cargo 集成测试用（指向 `postgres` 库，测试自动建 `chat_test`）
  - `APP_DATABASE_URL`：手动跑后端用，启动时写成 `DATABASE_URL=$APP_DATABASE_URL`
  - `PORT` / `CHAT_WEB_PORT` / `CHAT_API`：你的后端端口、前端 dev 端口、前端代理目标
  - `JWT_SECRET`、`FILES_DIR`：手动跑后端用
- **永远不要**连接 `8080`、`5432`、`55433` 或别的槽位端口；不要打印 `.env.local` 的内容、token、口令哈希。
- 依赖已装好：`pnpm install --offline` 可在 worktree 根目录补装；Rust 依赖首次编译需要几分钟，属正常。
- 跑后端手动验证（Git Bash）：

  ```bash
  cd server && DATABASE_URL=$APP_DATABASE_URL BIND=127.0.0.1 cargo run
  ```

  前端推荐直接用后端托管构建产物：先 `cd app && npm run build`，后端环境里带上 `APP_DIST=$APP_DIST`，同源访问 `http://127.0.0.1:$PORT`。（`npx vite` 也可用，`vite.config.ts` 读取 `CHAT_API` / `CHAT_WEB_PORT`，但在后台会话里常出现端口不通。）
- **后台起的服务用完必须关掉**（`taskkill //PID <pid> //T //F`），否则残留子进程会让你的会话无法正常结束。
- 浏览器实测只用**无头 Playwright**：在 `server/target/wt/browser/` 建临时 node 项目 `npm init -y && npm i playwright`（本机已缓存 Chromium，不要下载浏览器）。**禁止**使用 cua-driver、computer-use 或任何操作真实桌面 / 窗口的工具。
- 全新数据库里**第一个注册的用户自动成为管理员**；测试普通用户权限时先注册一个占位用户。
- Windows 注意：后端运行时 `chat-server.exe` 被占用，`cargo build/test` 会报 `failed to remove file`。先停后端，或测试时用 `CARGO_TARGET_DIR=target/verify cargo test --tests`。

## 2. 硬约束（违反任何一条就停下并在交付文档里说明）

- 只改你的 worktree。禁止 `git commit/push/reset/checkout/restore/clean/stash/rebase/branch/worktree`；可以用 `git status/diff/log/show/add`。
- 禁止 `cargo clean`、禁止操作 docker / wsl。
- 只做任务单范围内的事；发现范围外的问题记在交付文档「发现的其他问题」里，不顺手改。
- 不新增任务单没列出的依赖（npm / cargo 都一样）。
- 迁移只用任务单分配的编号；不修改已有迁移文件（`0001` 起到当前最新，及其他任务的迁移）。新迁移要追加到 `server/tests/migrations.rs` 的迁移列表末尾。
- 不在 `server/src/config.rs` 的 `Config` 里加字段；可配项放 `server_settings`（第 1 阶段 P1-2 建立）。
- 临时文件只放 `server/target/wt/` 下（已被忽略），结束时清理你自己建的。

## 3. 代码约定

- 读懂要改的代码和调用链再动手；风格、注释密度、命名跟周围代码一致（中文注释）。
- 最小实现：不加没用到的抽象、不写「以后可能用」的配置。
- 权限一律服务端强制，客户端隐藏按钮不算权限。
- 旧客户端兼容：新增响应字段可选；新 WS 帧类型旧客户端会忽略。
- 新功能尽量放新文件；共享文件（`api.rs` 路由表、`ChatView.tsx`、`MePanel.tsx`、`styles.css`、`types.ts`、`api.ts`）只加必要的接入代码，别重排、别格式化无关部分。
- 设置类 UI 做成独立组件，在 `MePanel.tsx` / `AdminPanel.tsx` 里只加一行挂载。
- 新 CSS 追加在 `styles.css` 末尾，带 `/* ===== V2 <任务号> ===== */` 分节注释；颜色用 CSS 变量（`--bg`、`--panel`、`--text`、`--muted`、`--line`、`--green` 等），不要写死十六进制色。
- `server/tests/it.rs` 新测试追加到文件末尾，带 `// ===== V2 <任务号> =====` 分节注释，复用已有 helper（`spawn`、`register`、`befriend`、`dm_conv`、`ws_url`、`next_of`、`spawn_named_db` 等）。改全局设置 / 影响整库的测试必须用 `spawn_named_db(&format!("chat_it_{}", uniq()))` 独立库。
- 先写能在实现前失败的测试，再实现（至少覆盖权限拒绝、主路径、一个边界）。

## 4. 必须通过的检查（交付前亲自执行）

```bash
cd server && cargo fmt && cargo test --tests        # 全部通过，不只是你的新测试（基线已 fmt 干净，cargo fmt 只会动你改的代码）
cd ../app && npx vitest run && npm run build         # build 含 tsc，必须跑
```

任务单里有额外检查（如 Tauri `cargo check`、浏览器实测）也必须执行。任何一项失败先修；修不了就如实写进交付文档。

## 5. 交付

写 `docs/archive/v2/delivery/<任务号>.md`，内容：

1. 实现摘要：改了哪些文件、各做了什么（一行一个）。
2. 接口契约：新增 / 变更的 REST、WS 帧、表结构（和任务单不一致的地方要标出并说明原因）。
3. 验证记录：每条命令 + 真实结果（通过数 / 失败数）；没跑的写「未运行」和原因。
4. 未覆盖项与已知限制。
5. 发现的其他问题（范围外，只记录）。

最后在终端输出一张表：任务单各子项 → 完成 / 部分 / 未做 → 证据。不要声称没有亲自执行过的结果。
