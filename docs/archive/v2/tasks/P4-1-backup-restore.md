# P4-1 管理后台备份 / 恢复

先读 [../devin-rules.md](../devin-rules.md)。迁移号：不需要。**这是会清空数据库的功能，安全细节一条都不能省。**

## 背景

自部署最怕丢数据。数据分两部分：Postgres 里的全部表，和 `FILES_DIR` 下的文件（附件原文件、`*.thumb.jpg` 缩略图、`avatars/` 头像）。用户要求：在管理页面**生成备份文件、下载到本地；上传备份文件恢复**。

## 备份文件格式

`chat-backup-<UTC 时间 yyyyMMddTHHmmssZ>.tar.gz`，内容：

```
manifest.json          {"format":1,"created_at":"...","migration_version":18,
                        "tables":[{"name":"users","rows":12}, ...],"files":<数量>,"files_bytes":<字节>}
db/<表名>.csv          COPY <表> TO STDOUT WITH (FORMAT csv, HEADER true)
files/<相对路径>        FILES_DIR 下的全部文件（递归，含 avatars/），但排除 .backups/ 和恢复用的临时目录
```

- **表清单动态读取**：`information_schema.tables` 里 `public` schema 的全部 `BASE TABLE`，排除 `_sqlx_migrations`。不要写死表名（以后加表不用改备份代码）。
- `migration_version` = `_sqlx_migrations` 里成功迁移的最大 `version`。
- 数据库部分在**同一个只读、REPEATABLE READ 事务**里导出，保证各表是同一时刻的快照。
- 用 sqlx 的 `copy_out_raw` / `copy_in_raw`（`sqlx-postgres` 0.8 已提供），不要逐行 SELECT / INSERT。
- 文件边导出边可能有新上传，这部分文件可能不在快照里，接受；在代码里用 `// ponytail:` 注释写明。

## 存储位置

`FILES_DIR/.backups/`（不要在 `Config` 里加字段）。生成时先写 `*.tar.gz.part`，完成后再改名，列表只显示已完成的文件。

## 接口（全部 `auth::require_admin`）

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/admin/backups` | 开始生成备份，后台任务执行，立即返回 `202 {name}`；已有备份在生成中时返回 409 |
| `GET` | `/v1/admin/backups` | `{ in_progress: bool, items: [{name, size, created_at}] }`，按时间倒序 |
| `POST` | `/v1/admin/backups/{name}/download-ticket` | 返回 `{url}`：一次性、60 秒有效的下载地址（`<a>` 带不了 Authorization 头，沿用 WS ticket 的做法） |
| `GET` | `/v1/admin/backups/{name}/download?t=<ticket>` | 凭 ticket 流式下载（不要整个读进内存），`Content-Disposition: attachment` |
| `DELETE` | `/v1/admin/backups/{name}` | 删除备份文件 |
| `POST` | `/v1/admin/restore` | multipart：`file`（备份文件）+ `confirm`（必须等于 `RESTORE`）。路由单独放开 body 上限，**流式写入临时文件**，不要读进内存 |

- 备份名只接受 `^chat-backup-\d{8}T\d{6}Z\.tar\.gz$`，否则 400（防路径穿越）。
- 下载 ticket 用完即焚，过期 401。

## 恢复流程（按顺序，任何一步失败都要停下并如实返回错误）

1. **校验**（不改任何东西）：能解开 tar.gz；有 `manifest.json` 且 `format == 1`；`migration_version` 等于当前数据库的迁移版本（否则 400「备份版本与当前服务端不一致」）；manifest 里的表集合与当前表集合一致；所有条目路径都是相对路径、不含 `..`、不是绝对路径、不是符号链接（否则 400）。
2. **进入维护模式**：`AppState` 里加一个 `maintenance: AtomicBool`。一个路由层中间件在它为 true 时，对除了正在执行的恢复请求之外的所有 `/v1` 请求返回 503「服务维护中，请稍后」（WS 升级也拒绝）。
3. **踢下线**：给所有在线连接发 `force_logout`（reason `maintenance`），断开。
4. **恢复数据库**（单个事务）：`TRUNCATE` 全部业务表（一条语句列出全部表，`CASCADE`）；再按外键依赖的拓扑顺序（被引用的表在前，自引用忽略）逐表 `COPY <表> FROM STDIN WITH (FORMAT csv, HEADER true)`；提交。失败则回滚，文件不动，退出维护模式，返回 500 和原因。
5. **恢复文件**：解到 `FILES_DIR/.restore-new/`，全部成功后把现有文件（排除 `.backups/`）移到 `FILES_DIR/.restore-old/`，再把新文件移到位，最后删掉 `.restore-old/`。
6. **让所有人重新登录**：`UPDATE users SET session_version = session_version + 1`，并把 `sessions` 全部置为撤销、`refresh_tokens` 全部撤销。备份里的旧令牌和恢复前的令牌都不能再用。
7. **退出维护模式**，返回 `{ tables: [...], files: n }`。

同时只允许一个恢复或备份在进行（用同一把锁）。

## 客户端

新组件 `app/src/ui/BackupPanel.tsx`，在 `AdminPanel.tsx` 里加一行挂载（放在设置之后）：

- 「立即备份」按钮 → 生成中显示进度提示并轮询列表（约 2 秒一次），完成后出现在列表里。
- 列表：文件名、大小、时间；「下载」（先拿 ticket 再用 `<a href>` 触发下载，Tauri 端同样可用）、「删除」（二次确认）。
- 「从备份文件恢复」：选择文件 → 红色警告「恢复会用备份覆盖全部数据和文件，所有用户（包括你）都会被登出」→ 输入框要求键入 `RESTORE` 才能点确认 → 上传（显示上传百分比，用 `XMLHttpRequest` 的 progress 事件）→ 成功后提示「恢复完成，请重新登录」并执行登出。
- 所有异步操作有 busy / 错误状态。

## 允许新增的依赖

`server/Cargo.toml`：`tar = "0.4"`、`flate2 = "1"`、`tokio-util = { version = "0.7", features = ["io"] }`（流式下载 / 上传用）。打包 / 解包放在 `spawn_blocking` 里做。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P4-1 =====`，**全部用 `spawn_named_state` 独立库**（恢复会清空整个库），各测试的 `FILES_DIR` 也要用各自独立的临时目录：

1. 造数据：两个用户、好友、私聊消息、一个附件（记下字节和 SHA-256）、一个头像。备份 → 列表里出现、下载 ticket 能下载、manifest 表数与行数正确、tar 里有附件和头像文件。
2. 备份后改数据：发新消息、删一条消息、改昵称、删附件文件 → 上传刚才的备份恢复 → 消息、昵称回到备份时刻，附件字节与 SHA-256 一致，头像可访问。
3. 恢复后：恢复前的 access token 401、refresh 401；用原密码能重新登录。
4. `confirm` 缺失或不是 `RESTORE` → 400，数据不变。
5. 篡改 manifest 的 `migration_version` → 400「版本不一致」，数据不变。
6. tar 里放 `../evil` 或绝对路径条目 → 400，`FILES_DIR` 外没有出现新文件。
7. 非管理员调用任何接口 403；下载 ticket 重复使用 401、过期 401；非法备份名 400。
8. 维护模式：把 `maintenance` 置 true 后普通接口返回 503（直接改 state 验证中间件即可）。
9. 备份生成中再次请求 409。

Vitest：确认词校验、文件大小格式化等纯函数。

浏览器实测（真实执行，截图放 `server/target/wt/`）：管理员点「立即备份」→ 列表出现 → 下载得到文件；发一条新消息 → 上传刚才的备份并输入 RESTORE → 页面回到登录页；重新登录后那条新消息不见了。

## 不做

- 定时自动备份、备份到对象存储 / 远程、增量备份、备份加密。
- 跨迁移版本恢复（升级 / 降级转换）。

## 修订记录

### Amendment (MM1/MM5) — 2026-10-15

- `migration_version` 校验由「必须等于当前」放宽为「不比当前新」：`manifest.migration_version <=` 当前 `_sqlx_migrations` 最大成功版本即可恢复；比当前新仍拒绝。400 文案沿用「备份版本与当前服务端不一致」——注意它现在的含义是「备份版本比当前服务端新」。
- 回灌改为按 **CSV 表头驱动 COPY 列清单**：备份缺少的新列（如 0039 加的 `attachments.recalled_at`）由库默认值/NULL 补齐；CSV 含当前库不存在的列直接 400（跨版本恢复不许悄悄丢数据）。
- 原因：MM1（0039 消息撤回）给 `attachments` 加了 `recalled_at` 后，MM1 之前生成的备份应可恢复。
- 测试锁定（`tests/it/backup.rs`）：`mm5_restore_accepts_older_backup_without_recalled_at`（版本拨回 38 + 删列 → 恢复成功，`recalled_at` 为 NULL）、`mm5_restore_rejects_unknown_column`（未知列 → 400 且数据不变）、`p41_restore_rejects_version_mismatch`（加强：精确错误文案 + 全表行数/文件清单逐项不变）。
