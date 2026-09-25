# P1-3 按会话保留草稿 + 暗色模式

先读 [../devin-rules.md](../devin-rules.md)。本任务**只改客户端**，不改服务端、不加迁移、不加依赖。

## 一、草稿（B1）

### 背景

`app/src/ui/ChatView.tsx` 切换会话时 `setText("")`（约 186 行），没发出去的文字直接丢失。

### 目标行为

1. 输入框内容按会话保存：切到别的会话再切回来，内容还在；刷新页面、重启客户端后也还在（同一账号、同一设备）。
2. 发送成功清空该会话草稿（发送时即清，失败重试走现有 outbox，不回填草稿）。
3. 会话列表：某会话有草稿、且不是当前打开的会话时，副标题显示红色 `[草稿]` + 草稿首行（截断），替代最后一条消息预览（`[有人@我]` 标记仍显示在最前）。正在输入提示优先级保持最高。
4. 编辑消息状态下（`editingMessage`），输入框里是被编辑消息的内容，**不能**当草稿保存；现有 `draftBeforeEdit` 机制保存的才是该会话的草稿。切换会话会退出编辑态（现有逻辑），此时应把 `draftBeforeEdit` 作为旧会话的草稿保存。
5. 回复目标（`replyingTo`）、待发附件不进草稿（只存文本）。
6. 切换账号不串草稿。

### 实现要点

- `app/src/store.ts`：`AppState` 加 `drafts: Record<string, string>`；提供 `setDraft(convId, text)`（空串即删除键）。
- 持久化：`localStorage` 键 `chat_drafts:<userId>`，JSON。写入防抖约 300ms；读写包 try/catch，解析失败按空处理。登录 / 启动拿到 `me` 后加载；`App.tsx` 里 `chat:logout` 的状态重置要把 `drafts` 清成 `{}`（本地存储保留，下次同账号登录还能恢复）。
- `ChatView.tsx`：输入变化时更新当前会话草稿；切换会话时从 `drafts` 读新会话的值替代 `setText("")`。注意现有的 `draftRevision`、`mutation` 等归属判定别被破坏。
- `ConvList.tsx`：预览逻辑按上面第 3 条改。样式类名 `draft-tag`。
- 把「列表预览应显示什么」抽成纯函数（输入会话、草稿、是否当前会话、typing 状态），写单测。

## 二、暗色模式（B3）

### 目标行为

1. 「我」页新增「外观」卡片：跟随系统 / 浅色 / 深色，三选一。独立组件 `app/src/ui/ThemeSetting.tsx`，在 `MePanel.tsx` 里加一行挂载（放在「连接」卡片之前）。
2. 选择存 `localStorage`（`chat_theme`：`system` | `light` | `dark`），按设备生效。
3. 启动时在 React 渲染前应用（`app/src/main.tsx` 里设置 `document.documentElement.dataset.theme`），避免先白后黑闪一下。`system` 时不设 `data-theme`，由媒体查询决定。
4. 所有界面（登录页、会话列表、聊天、气泡、菜单、弹窗、toast、管理后台、投票卡片、文件预览）在深色下都可读，对比度足够。

### 实现要点

- `styles.css` 现在 `:root` 有 11 个变量，另有约 39 处写死的颜色（`#f0f0f0`、`#f3f3f3`、`#e9e9e9`、`#d5e8d4`、`#576b95`、`#fef0d5`、`#fff7e0`、`#e8f5ec`、各种 `rgba(0,0,0,..)` 阴影等）。把它们收编成语义变量，例如 `--hover`、`--selected`、`--avatar-bg`、`--avatar-fg`、`--link`、`--mention-bg`、`--mention-border`、`--warn-bg`、`--warn-fg`、`--chip-me-bg`、`--overlay`、`--shadow`、`--input-bg`、`--code-bg`、`--code-fg`。
  - **必须定义 `--link`、`--code-bg`、`--code-fg`**：同阶段 P1-4（链接 / 代码块渲染）会使用这三个变量，并带回退值。
- 结构：

  ```css
  :root { /* 浅色值 */ color-scheme: light; }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) { /* 深色值 */ color-scheme: dark; }
  }
  :root[data-theme="dark"] { /* 深色值（与上面相同） */ color-scheme: dark; }
  ```

  深色值只写一份最好（例如用同一个选择器列表 `:root[data-theme="dark"]` 与媒体查询内的选择器共用规则）；做不到就两处保持一致。
- `body`、输入框、`select`、`button` 等都要走变量；`color-scheme` 让原生滚动条和表单控件跟着变。
- 气泡：`--bubble-mine` 深色下用暗绿（如 `#2e6b3a` 一类），文字仍清晰。
- 侧栏 `--rail` 本来就是深色，深色模式下略微调整即可。
- 只改颜色，不改布局、间距、字号。
- 不要改动别的任务会碰的文件结构；新规则追加在文件末尾分节，已有规则里的写死颜色原地替换成变量。

## 测试

- Vitest：草稿预览纯函数；草稿持久化的读写（可以把 localStorage 读写封装成接收 storage 对象的函数来测，包括损坏 JSON、按用户隔离）；主题解析函数（存储值 → `data-theme` 取值）。
- 浏览器实测（真实执行，截图放 `server/target/wt/` 并在交付文档列出路径）：
  - 草稿：A 会话输入文字 → 切到 B → 切回 A 文字还在；列表中 A 显示 `[草稿]`；刷新后仍在；发送后草稿标记消失。
  - 主题：浅色、深色各截一张聊天页、登录页、管理后台；「跟随系统」下用 `page.emulateMedia({ colorScheme: "dark" })` 验证生效。

## 不做

- 草稿跨设备同步（服务端存储）。
- 自定义主题色。
