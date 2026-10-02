你是 Chat 任务 {TASK} 的执行者。当前目录是你专属的 git worktree。

1. 先完整阅读 {DOCS}/devin-rules.md（通用规则、环境、硬约束），再读你的任务单 {SPEC}（任务单开头列出的前置文档也要读），再读 CONTEXT.md。
2. 按任务单实现全部子项，先写测试再实现。
3. 亲自执行 devin-rules.md 第 4 节的全部检查，以及任务单里要求的额外检查（浏览器实测等）。
4. 写交付文档 {DOCS}/delivery/{TASK}.md，最后在终端输出子项状态表。

这是非交互运行：没有人会回答你的问题。
- .env.local 的全部变量已在你的进程环境里，直接用 $DATABASE_URL 等；不要读取、打印或 source .env.local，不要单独探测数据库连接。
- 浏览器实测只用无头 Playwright；禁止 cua-driver、computer-use 或任何操作真实桌面的工具。
- 后台起的服务用完就关掉。
- 遇到被拒绝的命令，换一种被允许的方式完成，或记进交付文档后继续，不要停下等待。不要 git commit。
