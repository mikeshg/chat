# Devin E 执行单：V1 修复收尾与浏览器级复验

你接手 Chat V1 的修复收尾。**现状以 `docs/archive/handover-20260925.md` 为准**（先完整读它，再读 `delivery-D.md` §1、§2、§4 的复现步骤）。工作区有约 8k 行未提交改动，全部是要保留的成果。

## 任务（按顺序做，每项完成后在 `docs/archive/delivery-E.md` 追加真实记录）

**E1 编辑流程修复（handover §5 第 5 项，唯一确认未修的代码缺陷）**
- `app/src/ui/ChatView.tsx` 的 `submitEdit` 改为使用已有的 `prepareEditSubmission`：用 `useRef` 保存上一次提交，内容未变的重试复用同一 `clientOpId` 与 `expectedVersion`，内容变化或换消息才生成新 op。
- 完成/失败回调加归属判定，沿用本文件与 `PollCard.tsx`/`NewPollModal.tsx` 已有的 owner token + `session()` 代次模式：用户已取消编辑、改编辑另一条、或切换账号后，旧回调不得清 `editingMessage`、不得覆盖输入框。
- 先写能在修复前失败的测试（可把判定逻辑抽成纯函数测，别引入新依赖），再修。确认服务端对同一 `client_op_id` 的编辑重放返回已有视图（`it.rs` 若无对应用例则补）。

**E2 浏览器级复现 M1–M5 + 编辑两场景**
- 在现有隔离环境跑：前端 `http://127.0.0.1:15173`（代理到 18082）。按 `delivery-D.md` §1 各条“复现/真实浏览器”步骤逐条重跑 M1–M5，外加：编辑丢响应后重试不 409、取消编辑后新草稿不被旧回调清空。
- 时序/故障注入用网络层手段（请求拦截、延迟、丢弃一次响应），**不得直接改 store 伪造成功**。M4 用 15 秒后截止的投票跨过截止时刻；M5 只丢一次创建响应再点重试。
- 若本机无浏览器自动化：可在 `server/target/fixes-20260925/browser/` 建临时 node 项目装 playwright，**不得加进项目依赖**。
- 每条记录：步骤、预期、实际、证据路径（截图/JSON 放 `server/target/fixes-20260925/browser/`）。失败就如实写失败并定位原因，修了再重跑。

**E3 agent 自写测试的可信度审查（handover §5 第 2 项）**
- 对 handover §4 表中列出的测试 + `poll_creation_replay_is_private_over_rest_and_ws` + `app/src/*.test.ts`：逐个判断“修复被回退时是否会失败”。
- 至少做 3 次变异验证并还原：H1（`api.rs` 与 `ws.rs` 去掉 `inserted` 判断改为总是广播）、M1（`store.ts selectMessage` 去掉版本比较）、M2（`selectPreference` 去掉时间比较）。测试必须变红，否则补强断言。改完用 `git diff` 确认变异已完全还原。
- 输出表：测试名 / 覆盖的缺陷 / 变异结果 / 是否补强。

**E4 服务端补充平面边界用例**：`it.rs` 补投票问题 200×"𠀀" 通过、201 拒绝；选项 100/101；消息 4000/4001。

**E5 管理员 E2E**：首个注册用户即管理员。先在隔离库查最早注册的用户及其角色（只输出用户名和角色，不输出任何哈希/token）；若是 e2e 脚本建的账号（一次性口令 `pass123456`），用 `E2E_ADMIN_USER=<名> E2E_ADMIN_PASS=pass123456` 跑；否则在同一隔离容器里新建一个你自己的库 + 18083 端口后端做一次，结束后清理你建的库和进程。

**E6 全量复验**：按 handover §7（已修正版）完整重跑：build、vitest、cargo test、E2E phase1、真实停止并重启后端后 phase2、H1 隐私探针、冻结安装、`git diff --check`。

**E7 交付**：写 `docs/archive/delivery-E.md`（每条命令 + 真实结果 + 未覆盖项），并更新 `handover-20260925.md` §5 各项状态。不在范围：混合客户端版本兼容（需产品决策，只记录）、Tauri/Android、视觉像素审查。

## 硬约束（违反任何一条即停止并报告）

- 禁止 `git reset/checkout/restore/clean/stash/commit/push/rebase`；不得删除或覆盖既有改动；`.idea/` 不是你的，别碰。
- 禁止 `cargo clean`：`server/target/fixes-20260925/` 在 cargo target 目录里，里面是隔离环境脚本和凭据，会被一起删掉。
- 只用隔离环境：数据库 `127.0.0.1:55433`（URL 从 `server/target/fixes-20260925/runtime.json` 读入环境变量）、后端 18082、前端 15173。**永远不连 8080 和 5432**，不 DROP 任何不是你建的库。
- 不在文档、日志、终端输出里打印 `runtime.json` 内容、token、口令哈希。
- 不新增项目依赖，不重写模块；临时文件只放 `server/target/fixes-20260925/` 下，结束只清理你自己建的。
- Windows 注意：后端运行时 `chat-server.exe` 被占用，`cargo build/test` 会失败。只跑测试用 `CARGO_TARGET_DIR=target/fixes-20260925/verify-target`；改了服务端代码要进浏览器验证时，先停 PID（`server.pid`）再 `cargo build`，再用 `node server/target/fixes-20260925/server.mjs` 重启。
- E2E phase2 必须带与 phase1 相同的 `BASE_URL=http://127.0.0.1:18082`。
- Shell 是 Windows + Git Bash；PowerShell 嵌套引号容易被截断，优先用 Git Bash 或写脚本文件。

## 结束时

在终端输出一张表：E1–E7 各自状态（完成/部分/未做）+ 证据路径 + 剩余风险。不要声称未亲自执行的结果。
