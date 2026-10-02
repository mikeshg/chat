# P4-2 E2E 覆盖 V2 功能 + README / CONTEXT 更新

先读 [../devin-rules.md](../devin-rules.md)。迁移号：不需要。**本任务不改业务代码**（`server/src`、`app/src` 都不动；发现 bug 记进交付文档，不修）。

## 背景

`scripts/e2e.mjs` 是对着运行中的部署跑的真实用户旅程（phase1 完整旅程、phase2 在后端真实重启后验证持久化），目前只覆盖 V1 功能。V2 做完了第 1–3 阶段的 14 个功能，需要把主路径加进去；README 的功能列表和 `CONTEXT.md` 的领域词汇也还停在 V1。

同阶段 P4-1 在做备份 / 恢复，本任务**不覆盖备份恢复**（它会清空部署数据，不适合放进通用 E2E）。

## 一、E2E 扩展

在 `scripts/e2e.mjs` 的 phase1 里按 V2 功能分节追加（风格、`ok()` 断言、`api()`、`Ws` 辅助都沿用现有写法；每节开头打印 `== V2 <功能> ==`）：

| 功能 | 断言要点 |
|---|---|
| 注册开关 / 邀请码 | 仅在提供了 `E2E_ADMIN_USER/PASS` 时执行：切到 `invite` → 无码注册 403 → 生成 1 次邀请码注册成功 → 再用 403 → **最后切回原来的模式**（先读出原值） |
| 登录设备 / 改密 | 同一用户两次登录 → `/users/me/sessions` 两条；移出其中一条后它的 token 401；改密后另一台设备 token 401、当前设备新 token 可用、新密码能登录 |
| 头像 | 上传一张最小 PNG → `avatar_version` +1、`GET /users/{id}/avatar?v=` 200；伪装类型 400 |
| 缩略图 | 用 node 生成一张 > 480px 的 PNG（可用 `zlib` 手写最小 PNG 编码，或放一张测试图片到 `scripts/fixtures/`）上传 → `has_thumb=true`、`?thumb=1` 返回 `image/jpeg` |
| 语音 | 上传 `audio/webm` + `duration_ms` 并经 WS 发送 → `kind=audio`、正文 `[语音]` |
| 文件传输助手 / 转发 | 新用户有且只有一个 `kind=self` 会话；把一条消息转发到它 → `forwarded_from` 正确；同一 `client_op_id` 重试不重复 |
| 全局搜索 | 两个会话发含同一关键词的消息 → `/v1/search?q=` 两条；删掉一条后只剩一条 |
| 公告 / 置顶 | 群主设公告 → 成员会话列表里有 `announcement`；成员置顶一条消息 → `/pins` 返回它 |
| 会议地址 | `/v1/server-info` 有 `meeting_base_url` |
| 推送 | `/v1/users/me/push` 能读出 `available` 字段（不要求配置 ntfy） |

phase2（重启后）追加：头像、`self` 会话、公告、置顶在重启后仍然存在。把需要的 id 写进现有的 `.e2e-state.json` 状态结构。

**E2E 的前提**：全新部署里第一个注册的用户会自动成为管理员，脚本里的普通用户会因此变成管理员、导致「非管理员 403」断言失败。在脚本开头加检查：如果 `E2E_ADMIN_USER` 没给、且库里还没有任何用户（注册第一个用户后发现自己是管理员），就打印清楚的提示并退出 2，而不是跑出一堆误报失败。

验证方式：在你自己的槽位后端上跑（`BASE_URL=http://127.0.0.1:$PORT`）：先注册一个管理员，`E2E_ADMIN_USER=... E2E_ADMIN_PASS=... BASE_URL=... node scripts/e2e.mjs` 全绿；用 `wt-server.mjs stop` + `start` 真实重启后端后 `node scripts/e2e.mjs phase2` 全绿。启动后端的方法见 devin-rules / 续跑提示（`node C:/Users/xie_f/projects/other/chat/scripts/devin/wt-server.mjs start`）。

## 二、文档

- `README.md`：「功能」一节补上 V2 功能（每项一行，风格与现有条目一致）；「测试」一节写明 E2E 的管理员前提和 `E2E_ADMIN_USER/PASS`；「环境变量」保持不变（V2 没有新增环境变量，可配项都在管理后台）。新增一小节「推送（ntfy）」说明：管理员在后台填 ntfy 服务器地址，用户在「我」页开启并在手机 ntfy App 订阅显示的地址。
- `CONTEXT.md`：词汇表补上 V2 引入的领域概念，每条一两句，与现有写法一致：Session（登录会话 / 设备）、RegistrationMode、Invite、ServerSettings、SelfConversation（文件传输助手）、Forward、Pin、Announcement、StorageQuota、Thumbnail、VoiceMessage、Avatar、PushTopic、ClientState（活跃连接）。边界规则补上对应的新规则（例如「公告仅群主」「任何成员可置顶，上限 20」「推送服务器只由管理员配置」「恢复备份会让所有人重新登录」——备份这条按 P4-1 任务单的规则写）。
- 不改 `docs/archive/` 下的历史文档。

## 检查

devin-rules 第 4 节的检查照做（本任务不改业务代码，cargo / vitest 应保持原样全绿）。E2E 两段都要亲自在自己的槽位后端上跑通。

## 不做

- 浏览器级 E2E（Playwright 流程固化进仓库）。
- 备份恢复的 E2E。
