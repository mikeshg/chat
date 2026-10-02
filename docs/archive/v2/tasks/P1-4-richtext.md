# P1-4 链接 / 行内代码 / 代码块渲染 + Tauri 打开外链

先读 [../devin-rules.md](../devin-rules.md)。本任务**只改客户端**（含 Tauri 壳），不改服务端、不加迁移。

## 背景

消息正文现在由 `app/src/ui/ChatView.tsx` 的 `renderContent`（约 32 行）渲染：只把 `@username` 高亮，其余是纯文本（`white-space: pre-wrap`）。链接点不了，贴代码也难读。

## 目标行为

1. **链接**：正文里的 `http://` / `https://` 链接变成可点击链接。
   - 只识别这两个协议，`href` 永远不会是 `javascript:`、`data:` 等。
   - 链接只由 ASCII 可见字符组成，遇到空白、中文字符和中文标点立即结束（例如 `看这个https://a.com，好的` 只链接 `https://a.com`）。
   - 末尾的英文标点 `. , ; : ! ? ' "` 不算进链接；末尾的 `)` 只有在链接内括号不配对时才剥掉（`https://en.wikipedia.org/wiki/Foo_(bar)` 保留括号，`(见 https://a.com)` 剥掉）。
   - 显示文本过长（> 60 字符）时中间省略显示，`title` 显示完整地址。
   - Web：`<a href target="_blank" rel="noopener noreferrer">`。
   - Tauri：拦截点击，调用 `@tauri-apps/plugin-opener` 的 `openUrl(url)` 用系统浏览器打开（WebView 内不跳转）。
2. **行内代码**：`` `code` `` 渲染为等宽、浅底色的 `<code>`，内部不再识别链接和 @。
3. **代码块**：三个反引号围起来的多行文本渲染为 `<pre><code>`：
   - 开头 ```` ``` ```` 后面同一行的语言名只作为右上角标签显示，不做语法高亮。
   - 等宽字体、保留缩进和换行、横向滚动、不自动折行。
   - 右上角「复制」按钮，复制代码原文，成功后短暂显示「已复制」。
   - 没闭合的 ```` ``` ```` 当普通文本处理。
   - 内部不识别链接和 @。
4. **@提及**：现有高亮行为保持不变（`.mt` 类名），只在普通文本段里识别。
5. 适用范围：只作用于聊天气泡里的消息正文（`showText(m)` 为真时）。回复引用摘要、会话列表预览、搜索结果、toast 仍用纯文本。
6. 不引入 `dangerouslySetInnerHTML`，全部生成 React 元素。

## 实现要点

- 新文件 `app/src/richText.tsx`：
  - 纯函数 `tokenize(text): Token[]`，Token 类型如 `{ t: "text" | "link" | "code" | "block" | "mention", ... }`，先切代码块、再切行内代码、再在普通文本里切链接和 @。
  - 组件 `RichText({ text })` 把 Token 渲染成元素；链接点击处理放这里。
- `ChatView.tsx`：用 `<RichText text={m.content} />` 替换 `renderContent(m.content)` 的调用，删除旧的 `renderContent`（确认没有别的调用点）。
- 样式追加在 `styles.css` 末尾分节 `/* ===== V2 P1-4 ===== */`。颜色用变量并带回退：`var(--link, #576b95)`、`var(--code-bg, rgba(0,0,0,0.06))`、`var(--code-fg, inherit)`（同阶段 P1-3 暗色模式会定义这些变量）。代码块在「我」发的绿色气泡里也要可读。
- 气泡最大宽度内代码块横向滚动，不能把气泡撑破。手机宽度（375px）下不能出现页面横向滚动。

## Tauri 改动（允许新增的依赖）

- `app/src-tauri/Cargo.toml`：`tauri-plugin-opener = "2"`
- `app/src-tauri/src/lib.rs`：`.plugin(tauri_plugin_opener::init())`
- `app/src-tauri/capabilities/default.json`：加入允许打开 `http:` / `https:` 链接所需的权限（查插件文档确认，一般是 `opener:default`；如果默认权限不含 URL 打开，按插件文档配置 `opener:allow-open-url` 及其 scope）。
- `app/package.json`：`@tauri-apps/plugin-opener@^2`，以及 `@tauri-apps/api@^2`（如果需要）。
- 同阶段 P1-1 也会改这几个 Tauri 文件（加通知插件），各加各的行即可，主会话合并。
- Tauri 检测：`"__TAURI_INTERNALS__" in window`，用动态 `import()` 加载插件，Web 端不受影响。

## 测试

- Vitest 新文件 `app/src/richText.test.ts`，至少覆盖：
  - 中文标点截断：`看这个https://a.com，好的`。
  - 末尾标点：`https://a.com.`、`https://a.com?`、`(见 https://a.com)`、维基百科括号链接。
  - 多个链接、链接紧贴 @提及。
  - 行内代码里的链接和 @ 不被识别。
  - 代码块：带语言名、不带语言名、未闭合、代码块前后有普通文本。
  - `javascript:alert(1)` 不产生链接 Token。
  - 空串、纯空白。
- `app/src-tauri` 下 `cargo check` 必须通过。
- 浏览器实测（真实执行，截图放 `server/target/wt/`）：发一条包含链接、行内代码、带语言名代码块的消息，截图看渲染；点击链接在新标签页打开；点「复制」后剪贴板内容正确（Playwright 需授予 `clipboard-read` 权限）；375px 宽度下长代码块不导致页面横向滚动。

## 不做

- Markdown 粗体、斜体、列表、标题。
- 语法高亮、链接预览卡片（需要服务端抓取）。
- 输入框里的富文本编辑。
