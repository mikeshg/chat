# A19 宿主路径安全：附件落盘名 + workspace 档真实路径

来源：2026-09-28 代码审查（基于 `706dbea`）第 2、3、7 条，已由主会话核对属实。先读 [../devin-rules.md](../devin-rules.md) 和 `CONTEXT.md` 的「Agent」小节。本票**只改 `agent-host/`**，没有迁移。

## 一、附件落盘名（审查第 3、7 条）

`agent-host/src/files.rs` 的 `safe_name` + `download_inputs` 有两个问题：

1. **盘符相对路径写出 `in/`**：`safe_name("C:foo.txt")` 原样返回 `C:foo.txt`；Windows 上 `in_dir.join("C:foo.txt")` 因为「有前缀无根」会整个替换成 `C:foo.txt`，实际写到宿主进程在 C 盘的当前目录，覆盖同名文件，且不会被 `WorkDir` 清理。附件名来自服务端 `attachments.file_name`，服务端不清洗 multipart 文件名，转发会原样复制，所以群友可以构造这种名字再让主人转发给 Agent。
2. **同名附件互相覆盖**：同一次调用两个 `report.txt`，后下载的覆盖前一个，两个 `LocalFile.path` 指向同一个文件。

要求：

- 每个附件落到 `in/<附件 id>/<清洗后的文件名>`，保证同名不冲突，Agent 看到的仍是可读的原名（`LocalFile.name` 继续用于 ACP `resource_link.name`）。
- 清洗后的文件名只能是单个普通文件名：去掉路径分隔、盘符 / `:`（含 NTFS 备用数据流写法）、Windows 非法字符与控制字符、结尾的点和空格；Windows 保留设备名（`CON`、`NUL`、`COM1` 等，含带扩展名的 `nul.txt`）要改掉；清洗后为空就用附件 id。
- 最后兜底断言：拼出来的路径的父目录必须正好是 `in/<附件 id>`，否则跳过该附件并记日志。
- 测试：`C:foo.txt`、`..\..\x`、`a/b/c.txt`、`nul.txt`、`a:stream`、空名、两个同名附件（内容不同，落盘后两份都在、内容各自正确）。落盘测试放 `agent-host/tests/files.rs`，照 `download_inputs_writes_in_dir_and_skips_failures` 的本地假服务写法，别为此加依赖。

## 二、workspace 档按真实路径判断（审查第 2 条）

`agent-host/src/permission.rs` 的 `path_inside` 只做词法归一。工作目录里有 junction / 符号链接指向目录外时（pnpm、monorepo 常见），`workspace/linked/file` 被判为目录内，workspace 档自动放行编辑 / 删除 / 移动，实际写的是目录外。

背景：workspace 档是防误操作的护栏，不是沙箱（`locations` 由 Agent 自己上报），所以目标是「诚实的 Agent 经过链接写到目录外时要走审批」。

要求：

- 根目录（工作目录 + 额外目录）用 `std::fs::canonicalize` 解析；解析失败的根跳过。
- 位置路径：从原始路径（**先不做词法 `..` 归一**）找最长的已存在前缀，canonicalize 它，再接上剩余不存在的部分。剩余部分含 `..` 或任何非普通组件 → 判为不在目录内（交给主人审批）。
- 比较在 canonicalize 后的形式上做（Windows 上两边都是 `\\?\` 前缀，保持一致）；前缀边界规则不变（`C:\workspace` 不是 `C:\work` 的子路径）。
- 现有单测里用不存在的 `C:\work`、`D:\extra` 做的词法用例要改成在临时目录里建真实目录；新增：junction / 符号链接指向目录外 → 不自动放行；链接指向目录内另一处 → 放行；新文件（父目录存在）→ 按父目录判断；`dir/新目录/../x` 这类剩余部分含 `..` → 不放行。Windows 上建 junction 用 `cmd /C mklink /J`（不需要管理员），Unix 用 `std::os::unix::fs::symlink`，建不出来的平台跳过该用例并打印原因。

## 检查

本票不改 `server/`、`app/`：devin-rules §4 只需跑 `agent-host`（`cargo fmt && cargo test`）和 `app/src-tauri`（`cargo check`）两项，交付文档写明 server / app 两项「未运行：本票未改动」。

## 不做

- 服务端清洗上传文件名（另议）；MCP `send_message` 的 `file_paths` 相对路径解析基准（另议）。
