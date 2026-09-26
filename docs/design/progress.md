# Chat 任务总览与当前进度

**整理日期：2026-09-25（Asia/Shanghai）**

**原始会话：`codex://threads/01a0d148-af62-7e53-bab9-b959b7d3bbf2`**

**工作区：`C:\Users\xie_f\projects\other\chat`；分支：`main`；V1 全部实现已提交为 `38fba28`（2026-09-25 14:46）。其后是 V2 程序的提交，本文时点（22:40）HEAD 为 `4d2998f`，V2 仍在同一 checkout 并发提交。V1/V2 均未推送、未部署。**

## 1. 当前结论与文档入口

消息生命周期、会话偏好、群投票均已有实现和 A/B/C 阶段报告；**2026-09-25 集成验收结论为 NEEDS_CHANGES，随后按用户要求执行修复，修复主体已落地、通过自动化与浏览器级复验，并已提交为 `38fba28`（14:46）；这不等于 V1 发布通过——结论尚未在 HEAD `cb34787`（含 24 个 V2 提交）上重跑**。验收时的 Rust 35/35、前端 12/12、E2E 58/58 与重启后 4/4 已被修复轮最终一轮取代：it 43 + migrations 1、Vitest 36/36、E2E 58 + 真实重启后 phase2 9、浏览器级 M1–M5 + 编辑两场景 25/25，详见 [delivery-E.md](delivery-E.md) §E6 与 [handover-20260925.md](handover-20260925.md) §2。

首次恢复只整理文档；随后按用户“继续验收”要求，由当前 Qoder 会话在全新隔离资源中执行测试与真实浏览器交互，未恢复原 Devin D。验收后用户要求“你修复一下”，本轮已修改业务源码并补回归，另修复了修复过程引入的前端编译中断。交付快照与复现步骤见 [delivery-D.md](delivery-D.md)，修复落地位置、证据强度与在跑资源见 [handover-20260925.md](handover-20260925.md)。

| 文档 | 内容与用途 |
|---|---|
| [chat-v1-feature-plan.md](chat-v1-feature-plan.md) | 产品目标、详细行为、数据模型、REST/WS、权限/版本/恢复、分工与验收；第 9 节区分实现与设计差距 |
| 本文 `progress.md` | 总目标、上轮修复范围、实际阶段进度、测试证据、剩余问题和恢复顺序 |
| [delivery-A.md](delivery-A.md) | A 消息生命周期阶段快照，含完整 MessageView 反序修复 |
| [delivery-B.md](delivery-B.md) | B 置顶/静音/归档阶段快照，含本人多设备同步与应用内通知 |
| [delivery-C.md](delivery-C.md) | C2 群投票阶段快照，含匿名个性化、替换投票与最小 UI |
| [delivery-D.md](delivery-D.md) | 集成验收当次的快照：已复现问题、命令结果、浏览器覆盖与清理；§7 记录同日修复进度 |
| [handover-20260925.md](handover-20260925.md) | 修复与复验交接：本轮实测数字、各缺陷落地位置与证据强度、在跑隔离资源、从零复验命令 |
| [task-C.md](task-C.md) | C2 的历史执行单；说明从独立投票帧收敛为 REST + 现有 WS 广播的决策 |

A/B/C 文档中“下一阶段尚未实现”等表述只代表当时的阶段边界；验收结论以 delivery-D 为准，修复后的当前实测以 handover-20260925 为准。该报告不冒充原 Devin D 完成，也不把验收执行结束或“测试全绿”等同于功能验收通过。

## 2. 任务来源、总目标与范围

### 2.1 已确认的任务链

1. 找回上一轮 review 记录，整理全部 P1/P2 问题，通过 Paseo 交给 Devin 修复。
2. 在保留现有修复的基础上，对照成熟聊天产品确定下一轮功能方向。
3. 用户同意制定详细设计并拆分给多个 Devin；采用**同一工作区、分阶段串行写入**，不是多人并发改同一 checkout。
4. 恢复原会话最后要求：**将任务目标、设计以及当前进度全部整理为文档**，已完成。
5. 用户随后要求**继续验收**；已执行当前工作区的集成验证，结论不通过，保留缺陷清单。
6. 用户要求**“你修复一下”**；H1–H3、M1–M5 及第 2 节高优先级并发项已在工作区落地修复并补回归，未自动扩展为提交、推送或部署。

### 2.2 产品目标

定位为熟人、小群体和小团队可长期使用的可靠聊天工具。先补消息交互与轻量协作，不做所有竞品功能的集合。

| 层次 | 本轮目标 | 当前边界 |
|---|---|---|
| 可靠性基线 | 保留已有鉴权、上传、群生命周期、管理权限、多设备同步 P1/P2 修复 | 附件升级已改为保留引用（0005 + 迁移夹具）；撤销/刷新并发有新增测试但属 agent 自写自测 |
| A 消息生命周期 | 回复/引用、作者限时编辑、软删除、reactions | 引用墓碑与预览版本反序已在 store 修复并有单测；浏览器交错序列未重跑 |
| B 个人会话偏好 | 置顶、定时/永久静音、归档、本人多设备同步 | 迟到偏好回退已按 `updated_at` 修复并有单测；通知仍仅应用内 toast |
| C 群协作 | 单/多选、匿名/实名、截止、撤票、关票 | 创建重放泄露已修（`Posted.inserted`）并有运行时探针；到期计时与丢响应重试为代码/单测级证据 |
| D 集成交付 | 协议、权限、锁文件、迁移、回归和真实浏览器检查 | 验收结论 NEEDS_CHANGES 后已修复并自动化复验，剩余是浏览器级复验与提交决策 |

后续候选但不纳入当前验收：消息收藏、稳定深链接、全局通知中心/Android 推送、高级搜索、threads/topics、群角色扩展、邀请链接、语音消息、定时发送。E2EE、音视频通话、公共频道、Bot/插件、AI 总结、复杂工作流需单独立项。

## 3. 上一轮 P1/P2 修复基线

原 review 记录来自 Paseo 会话 `b59b780c-ab59-4282-8a07-6d726cf735b3`；原始 probe 位于 `server/target/review-20260924-a/`。这些是历史位置，运行产物不视作稳定归档；其中状态 JSON 可能包含账号和 token，不复制进文档或提交。

历史修复执行者：`22942a2c-8f44-482e-8ec8-8fda0ac5e635`。以下是**历史任务清单与报告摘要，不是本次重新验证结论**。

| 编号 | 历史修复目标 | 历史方案 / 当前复验重点 |
|---|---|---|
| P1-01 | 附件 Stored XSS | 安全 MIME 预览白名单、默认下载、`nosniff`；保留图片/音视频/PDF，需复验有效载荷与预览 |
| P1-02 | 禁用/强制登出后旧 WS 仍可读写 | 服务端主动关闭连接，消息/sync/read/typing 重新校验授权 |
| P1-03 | 密码重置未撤销旧 access token | `users.session_version`，撤销旧 token/ticket/连接；权限变更路径亦需核对 |
| P1-04 | 非法 refresh 请求耗尽连接池 | 避免事务内诊断再次借池；并发无效请求不拖住正常 API |
| P1-05 | multipart 默认大小与上传配置不一致 | body limit 对齐配置并保留流式校验；原验收要求 1/3/25 MiB 及超限 |
| P1-06 | 转让/退群竞态产生零群主 | 会话行锁和锁内重检；测试验证唯一 owner 不变量，不写死竞态赢家 |
| P1-07 | 解散/邀请竞态复活会话 | 同锁检查 dissolved 状态，解散后不可新增成员或发送消息 |
| P1-08 | 管理员变更导致零可用管理员 | advisory lock、可用管理员计数、禁止 disabled 用户升 admin |
| P1-09 | 客户端将最大 seq 当连续水位 | 连续水位与缺口补拉；需实际断网/乱序复验 |
| P1-10 | 登录/登出异步结果污染新账号 | session generation；新增 UI 回调和交互状态仍有待检路径 |
| P1-11 | 实时缓存短路历史加载/历史覆盖实时消息 | `historyLoaded` 与按 id 合并；搜索、历史、ACK 等路径一起复验 |
| P2-12 | 中文好友验证消息按字节截断 panic | 字符安全截断 |
| P2-13 | 附件唯一索引无法兼容历史重复引用 | 0005 已改为为每条重复引用复制附件元数据并共享 storage_path；`server/tests/migrations.rs` 用旧版夹具先复现 3→1 丢失再验证保留 |
| P2-14 | WS sync 500 条无续页 | `has_more` 分页，客户端连续补拉 |
| P2-15 | 旧 read_sync 清除新未读 | 已有单调已读判断，但较大且落后于本地消息的 read_sync 仍直接清零；D 必检 |
| P2-16 | 删除好友后保留 DM 泄露 presence | 按好友/共群关系过滤可见性 |
| P2-17 | 拒绝好友申请后同方向重复提交 | 服务端限制同方向重申 |
| P2-18 | stopWs 清 timer 未置空导致不再重连 | 清 timer 并复位重连状态 |
| P2-19 | after_seq=0 被 truthy 条件忽略 | 显式判断参数是否存在 |
| P2-20 | Tauri CSP 阻止安全附件预览 | 补图片/媒体/iframe 规则；Tauri 运行时仍待验证 |

历史主会话记录：服务端 18/18，隔离 E2E phase1 58/58、phase2 4/4；原要求基线为 57/57，不能将不同时间点的数字当成当前验收数。主会话曾记录本地 Vite 独立复跑遇权限错误，而 Devin 报告构建通过，两类证据保留区别。

## 4. V1 技术设计摘要与实际落点

详细约束见 [设计文档](chat-v1-feature-plan.md)。当前分层是 Rust REST/WS → domain 事务 → PostgreSQL 表及 `message_views` → 个性化 `MessageView` → React store/UI；桌面壳另受 Tauri CSP 限制。

### 4.1 消息生命周期

- 回复通过 `reply_to_id` 指向同会话消息，预览只展开一层，墓碑保留 id/seq 便于定位。
- 编辑仅作者的普通文本、服务端 15 分钟窗口；要求 `expected_version`，冲突返回 409。
- 软删除保留 id/seq，掩蔽正文/附件/提及/投票；本人或群主可删除，删除后附件下载失效；物理文件只清理无其他 attachment 行引用的路径。
- reaction 按 `(message_id, user_id, emoji)` 唯一，实际变更才增加消息版本；每人每消息最多 20 种，字符串最多 32 字节。
- `message_updated/message_deleted/reaction_update` 均携带完整 MessageView。不能用 partial reaction 帧先提升全局版本，再把迟到正文更新丢弃。
- UI 用 REST；服务端也支持 WS 编辑/删除/reaction。mutation 回执 `client_op_id` 不得消费发送 outbox 的 `client_msg_id`。

### 4.2 会话偏好与通知

- `conversation_preferences` 以会话/用户为主键，置顶、归档、定时/永久静音互不跨用户传播。
- GET 会话列表附带 `preference`；PUT 仅更改提供的字段；`preference_updated` 只给本人的所有连接。
- 置顶按 pinned_at 倒序；普通新消息不解档，打开归档会话或新消息 @本人会解档。
- 静音只抑制应用内 toast；未读照计，@本人可突破静音。toast 最多四条、五秒消失、点击打开会话。
- 没有 OS/Android 推送、浏览器通知权限申请或通知历史中心。

### 4.3 群投票

- `polls/poll_options/poll_votes` 挂在 `kind='poll'` 消息；问题 1–200 字符、2–10 个不同选项、每项 1–100 字符，选项归属用复合外键兜底。
- 创建走 `POST /v1/conversations/{id}/polls`；替换选择走 `PUT /v1/messages/{id}/poll/votes`；空集合撤票；关票走 `POST /v1/messages/{id}/poll/close`。
- 仅群成员可创建/投票，创建者或群主可关；关闭、过期、墓碑后拒绝投票，结果仍可读取（墓碑除外）。
- 创建复用 `message`，投票/关闭复用 `message_updated`；没有独立投票 WS 入站指令或 poll_updated 等帧。
- 实际投票变更同时升 poll.version 与消息 edit_version，seq 不变；反序保护以完整消息 edit_version 为准，不要求两个计数相等。
- 匿名视图应不带其他 voter 身份，`my_option_ids` 必须按接收者计算；验收时复现的 REST/WS 创建幂等重放错误广播创建者选择，已改为仅真实插入才广播（`Posted.inserted`），由 `replay-privacy-fixed.mjs` 探针与 `poll_creation_replay_is_private_over_rest_and_ws` 覆盖。
- 最小 UI 已有动态选项、单/多选、匿名、截止时间、点击即投/撤票、关闭确认；没有改题/改选项/重开投票功能。

### 4.4 恢复与迁移

- seq 仅表达新消息顺序，编辑/删除/reaction/投票不推进 seq。
- 重连刷新目录，按连续水位补新消息，并每批至多 100 个缓存 id 回读旧消息变化。无权限时清缓存，不新增通用事件日志。
- 0007 消息生命周期 → 0008 会话偏好 → 0009 投票；0005/0006 属于此前基线。用户确认 0005–0009 仅在本地测试、尚未部署，用户确认 0005–0009 仅在本地测试、尚未部署，因此本轮直接修正了未发布的 0005（重复附件引用改为复制元数据并共享 `storage_path`），未伪造已应用迁移的校验和，也未重建或删除任何现有业务库。今后不得再修改已在真实库执行过的迁移。
- 幂等分层：发送唯一键、编辑最近一次 edit_op_id、其余多为状态幂等；当前不是完整持久化 mutation 队列。

## 5. 多 Devin 阶段进度

下表中的交付/idle 是原会话及阶段报告记录，不代表今天已重新查询每个 agent 的运行状态。

| 阶段 | Paseo agent ID | 进度与交付 |
|---|---|---|
| A 消息生命周期 | `9c7e0051-70b0-4d7b-b238-990439bbf7a4` | 2026-09-24 已阶段交付；[delivery-A.md](delivery-A.md)；26 个 Rust / 5 个前端测试的历史结果 |
| B 会话偏好 | `49bba347-e15f-4021-9313-6939e0490ccd` | 2026-09-24 已阶段交付；[delivery-B.md](delivery-B.md)；29 / 9 的历史结果 |
| C 初次投票执行 | `583d8774-9deb-4a16-b140-e4c1069da12e` | 原会话记录停止并由 C2 替代，不作为最终交付者 |
| C2 最小投票闭环 | `ad375fa2-30e8-4ffc-b0a1-810892b47d5a` | 2026-09-24 已阶段交付；[delivery-C.md](delivery-C.md)；35 / 12 的历史结果 |
| 原 Devin D | `73c40a92-34bd-4334-b22a-bb4a5c6da47f` | 2026-09-24 16:04 启动；当前 daemon 不可连接，原 agent 完成状态未知 |
| 本轮集成验收 | 当前 Qoder 会话 | 2026-09-25 已执行，见 [delivery-D.md](delivery-D.md)；结论 NEEDS_CHANGES，复现 H1–H3、M1–M5 |
| 本轮修复与复验 | 当前 Qoder 会话 + 两个因额度中断的子 agent | 2026-09-25 已执行，见 [handover-20260925.md](handover-20260925.md)；H1–H3、M1–M5 落地并自动化复验通过，另修回子 agent 中断留下的前端编译中断 |

### D 当前状态核对

2026-09-25 只读调用已安装的 Paseo CLI：

```powershell
& 'C:\Program Files\Paseo\resources\bin\paseo.cmd' inspect 73c40a92-34bd-4334-b22a-bb4a5c6da47f --json
```

返回 `DAEMON_NOT_RUNNING`。这只证明当前无法连接本地 Paseo daemon，**不能推断 D 已完成、已失败或正在继续执行**。本次未启动/重启 daemon，未批准其历史未知权限请求，未向 D 发送新消息。

### 关键过程记录

- 上轮修复时曾整体重写 `app/src/ws.ts`，原会话记录随后从快照恢复并增量修改；继续工作不得再以覆盖文件方式丢弃 outbox/catalog 等既有内容。
- A 的竞态测试曾固定断言 owner 的具体赢家，已在该阶段改成唯一 owner 与 membership 一致性不变量。
- A 曾出现 reaction(v2) 先于编辑(v1) 时正文丢失的问题，后续统一为完整 MessageView，并补服务端帧测试及前端反序 reducer 测试。
- C 初次执行被停止，C2 按更窄执行单完成实现；投票使用 REST + 现有 WS 广播是明确决策，不是缺少设计中的独立 poll 帧。
- 原 `progress.md` 停留在 B 执行中、C/D 未启动，已由本文纠正。

## 6. 验证证据与复验入口

### 6.1 历史报告与本轮实测

各阶段数字是**当时累计测试数，不可相加**；历史报告与当前工作区实测明确分列。

| 时间点/来源 | Rust | 前端测试 | 构建与其他记录 |
|---|---|---|---|
| 上轮修复，原 Codex 会话 | 18/18 | 没有完整客户端竞态验收证据 | Devin 报告 build 通过；隔离 E2E 58/58 + 4/4 |
| A，[delivery-A.md](delivery-A.md) §3 | `cargo test --locked --test it` 26/26 | Vitest 5/5 | fmt、check --locked --all-targets、npm build 报告通过 |
| B，[delivery-B.md](delivery-B.md) §3 | `cargo test --manifest-path server/Cargo.toml` 29/29 | Vitest 9/9 | fmt、cargo check、npm build 报告通过 |
| C2，[delivery-C.md](delivery-C.md) §3 | `cargo test` 35/35 | Vitest 12/12 | fmt、cargo check、npm build 报告通过 |
| 本轮验收，[delivery-D.md](delivery-D.md) §3 | `cargo test --locked` 35/35 | Vitest 12/12 | Rust/前端编译通过，E2E 58/58，真实后端重启后 4/4；2 附件字节/标识符保留 |
| 本轮修复复验，[handover-20260925.md](handover-20260925.md) §2 | `cargo test --tests` 42/42 + `--test migrations` 1/1 | Vitest 33/33（6 文件，3.2.7） | `npm run build` 首次失败（ChatView 半改状态 5 个 TS 错误）→ 修复后通过；E2E 48/48，真实重启后 phase2 9/9 含附件 SHA-256；干净目录冻结安装 107 packages；H1 探针 17 项断言全真；`git diff --check` 通过 |

**通过正常路径不等于验收通过，套件全绿也不等于可发布**：本轮就是靠 `npm run build` 才抓到所有测试套件都报绿的编译中断。验收阶段的问题**（frozen lock、附件升级保留、匿名重放个性化失败，store 定向探针 8 个预期失败，浏览器复现引用残留/过期按钮/丢响应重复创建）已在修复轮处理后不再复现，但**浏览器级 M1–M3 原始操作序列尚未重跑**，该缺口保留在 handover §5。完整命令和证据分见 delivery-D 与 handover，不将定向探针与既有套件混记。

### 6.2 修复后的复验矩阵

以下保留验收命令入口；验收阶段已执行与未覆盖的范围以 delivery-D §3–4 为准，修复轮结果以 handover §2、§7 的从零命令矩阵为准。Tauri、窄屏/键盘、完整多时区、混合客户端版本及精确并发时序窗口仍未覆盖。

`git diff --check` 此前检出的 `app/src/styles.css:503` EOF 多余空行已在收尾时最小修正，现在通过（D-12 关闭）。

| 验收项 | 命令/场景 | 完成标准 |
|---|---|---|
| Rust 格式 | `cargo fmt --manifest-path server/Cargo.toml --check` | 无格式差异 |
| Rust 编译 | `cargo check --manifest-path server/Cargo.toml --locked --all-targets` | 所有 target 通过 |
| Rust 回归 | `cargo test --manifest-path server/Cargo.toml --locked` | 基线与新增回归全过，记录数量和失败项 |
| 前端单测 | `pnpm -F app test` | reducer 及补充竞态回归全过 |
| 前端类型/构建 | `pnpm -F app exec tsc --noEmit`；`pnpm build` | 类型与 Vite 构建通过；Vitest 用 esbuild 只剥类型，**未跑 build 不得声称前端可用** |
| 依赖复现 | 仅拷贝 manifest/workspace/lock 到空目录后 `pnpm install --frozen-lockfile --offline` | 干净目录可安装；把现有 `node_modules` 一起拷进去会得到“Already up to date”的假绿 |
| 迁移 | 空库 0001→0009；`cargo test --test migrations` 的旧版本夹具升级；历史重复附件数据 | 除“迁移能执行”外必须断言**数据保持性**（每条历史消息仍可访问附件、删除单条引用不回收共享文件） |
| 既有 E2E | 隔离 BASE_URL 下运行 `node scripts/e2e.mjs` 与 `node scripts/e2e.mjs phase2` | 全部断言通过；phase2 本身不重启服务，持久化验证需单独安排真实重启 |
| 新 UI | 启动隔离后端与前端，用浏览器实际操作 A/B/C；需要时检查 Tauri | 正常路径、权限失败、双设备、断网、窄屏/键盘、截止/时区；检查 console/network |
| 变更完整性 | `git diff --check`、核对未跟踪文件 | 无空白错误，无凭据/临时状态误入交付 |

根目录 `pnpm test` 只执行后端 Cargo 测试，不包含前端 Vitest。现有 `ui-smoke.mjs`/`rt-smoke.mjs` 主要记录观察，不能只按退出码 0 认定业务通过；它们使用固定浏览器路径/调试端口/临时 profile，使用前要检查是否与现有实例冲突。

### 6.3 测试环境边界

- 不连接默认 `:8080` 实例做写入测试，不启动第二个聊天进程共用现有业务数据库。
- 当前 `server/tests/it.rs:14-51` 会从 `DATABASE_URL` 的服务器/连接信息派生固定库名 `chat_test`，必要时自动建库并执行迁移；**仅换 DATABASE_URL 中的库名并不能获得随机独立测试库**。先确认该 PostgreSQL 实例和 chat_test 专供本次测试。
- 集成测试使用临时监听端口，但不等于数据库也完全隔离；不得 DROP/重建已有库来绕过失败。
- `scripts/e2e.mjs` 会注册账号、发送消息、上传文件、操作群，并写出含凭据/token 的状态文件；必须使用隔离部署，不把原始状态或未脱敏日志写进报告。
- 验收轮使用新建容器、55432 数据库端口、18081 后端和专用上传目录，收尾后已全部停止并移除。修复轮资源**交接时仍在运行**：容器 `chat-fixes-20260925-6825`（`127.0.0.1:55433`，tmpfs）、后端 18082、前端 dev 15173，凭据仅存被忽略的 `server/target/fixes-20260925/runtime.json`（0600）；拆毁命令见 handover §6，不要假设它们会一直活着，也不要顺手动其他容器或用户库。

## 7. 集成待办与已发现的设计缺口

以下保留首次整理时的 D 编号供交接。修复轮后 **D-01/02/03/06/11/12 已关闭并有对应回归**；D-04/05/07/08/09 代码已落地并有单元/集成级证据，但**浏览器级交错序列未重跑**；D-10 的 Tauri、窄屏/键盘、多时区、混合版本仍是缺口。运行时证据与落地位置分见 delivery-D 与 handover §4。

D-04 的 ACK 回退从未被证明（`resolveAck` 走 `upsertMessage`/`selectMessage`，本来就带版本判断），不能与已复现的目录/实时预览回退混为一谈；D-09 单靠登出字段未清也不能证明串号，确认需处理的是在途响应的归属。新增在线引用残留、撤销竞态等见 delivery-D，而非仅限原十二项。

| ID | 修复轮后的现状 | 剩余动作 |
|---|---|---|
| D-01 | 已关闭：`pnpm-lock.yaml` 收录 Vitest；A 阶段遗留的 `app/package-lock.json` 可逆移入忽略目录，只保留 pnpm 一套交付锁文件 | 若换机器，重跑 handover §7 的干净目录冻结安装 |
| D-02 | 已关闭：0005 改为复制附件元数据并共享 `storage_path`；`server/tests/migrations.rs` 旧夹具先红后绿 | 真实部署前核实目标库已应用的迁移版本，不改已在生产执行的迁移 |
| D-03 | 已关闭：幂等命中传 `inserted=false`，REST/WS 创建重放只回 ACK 给发起者，广播视图逐接收者 `view_by_id(viewer)` | 探针**已复跑通过**（delivery-E §E6：`replay-privacy-fixed.mjs` 18 项布尔全真）；剩余的是在 HEAD 上重跑 |
| D-04 | 已改：`store.ts` 的 `selectMessage`/`selectLast` 按 `edit_version` 取舍，`resolveAck` 亦经 `upsertMessage` 走同一版本判断；当时就未证明 ACK 会回退 | 浏览器交错序列**已重跑并通过**（delivery-E §E2 M1，编辑与删除两条陈旧目录响应）；剩余的是在 HEAD 上重跑 |
| D-05 | 已改：`app/src/ws.ts` read_sync 先按 `readSeq` 单调过滤，再把未读夹到 `max(0, last_seq - read_seq)`，并在校验剩余条数不足时回查目录 | 补“本地 seq=12、收到 read_sync(seq=10)”的定向用例；断网/多设备实测仍未做 |
| D-06 | 已关闭：`selectPreference` 按 `updated_at` 新旧判定；服务端 `domain.rs:1679` 用 `GREATEST(clock_timestamp(), 旧值 + 1µs)` 单调推进 `updated_at`，只提供可靠排序、不做新旧拒绝 | 浏览器交错**已重跑并通过**（delivery-E §E2 M2，含服务端 pref 回读）；双设备端到端乱序仍未做 |
| D-07 | `PollCard.tsx` 已有 `schedulePollDeadline` 本地计时 + 到期重算 + 冲突回读，并有新单测 | 跨截止时刻实测**已补**（delivery-E §E2 M4：15 秒截止、无刷新下 `disabled=2` + “已截止”） |
| D-08 | 已改：`NewPollModal`/`ChatView`/`PollCard` 以 `owner` token + `session()` 归属校验包裹回调，创建在内容未变时复用同一 `client_msg_id` 重试 | 故障注入实测**已补**（delivery-E §E2 M5：abort 首个创建响应后重试得 `ui=1 server=1 seqs=1`） |
| D-09 | 已改：`ChatView`/`PollCard`/`NewPollModal` 的回调以组件实例 token + `session()` 代次双重归属校验，`api.ts` 在响应处也校验代次 | `api.test.ts` 已覆盖旧代次响应拒绝（delivery-E §E3 判定回退会变红）；A→B 切换下的浏览器级在途实测仍未做 |
| D-10 | 浏览器级 M1–M5 与编辑两场景**已完成**（delivery-E §E2，25/25）；Tauri、窄屏/键盘、多时区、混合客户端版本仍未覆盖 | 剩余项按 handover §5 第 4、8、9 项逐项补齐，未实测前不得改判为通过 |
| D-11 | 已对齐 `CONTEXT.md` 中过时的“不可编辑、不可撤回”口径；阶段报告保留为历史快照 | 后续按文档维护流程继续校正 |
| D-12 | 已关闭：`styles.css` EOF 多余空行去除，`git diff --check` 通过 | 收尾前重跑一次即可 |

匿名普通投票/关闭路径本就采用按接收者视图；D-03 特别指创建重放路径，不应泛化为“所有匿名投票都会泄露”。修复轮确实修改了业务源码（含未发布的 0005 迁移与前端 store/UI），这些改动已随 `38fba28` 进入版本库；仍**未推送、未部署**。

## 8. 交付清单与提交状态

2026-09-25 22:40 复核：本节原先记的“`git status --short` 显示大量未提交内容”与“`docs/design/`、迁移仍未跟踪”两条都已失效——V1 的全部实现（前几轮 P1/P2 之外的 A/B/C 实现、本轮修复对服务端/store/投票 UI/`ChatView.tsx` 的改动、0005–0009 迁移、`server/tests/migrations.rs`、Vitest 用例、测试与整个 `docs/design/`）已提交为 `38fba28`（14:46），逐项 `git cat-file -e 38fba28:<path>` 均可取到。本文时点工作区只剩 3 个已跟踪文档的修改加 2 项未跟踪：`docs/design/{delivery-E,handover-20260925,progress}.md`（本会话这三处校订，按“不自动 commit/push”留在工作区）、`.idea/`、`docs/design/v2/tasks/P3-1-search-files.md`（后者属 V2 程序在飞内容，本会话未动、未暂存、未提交）。原先列出的 `scripts/v2/union.mjs`、`scripts/v2/append-section.mjs`、`app/src-tauri/Cargo.toml` 已被 V2 程序自己的提交吸收，不再出现在工作区——这也说明任何“此刻工作区只有 N 项”的描述都会随并发写入者失效，引用前必须重取 `git status`。`app/package-lock.json` 已可逆移入 `server/target/fixes-20260925/package-lock.from-stage-a.json`，不再作为第二套交付锁文件；`server/target/fixes-20260925/runtime.json`（0600）内含本次测试凭据，位于忽略目录，未进版本库。

| 分类 | 主要路径 |
|---|---|
| 服务端基线及功能 | `server/src/{api,auth,domain,lib,model,state,ws}.rs`，`server/Cargo.toml`、`server/Cargo.lock` |
| 迁移 | `server/migrations/0005_sync_invariants.sql` 至 `0009_polls.sql`（0005 为本轮就地修正的未发布迁移；`38fba28` 之后 V2 又新增了 0010/0011/0012/0015，不属 V1 交付） |
| 前端实现 | `app/src/{App,api,store,types,ws,styles}` 对应源文件，`app/src/ui/{ChatScreen,ChatView,ContactsPanel,ConvList}.tsx` |
| 新 UI/测试 | `app/src/ui/NewPollModal.tsx`、`app/src/ui/PollCard.tsx`；Vitest 用例 `app/src/{store,api,ws}.test.ts`、`app/src/ui/{ChatView,NewPollModal}.test.ts`、`app/src/ui/PollCard.test.tsx` |
| 配置和验收 | `app/package.json`、`app/src-tauri/tauri.conf.json`、`server/tests/it.rs`、`scripts/e2e.mjs` |
| 待处理依赖文件 | 无：根 `pnpm-lock.yaml` 已收录 Vitest，A 阶段遗留的 `app/package-lock.json` 移入 `server/target/fixes-20260925/package-lock.from-stage-a.json` |
| 新增测试 | `server/tests/migrations.rs`（升级数据保留夹具） |
| 文档 | `docs/design/` 已全部跟踪，包含设计、进度、A/B/C 报告、集成验收 delivery-D/E、修复交接 handover-20260925 和 C2 执行单；V2 程序另用 `docs/design/v2/**`，与 V1 分属不同树 |
| 保留用户内容 | `.idea/` 等已有未跟踪文件，本次不清理 |

上表按路径而非按“是否 tracked”列出交付范围：本轮之前 `docs/design/` 与迁移一度处于未跟踪状态，只记 tracked diff 会漏掉它们。不从 Git diff 单独推断每段代码的作者；多个阶段共享工作区。

## 9. 下一步恢复执行顺序

1. **确认写入者**：本 checkout 现在有**两个并发写入者**——本会话（V1 修复与文档）与 V2 程序。V2 在本轮编辑文档期间就提交了 `4d2998f`（22:37），并已把自身在飞文件收进提交。任何一步开始前先重取 `git status`/`git log`，不要把 HEAD 上的新提交误认为 V1 的回退。
2. **保护现状**：不能 reset、覆盖未知内容、删除用户产物、自动 commit/push。本会话的三处文档校订有意留在工作区，由用户决定是否提交。
3. **浏览器级复验已完成，无需重跑**：delivery-E §E2 已用无头 Playwright 跑完 handover §5 的 M1–M5 与编辑两场景（25/25，含交错目录帧、迟到偏好、编辑→删除→看引用、跨截止时间、丢响应重试），§E3 给出逐项判定。**剩下的缺口不是“没测”，而是“测的不是最新的树”**：见 handover §5 第 9 项。
4. **agent 自写测试审查已完成**：delivery-E §E3 已按断言强度逐文件审过撤销/刷新并发与 `server/tests/it.rs` 竞态用例，并把结论落到 §E2 的红/绿证据；若之后 V2 改写了这些用例，需按同一方式重审。
5. **提交决策已定**：V1 全部实现与文档已于 14:46 提交为 `38fba28`（该提交内含 0005 迁移修正与 `docs/design/`，不含 `.idea/`、`server/target/` 内的凭据与探针产物）。之后是推送与部署，本会话两者都不做。
6. **更新真实证据**：任何新数字写进 delivery-D/E 或 handover 时附命令、执行时点与它所属的 commit；未经实测的项目继续明确标注，不能因旧套件全绿便改成“全部通过”。

修复轮结束：H1–H3、M1–M5 已落地并通过自动化与浏览器级复验，另修回一个由修复引入的前端编译中断；结论**已随 `38fba28` 进入版本库，仍未推送、未部署**。当前挡在“V1 发布通过”前的唯一缺口，是这些证据没有在 `38fba28` 之后的任何一棵树上重跑过——V2 已重写 `store.ts`、`ws.ts`、`ChatView.tsx`、`api.rs`、`domain.rs`、`it.rs` 并新增 4 个迁移，grep 只能说明 V1 的修复仍然存在，不能替代复验。
