# Devin E 交付：V1 修复收尾与浏览器级复验

**执行日期：2026-09-25。环境：隔离库 127.0.0.1:55433（容器 `chat-fixes-20260925-6825`）、后端 18082、前端 15173。全部数字为本会话亲自执行。**

## E1 编辑流程修复（submitEdit 未接线）

**缺陷**（handover §5 第 5 项）：`ChatView.tsx` 的 `submitEdit` 每次 `crypto.randomUUID()`，丢响应重试拿新 op_id + 旧版本撞 409；完成回调无归属判定，取消/改编辑/换账号后会清 `editingMessage`、覆盖新草稿。`prepareEditSubmission` 只被单测调用。

**修复前测试（先红）**：`app/src/ui/ChatView.test.ts` 新增 `ownsEditResult` / `editRejected` 两组判定用例。修复前运行 `npx vitest run src/ui/ChatView.test.ts` → **3 failed**（`TypeError: ... is not a function`，函数不存在）。证据：`server/target/fixes-20260925/e1-test-red.log`。

**修复内容**（`app/src/ui/ChatView.tsx`）：

- `submitEdit` 改用 `prepareEditSubmission(editSubmission.current, …)`：同消息同内容的未确认重试复用同一 `clientOpId` + `expectedVersion`；`editRejected(e)`（400/403/404/409/422）清除留存的 op，其余错误（网络/5xx）保留供幂等重试。
- 归属判定 `ownsEditResult`：组件 owner token（`viewOwner`）+ 编辑代次（`draftRevision`，`startEdit`/`cancelEdit`/会话切换时递增）+ 目标消息 id + activeConv + `session()` 代次。成功回调在组件 owner 仍有效时（含已取消/改编辑另一条）按版本合并服务端视图；组件已切会话或卸载（`!current(token)`）时直接返回不合并，由 WS `message_updated` 广播补齐（主会话复核更正）。仅在仍属本编辑态时才清 `editingMessage`、还原草稿；失败回调归属失效时不写 `mutErr`。
- `mutation` ref 作在途互斥；会话切换 effect 复位 `mutation`/`mutBusy`/`draftRevision`，防跨会话忙碌卡死。

**修复后**：`npx vitest run` → **36/36**（ChatView.test.ts 5/5）；`npx tsc -b` → 通过。

**服务端重放确认**：`server/tests/it.rs` `lifecycle_edit`（:1881-1893）已覆盖同一 `client_op_id` 的编辑重放——返回已有视图（content 仍为 "v1"，`edit_version` 不二次递增），无需补用例。

## E2 浏览器级复现 M1–M5 + 编辑两场景

**工具**：`server/target/fixes-20260925/browser/`（临时 npm 项目装 playwright 1.63.0 + 本机缓存 chromium，非项目依赖）。套件 `browser-m-tests.mjs`，证据 `browser/evidence/`（results.json + 截图 + 被注入的陈旧响应 JSON）。

**故障注入手法**（全部网络层，不碰 store）：`page.route` 内 `route.fetch()` 真实打到服务端拿到响应后挂起 → `route.fulfill` 放行 = 滞后陈旧响应；`route.fetch()` 后 `route.abort()` = 响应已生成但客户端丢失。目录刷新触发方式：B 账号 REST 建群拉 A 入群 → 服务端 `invalidate:conversations` → 客户端 `refreshCatalogs`。整套仅注册 2 个共享账号（避开 api.rs rate_limit 20次/5min），B 只做 REST 种子/触发动作。

**结果**：`node browser-m-tests.mjs` → **25 passed / 0 failed**（`evidence/results.json`，run rid=beeid2cs）。逐项：

| 场景 | 步骤摘要 | 预期 | 实际 | 证据 |
|---|---|---|---|---|
| M1 | 发送→挂起目录响应→UI 编辑→放行 | conv-sub 不回退 v0 | conv-sub="m1-EDITED"，v0气泡=0 | m1-stale-conversations.json（快照确为 edit_version=0 的 m1-orig）|
| M1 | 发送→挂起目录响应→UI 删除→放行 | conv-sub 不复活正文 | conv-sub="消息已删除" | m1-stale-conversations-2.json |
| M2 | 挂起目录响应→永久静音+归档→放行 | 仍在归档 tab 且 🔕 | archived=1 mutedFlag=1；服务端回读 pref 一致 | m2-stale-conversations.json（快照 mute_forever=false, archived_at=null）|
| M3 | 回复→编辑原消息→删除原消息 | rq-text 实时变 V2 再变已删除 | "m3-orig-V2" → "原消息已删除"，无刷新 | m3-reply-preview.png |
| M4 | REST 建 15s 截止投票→跨过截止 | 无刷新下选项禁用+已截止 | disabled=2，foot="已截止"，服务端 expired=true | m4-expired.png |
| M5 | UI 建投票→abort 首次创建响应→点重试 | 只 1 条投票 | ui=1 server=1 seqs=1 | m5-single-poll.png |
| EDIT-A | UI 编辑→abort 首个 PATCH 响应→点保存 | 重试无 409 且退出编辑 | err=0 editing=0 edit_version=1 | edit-a-retry.png |
| EDIT-B | 编辑→延迟 PATCH 响应→取消→输入新草稿→放行 | 草稿不被旧回调覆盖 | composer="D1-新草稿"，编辑横幅无，消息已合并 eb-v1 | edit-b-cancel.png |

每场景另断言 0 console error（过滤 401/abort 自身噪音）。运行中发现并修正两处套件自身问题：`.conv-menu-btn` 需先 hover 父项；服务端注册限流（非缺陷）→ 共享账号重写。

## E3 agent 自写测试可信度审查 + 变异验证

**方法**：逐个判断“修复被回退时测试是否变红”，并对 handover §5 指定的 3 处变异实测，全部执行后还原、`git diff` 确认无残留（`grep -rn MUTANT app/src server/src` 为空）。

**变异实测**（全部按预期变红，断言无需补强）：

| 变异 | 改法 | 命令 | 结果 |
|---|---|---|---|
| M1 `selectMessage` 去版本比较 | `return incoming`（回到“后到覆盖”原缺陷） | `npx vitest run src/store.test.ts` | **9 failed** / 10 passed —— 生命周期乱序、陈旧创建快照、墓碑还原、投票乱序用例全红 |
| M2 `selectPreference` 去时间比较 | `return incoming ?? local` | 同上 | **1 failed** / 18 passed —— `rejects stale preferences via events and catalog` 红 |
| H1 `api.rs`+`ws.rs` 去 `inserted` 判断（重放也广播） | `if posted.inserted` → 裸块 | `cargo test --test it poll_creation_replay_is_private_over_rest_and_ws`（CARGO_TARGET_DIR=verify-target） | **FAILED**：sync 栅栏断言 `assert_ne!(f["type"], "message")` 于 it.rs:3614 捕获重放广播帧 |

**还原后复验**：`npx vitest run src/store.test.ts` → 19/19；`cargo test --test it poll_creation_replay_is_private_over_rest_and_ws` → 1 passed。

**逐测试可信度判定**（修复回退是否变红）：

| 测试 | 覆盖缺陷 | 回退会变红？ | 依据 | 补强 |
|---|---|---|---|---|
| `poll_creation_replay_is_private_over_rest_and_ws` (it.rs:3562) | H1 重放广播泄露（REST+WS 双路径、匿名选项隐私、幂等视图） | **是（实测）** | 上表 H1 变异 | 否 |
| `lifecycle_edit` (it.rs:1881) | 同 `client_op_id` 编辑重放返回已有视图、版本不二次递增 | 是 | 断言重放响应 content=="v1" 且 `edit_version` 不增；无 op 重放语义的旧实现会新建/升版本 | 否 |
| migrations.rs 升级夹具 | H2 附件关联/末次引用回收 | 是 | 夹具构造旧库跑迁移后断言附件行数与物理文件保留；断言为计数+存在性 | 否 |
| store.test.ts 生命周期/陈旧快照/引用预览/投票合并（19 例） | M1、M3 | **是（实测）** | M1 变异 9 红 | 否 |
| store.test.ts 偏好合并 | M2 | **是（实测）** | M2 变异 1 红 | 否 |
| `PollCard.test.tsx` 截止定时/过期渲染 | M4 | 部分是 | `schedulePollDeadline` 定时器与 `expired` 渲染逻辑回退会红；但“跨越截止后回读服务端”属 UI 接线行为，单测不覆盖 —— 已由 E2-M4 浏览器实测补位 | 否（缺口由浏览器覆盖） |
| `NewPollModal.test.ts` `preparePollSubmission` | M5 | 部分是 | 函数本身断言复用 `client_msg_id`；若 `create` 不传 `previous`（函数在但接线断开）单测仍绿 —— 已由 E2-M5 浏览器故障注入补位 | 否（缺口由浏览器覆盖） |
| `ChatView.test.ts` `ownsEditResult`/`editRejected`/`prepareEditSubmission` | E1 编辑流程 | 部分是 | 同上：纯函数断言逻辑正确，接线（`submitEdit` 是否调用）由 E2 EDIT-A/B 覆盖 | 否（缺口由浏览器覆盖） |
| `api.test.ts` 会话代次隔离 | 旧会话响应污染/刷新串代 | 是 | 若 `requireSession`/代次校验回退，`rejects an old-session %s response` 全部场景红 | 否 |
| `ws.test.ts` outbox/去重/read_sync | N(N+1)/2 帧放大、重复 toast、read_sync 吞计数 | 是 | `sent` 帧精确计数、toast 数、unread/has_mention 精确断言 | 否 |
| 会话撤销/升级窗口 7 测试（reset_and_disable_revoke_tokens_atomically、token_issuance_waits_for_user_authorization、registration_is_not_visible_before_token_issuance_commits、ws_rechecks_session_version_before_frames_and_preserves_other_connections、ws_reset_between_upgrade_and_registration_closes_without_presence、force_logout_actually_closes_socket、refresh_flood_no_pool_starvation） | 会话安全原子性/可见性窗口 | 是 | 断言为具体行为非宽松枚举：屏障未完成断言、半提交不可见（`json!([])`）、精确 conn_count(2→1/0)、presence 泄漏断言、踢下线帧 `reason=="disabled"` 且 socket 实际关闭、刷新洪泛后 200/401 分明 | 否 |
| 竞态测试（owner_transfer_leave_race、dissolve_invite_race、admin_role_safety） | 竞态终态收敛 | 是 | §5.3 已收紧：只接受明确状态码集（204/400/403），无 5xx 空间；终态成员/owner/管理员计数精确 | 否 |

**结论**：agent 自写测试非 happy-path 摆设 —— 3 处变异全部被已有断言当场杀死，无需补强。诚实缺口：三个 UI 接线类缺陷（M4 回读、M5/M1 提交链路接线）纯单测无法捕获“函数在但未被调用”的退化，此项已由 E2 浏览器级覆盖，属已知互补而非盲区。


## E4 服务端补充平面边界用例

`server/tests/it.rs` 新增 `boundary_char_counts`（Devin 编写）：投票问题 200×"𠀀" 通过 / 201 拒绝 400；选项 100 / 101 同理；WS 消息 4000 通过 / 4001 返回 `error` 帧 `code=bad_message`。Devin 运行该用例时被 smart 权限模式拒绝（需读取 `runtime.json` 的库凭据），**由主会话代跑**：`CARGO_TARGET_DIR=target/fixes-20260925/verify-target cargo test --tests` → it **43 passed**（含 `boundary_char_counts ... ok`）、migrations **1 passed**。

## E5 管理员 E2E（主会话执行）

Devin 查询管理员账号时被 smart 模式拒绝（命令要读取数据库口令），本项由主会话接手。

- 隔离库 `chat_fixes` 中最早注册用户是 H1 探针建的 `rp6825…a`（admin），口令为随机值且未保存，无法用于登录。
- 因此在同一隔离容器新建 `chat_e5_admin` 库，并在 `127.0.0.1:18083` 起独立后端（`server/target/fixes-20260925/server-e5.mjs`，使用同一 13:07 二进制；服务端源码自 Devin 交接以来未变）。
- 首个注册用户 `e5admin`（一次性测试口令）经 SQL 回读确认 `role=admin`。
- `BASE_URL=http://127.0.0.1:18083 E2E_ADMIN_USER=e5admin E2E_ADMIN_PASS=… node scripts/e2e.mjs` → **58 passed / 0 failed**，包含 admin login / overview / user search 与非管理员 403。日志：`e5-e2e-phase1.log`。
- 停止 18083 进程，重启后跑 `BASE_URL=http://127.0.0.1:18083 node scripts/e2e.mjs phase2` → **9 passed / 0 failed**。日志：`e5-e2e-phase2.log`。
- 清理：18083 进程已停；`chat_e5_admin` 已 DROP；`files_dir-e5` 已删；`scripts/.e2e-state.json` 已删。

## E6 全量复验（主会话执行，2026-09-25 14:40 前后）

| 检查 | 结果 |
|---|---|
| `git diff --check` | 通过 |
| `cd app && npm run build` | 通过，41 modules，JS 211.21 kB / CSS 17.23 kB |
| `npx vitest run` | **36 / 36**，6 个文件（E1 新增 3 例） |
| `cargo test --tests`（55433 隔离库，独立 target 目录） | it **43** + migrations **1**，全绿 |
| E2E phase1（含管理员）/ 真实重启后 phase2 | **58 / 58**、**9 / 9**（见 E5） |
| H1 隐私探针 `replay-privacy-fixed.mjs`（18082） | 18 个布尔项全部为 true |
| 浏览器 M1–M5 + EDIT-A/B | **25 / 25**（见 E2） |
| 冻结安装 | 未重跑：与快照 `refs/snapshots/pre-devin-e-20260925` 比对，`package.json`、`pnpm-lock.yaml`、workspace 均无变化，沿用 handover 中 107 packages 的结论 |

18082 后端未重启：服务端源码与二进制自上轮 phase2 验证后未变，本轮持久化验证在 18083 上完成。

## E7 结论与剩余项

- **已闭环**：H1–H3、M1–M5、编辑流程（E1）。每项都有自动化测试，外加浏览器级或探针级运行证据；H1、M1、M2 另经变异测试证明，修复回退时对应测试会变红。
- **未覆盖**（不在本轮范围）：
  - 混合客户端版本兼容（需产品决策）；
  - Tauri / Android 实机；
  - 视觉像素审查；
  - 主机或数据库崩溃恢复（持久化只验证了进程重启）。
- **提交状态（2026-09-25 22:30 校正）**：本轮改动已随 V1 整体提交为 `38fba28`（2026-09-25 14:46，含 E1–E6 涉及的代码、测试与本 `docs/design/`）。交接前快照 `refs/snapshots/pre-devin-e-20260925` 仍在，确认后可用 `git update-ref -d` 删除。此前本节写“全部改动仍在工作区”已失效。
- **HEAD 已前进，且仍在前进**：`38fba28` 之后全部是 V2 程序的提交。本文记录时点（22:37）HEAD 为 `4d2998f`，即 `38fba28` 之后第 25 个提交；V2 会话仍在并发提交，读到这句时 HEAD 很可能更靠后。V2 重写了本轮验证过的同一批文件（`38fba28..HEAD` 于 22:37 的行数）：`app/src/store.ts` +119、`app/src/ws.ts` +87、`app/src/ui/ChatView.tsx` +317、`server/src/api.rs` +807、`server/src/domain.rs` +279、`server/tests/it.rs` +2895，并新增迁移 0010/0011/0012/0015。**本文件 E1–E6 的所有数字只描述 `38fba28` 这棵树，未在其后的任何树上重跑**；把结论带到新 HEAD 前需按 handover §7 重做一遍。
- **Devin 执行说明**：以 `-p --permission-mode smart` 非交互运行，两次在需确认的调用处退出（curl 探测注册限流、读取数据库口令）。这两处拒绝都合理；对应工作由主会话完成，没有绕过权限。
