// 内置 agent 的本地工具：read_file / list_dir / grep / write_file /
// edit_file / shell。相对路径都按会话 cwd 解析；输出统一截断在
// OUTPUT_LIMIT 字符内并注明。
//
// describe() 供两处共用：ToolStart 事件上报的 tool_call（title/kind/
// locations），以及 Hooks::before_tool 里向宿主发 session/request_permission
// 时的同一张工具卡片——权限档位按 kind + locations 判，两处必须一致。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent::agent::{Tool, ToolContext, ToolError, ToolOutput};
use agent::llm::{tool_spec, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// 单次工具输出上限（字符），超过截断并注明。
const OUTPUT_LIMIT: usize = 30000;
/// grep 最多返回的匹配行数。
const GREP_LIMIT: usize = 200;
/// list_dir 的深度上限。
const LIST_DEPTH_MAX: u32 = 5;
/// shell 超时（秒）：默认 120，上限 600。
const SHELL_TIMEOUT_DEFAULT: u64 = 120;
const SHELL_TIMEOUT_MAX: u64 = 600;
/// shell 收集输出时最多留的字节数。比 OUTPUT_LIMIT 个 4 字节字符多一点：
/// 一旦丢过输出，留下的一定超过 OUTPUT_LIMIT 字符，capped 必然截断并注明。
const SHELL_KEEP_BYTES: usize = OUTPUT_LIMIT * 4 + 4;
/// 递归遍历时跳过的目录名。
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];

pub(crate) fn all(cwd: &Path) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile { cwd: cwd.into() }),
        Arc::new(ListDir { cwd: cwd.into() }),
        Arc::new(Grep { cwd: cwd.into() }),
        Arc::new(WriteFile { cwd: cwd.into() }),
        Arc::new(EditFile { cwd: cwd.into() }),
        Arc::new(ShellTool { cwd: cwd.into() }),
    ]
}

/// 相对路径按 cwd 解析；绝对路径照用（结果可能仍含 ..，权限判断在宿主侧
/// 按真实路径做，这里只负责拼出可以展示的绝对路径）。
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path.trim());
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn abs_string(cwd: &Path, path: &str) -> String {
    resolve(cwd, path).to_string_lossy().to_string()
}

/// 工具调用 → ACP tool_call 的 (title, kind, locations)。
/// 非内置工具名（注入的 MCP 工具，形如 chat__x）记 kind=other。
pub(crate) fn describe(name: &str, args: &Value, cwd: &Path) -> (String, String, Vec<String>) {
    let arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    let (title, kind, locations) = match name {
        "read_file" => (
            format!("读取 {}", arg("path")),
            "read",
            vec![abs_string(cwd, arg("path"))],
        ),
        "list_dir" => (
            format!("列目录 {}", arg("path")),
            "read",
            vec![abs_string(cwd, arg("path"))],
        ),
        "grep" => (
            format!("搜索 {}", arg("pattern")),
            "search",
            vec![abs_string(cwd, arg("path"))],
        ),
        "write_file" => (
            format!("写入 {}", arg("path")),
            "edit",
            vec![abs_string(cwd, arg("path"))],
        ),
        "edit_file" => (
            format!("修改 {}", arg("path")),
            "edit",
            vec![abs_string(cwd, arg("path"))],
        ),
        "shell" => (
            format!("运行 {}", short(arg("command"), 60)),
            "execute",
            vec![cwd.to_string_lossy().to_string()],
        ),
        other => (other.to_string(), "other", Vec::new()),
    };
    (title, kind.to_string(), locations)
}

fn short(s: &str, n: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn capped(mut s: String) -> String {
    if s.chars().count() > OUTPUT_LIMIT {
        s = s.chars().take(OUTPUT_LIMIT).collect();
        s.push_str(&format!(
            "\n（输出过长已截断，共 {} 字符上限）",
            OUTPUT_LIMIT
        ));
    }
    s
}

fn ok(content: String) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        content: capped(content),
        ..ToolOutput::default()
    })
}

fn err(message: impl Into<String>) -> ToolError {
    ToolError::new(message.into())
}

/// 读文件首块判断是否二进制（含 NUL 即视为二进制）。
fn read_text(path: &Path) -> Result<String, ToolError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("读 {} 失败: {e}", path.display())))?;
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return Err(err(format!("{} 是二进制文件，不可读", path.display())));
    }
    String::from_utf8(bytes).map_err(|_| err(format!("{} 不是 UTF-8 文本", path.display())))
}

fn to_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| err(format!("参数错误: {e}")))
}

// ---------- read_file ----------

struct ReadFile {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct ReadFileArgs {
    /// 文件路径：相对工作目录或绝对路径
    path: String,
    /// 起始行号（从 1 开始，默认 1）
    offset: Option<usize>,
    /// 最多返回多少行（默认到文件尾，输出仍受总字符上限约束）
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        tool_spec::<ReadFileArgs>(
            "read_file",
            "读取一个文本文件的内容，按行号输出。offset / limit 用来读文件的一部分。",
        )
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ReadFileArgs = to_args(args)?;
        let path = resolve(&self.cwd, &args.path);
        tokio::task::spawn_blocking(move || {
            let text = read_text(&path)?;
            let lines: Vec<&str> = text.lines().collect();
            let start = args.offset.unwrap_or(1).max(1);
            if start > lines.len() {
                return Err(err(format!(
                    "offset {start} 超出文件行数（共 {} 行）",
                    lines.len()
                )));
            }
            let take = args.limit.unwrap_or(usize::MAX);
            let mut out = String::new();
            let mut last = start;
            for (i, line) in lines.iter().enumerate().skip(start - 1).take(take) {
                last = i + 1;
                out.push_str(&format!("{:>6}|{}\n", i + 1, line));
            }
            if last < lines.len() {
                out.push_str(&format!(
                    "（还有 {} 行，用 offset={} 继续）\n",
                    lines.len() - last,
                    last + 1
                ));
            }
            ok(out)
        })
        .await
        .map_err(|e| err(format!("read_file 任务失败: {e}")))?
    }
}

// ---------- list_dir ----------

struct ListDir {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct ListDirArgs {
    /// 目录路径：相对工作目录或绝对路径
    path: String,
    /// 递归深度：1=只看直接子项（默认），最大 5
    depth: Option<u32>,
}

#[async_trait::async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        tool_spec::<ListDirArgs>(
            "list_dir",
            "列出目录内容（树形，目录名带 / 结尾）。depth 控制递归深度（默认 1，最大 5）；.git、node_modules、target 自动跳过。",
        )
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ListDirArgs = to_args(args)?;
        let root = resolve(&self.cwd, &args.path);
        let depth = args.depth.unwrap_or(1).clamp(1, LIST_DEPTH_MAX);
        tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&root)
                .map_err(|e| err(format!("打不开 {}: {e}", root.display())))?;
            if !meta.is_dir() {
                return Err(err(format!("{} 不是目录", root.display())));
            }
            let mut out = format!("{}/\n", root.display());
            list_into(&root, depth, 1, &mut out);
            ok(out)
        })
        .await
        .map_err(|e| err(format!("list_dir 任务失败: {e}")))?
    }
}

fn list_into(dir: &Path, depth: u32, level: u32, out: &mut String) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let indent = "  ".repeat(level as usize);
        if is_dir {
            out.push_str(&format!("{indent}{name}/\n"));
            if level < depth {
                list_into(&e.path(), depth, level + 1, out);
            }
        } else {
            out.push_str(&format!("{indent}{name}\n"));
        }
    }
}

// ---------- grep ----------

struct Grep {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct GrepArgs {
    /// 正则表达式
    pattern: String,
    /// 搜索起点：文件或目录（相对工作目录或绝对路径），默认工作目录
    path: Option<String>,
    /// 文件名过滤，只支持 *.ext 这种后缀写法（如 *.rs）
    glob: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        tool_spec::<GrepArgs>(
            "grep",
            "在文件里按正则搜索，输出 文件:行号: 内容，最多 200 条。glob 只支持 *.ext 后缀过滤。",
        )
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = to_args(args)?;
        let re = regex::Regex::new(&args.pattern).map_err(|e| err(format!("正则无效: {e}")))?;
        // glob 只认 *.ext 形态，取后缀做文件名结尾匹配
        let suffix = match &args.glob {
            Some(g) => match g.strip_prefix('*') {
                Some(s) if !s.is_empty() => Some(s.to_string()),
                _ => {
                    return Err(err(format!(
                        "glob 只支持 *.ext 这种后缀过滤，收到 {:?}",
                        args.glob
                    )));
                }
            },
            None => None,
        };
        let root = args
            .path
            .as_deref()
            .map(|p| resolve(&self.cwd, p))
            .unwrap_or_else(|| self.cwd.clone());
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            if !root.exists() {
                return Err(err(format!("{} 不存在", root.display())));
            }
            let mut out = String::new();
            let mut count = 0usize;
            let mut queue = VecDeque::from([root.clone()]);
            while let Some(item) = queue.pop_front() {
                if count >= GREP_LIMIT {
                    break;
                }
                let meta = match std::fs::metadata(&item) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    if let Ok(rd) = std::fs::read_dir(&item) {
                        for e in rd.filter_map(|e| e.ok()) {
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                            let name = e.file_name();
                            if is_dir && SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                                continue;
                            }
                            queue.push_back(e.path());
                        }
                    }
                    continue;
                }
                if let Some(suf) = &suffix {
                    if !item
                        .file_name()
                        .map(|n| n.to_string_lossy().ends_with(suf.as_str()))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                let Ok(text) = read_text(&item) else {
                    continue; // 二进制 / 读不了的文件跳过
                };
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let shown = item
                            .strip_prefix(&cwd)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| item.to_string_lossy().to_string());
                        out.push_str(&format!("{}:{}: {}\n", shown, i + 1, line));
                        count += 1;
                        if count >= GREP_LIMIT {
                            break;
                        }
                    }
                }
            }
            if count >= GREP_LIMIT {
                out.push_str(&format!("（匹配过多，只显示前 {GREP_LIMIT} 条）\n"));
            } else if count == 0 {
                out.push_str("（无匹配）\n");
            }
            ok(out)
        })
        .await
        .map_err(|e| err(format!("grep 任务失败: {e}")))?
    }
}

// ---------- write_file ----------

struct WriteFile {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct WriteFileArgs {
    /// 文件路径：相对工作目录或绝对路径；父目录不存在会自动创建
    path: String,
    /// 要写入的完整内容
    content: String,
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        tool_spec::<WriteFileArgs>(
            "write_file",
            "把内容完整写入文件（覆盖已有内容）；父目录不存在时自动创建。改已有文件的小部分内容优先用 edit_file。",
        )
    }

    fn exclusive(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: WriteFileArgs = to_args(args)?;
        let path = resolve(&self.cwd, &args.path);
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| err(format!("创建目录 {} 失败: {e}", parent.display())))?;
            }
            std::fs::write(&path, args.content.as_bytes())
                .map_err(|e| err(format!("写 {} 失败: {e}", path.display())))?;
            ok(format!(
                "已写入 {}（{} 字节）",
                path.display(),
                args.content.len()
            ))
        })
        .await
        .map_err(|e| err(format!("write_file 任务失败: {e}")))?
    }
}

// ---------- edit_file ----------

struct EditFile {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct EditFileArgs {
    /// 文件路径：相对工作目录或绝对路径
    path: String,
    /// 要被替换的原文（必须在文件里恰好出现一次；要替换多处传 replace_all）
    old: String,
    /// 替换后的新文本
    new: String,
    /// true = 把所有出现都替换掉
    replace_all: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        tool_spec::<EditFileArgs>(
            "edit_file",
            "把文件里的 old 文本替换为 new。old 必须在文件里唯一，否则传 replace_all=true 全部替换。",
        )
    }

    fn exclusive(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EditFileArgs = to_args(args)?;
        if args.old.is_empty() {
            return Err(err("old 不能为空"));
        }
        let path = resolve(&self.cwd, &args.path);
        tokio::task::spawn_blocking(move || {
            let text = read_text(&path)?;
            let n = text.matches(&args.old).count();
            if n == 0 {
                return Err(err(format!("在 {} 里没找到要替换的内容", path.display())));
            }
            let replace_all = args.replace_all.unwrap_or(false);
            if n > 1 && !replace_all {
                return Err(err(format!(
                    "old 在 {} 里出现 {n} 次，不唯一；传 replace_all=true 才能全部替换",
                    path.display()
                )));
            }
            let out = if replace_all {
                text.replace(&args.old, &args.new)
            } else {
                text.replacen(&args.old, &args.new, 1)
            };
            std::fs::write(&path, out.as_bytes())
                .map_err(|e| err(format!("写 {} 失败: {e}", path.display())))?;
            ok(format!("已更新 {}（替换 {n} 处）", path.display()))
        })
        .await
        .map_err(|e| err(format!("edit_file 任务失败: {e}")))?
    }
}

// ---------- shell ----------

struct ShellTool {
    cwd: PathBuf,
}

#[derive(JsonSchema, Deserialize)]
struct ShellArgs {
    /// 要执行的命令行（Windows 下经 cmd /C，其余经 sh -c；工作目录为会话目录）
    command: String,
    /// 超时秒数（默认 120，最大 600）；超时杀掉整个进程树
    timeout_secs: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        tool_spec::<ShellArgs>(
            "shell",
            "在工作目录里执行一条 shell 命令，返回合并的 stdout/stderr 和退出码。默认超时 120 秒。",
        )
    }

    fn exclusive(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs = to_args(args)?;
        if args.command.trim().is_empty() {
            return Err(err("command 不能为空"));
        }
        let timeout = args
            .timeout_secs
            .unwrap_or(SHELL_TIMEOUT_DEFAULT)
            .clamp(1, SHELL_TIMEOUT_MAX);
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &args.command]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &args.command]);
            c
        };
        cmd.current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Unix：子进程独立进程组，超时能整组杀；Windows 用 taskkill /T。
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| err(format!("命令启动失败: {e}")))?;

        // stdout / stderr 各起一个读任务，按到达顺序合并进一个缓冲。
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        if let Some(io) = child.stdout.take() {
            spawn_pipe_reader(io, tx.clone());
        }
        if let Some(io) = child.stderr.take() {
            spawn_pipe_reader(io, tx);
        }
        // 读输出和等退出共用一个截止时间：先关管道再继续跑的进程也逃不过超时。
        // 只留前 SHELL_KEEP_BYTES，再多的边读边丢：刷屏的命令占不了多少内存，管道照样读空不卡子进程。
        // ponytail: 留的是开头，报错常在结尾；需要时改成保留首尾各一段。
        let run = async {
            let mut buf = Vec::new();
            while let Some(chunk) = rx.recv().await {
                let room = SHELL_KEEP_BYTES.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            (buf, child.wait().await)
        };
        let res = tokio::time::timeout(Duration::from_secs(timeout), run).await;
        match res {
            Ok((output, status)) => {
                let status = status.map_err(|e| err(format!("等进程退出失败: {e}")))?;
                let code = status.code().unwrap_or(-1);
                let text = String::from_utf8_lossy(&output);
                let text = text.trim_end();
                let body = if text.is_empty() {
                    String::new()
                } else {
                    format!("{text}\n")
                };
                ok(format!("{body}[退出码 {code}]"))
            }
            Err(_) => {
                kill_tree(&mut child);
                let _ = child.wait().await;
                ok(format!("（命令超过 {timeout} 秒，进程已终止）"))
            }
        }
    }
}

/// 把子进程的一条输出管道读进合并缓冲。
fn spawn_pipe_reader<R>(io: R, tx: tokio::sync::mpsc::Sender<Vec<u8>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut r = tokio::io::BufReader::new(io);
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 超时后杀整棵进程树：Windows 用 taskkill /T /F，Unix 杀进程组。
fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(id) = child.id() {
        if cfg!(windows) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &id.to_string(), "/T", "/F"])
                .output();
        } else {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{id}")])
                .output();
        }
    }
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::agent::{Agent, AgentMessage};
    use agent::llm::{AssistantMessage, LlmEvent, Message, ScriptedProvider, StopReason};
    use serde_json::json;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("a29-tools-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn done(text: &str, calls: Vec<agent::llm::ToolCall>, stop: StopReason) -> LlmEvent {
        LlmEvent::Done(AssistantMessage {
            text: text.to_string(),
            reasoning: None,
            tool_calls: calls,
            stop,
            usage: None,
            replay: None,
        })
    }

    /// 用 ScriptedProvider 起一次真实 run：第一轮模型发这些 tool calls，
    /// 第二轮纯文本收尾。返回按调用顺序的每个工具结果（content, is_error）。
    async fn tool_results(tool: Arc<dyn Tool>, calls: &[(&str, Value)]) -> Vec<(String, bool)> {
        use agent::llm::ToolCall;
        let tool_calls: Vec<ToolCall> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("c{i}"),
                name: name.to_string(),
                arguments: args.to_string(),
            })
            .collect();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![Ok(done("", tool_calls, StopReason::ToolUse))],
            vec![Ok(done("done", vec![], StopReason::Stop))],
        ]));
        let agent = Agent::builder(provider).tool_arc(tool).build();
        let outcome = agent.prompt("go").unwrap().outcome().await;
        outcome
            .new_messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(Message::ToolResult {
                    content, is_error, ..
                }) => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect()
    }

    /// 单个工具调用的便捷封装。
    async fn run_tool(tool: Arc<dyn Tool>, name: &str, args: Value) -> (String, bool) {
        let mut results = tool_results(tool, &[(name, args)]).await;
        assert_eq!(results.len(), 1, "expected one tool result");
        results.remove(0)
    }

    #[tokio::test]
    async fn read_file_lines_offset_limit() {
        let d = dir();
        std::fs::write(d.join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        let tool: Arc<dyn Tool> = Arc::new(ReadFile { cwd: d.clone() });
        let (content, is_err) = run_tool(tool.clone(), "read_file", json!({"path": "a.txt"})).await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("1|l1"), "{}", content);
        assert!(content.contains("4|l4"), "{}", content);

        let (content, is_err) = run_tool(
            tool.clone(),
            "read_file",
            json!({"path": "a.txt", "offset": 2, "limit": 2}),
        )
        .await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("2|l2"), "{}", content);
        assert!(content.contains("3|l3"), "{}", content);
        assert!(!content.contains("|l4"), "{}", content);
        assert!(content.contains("还有 1 行"), "{}", content);

        // 二进制文件报错
        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 0]).unwrap();
        let (content, is_err) = run_tool(tool, "read_file", json!({"path": "b.bin"})).await;
        assert!(is_err, "{}", content);
        assert!(content.contains("二进制"), "{}", content);
    }

    #[tokio::test]
    async fn edit_file_requires_unique_old() {
        let d = dir();
        std::fs::write(d.join("a.txt"), "aa bb aa").unwrap();
        let tool: Arc<dyn Tool> = Arc::new(EditFile { cwd: d.clone() });
        let (content, is_err) = run_tool(
            tool.clone(),
            "edit_file",
            json!({"path":"a.txt","old":"aa","new":"x"}),
        )
        .await;
        assert!(is_err, "{}", content);
        assert!(content.contains("不唯一"), "{}", content);

        let (content, is_err) = run_tool(
            tool.clone(),
            "edit_file",
            json!({"path":"a.txt","old":"zzz","new":"x"}),
        )
        .await;
        assert!(is_err, "{}", content);
        assert!(content.contains("没找到"), "{}", content);

        let (content, is_err) = run_tool(
            tool.clone(),
            "edit_file",
            json!({"path":"a.txt","old":"bb","new":"cc"}),
        )
        .await;
        assert!(!is_err, "{}", content);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "aa cc aa"
        );

        let (_, is_err) = run_tool(
            tool,
            "edit_file",
            json!({"path":"a.txt","old":"aa","new":"x","replace_all":true}),
        )
        .await;
        assert!(!is_err);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "x cc x");
    }

    #[tokio::test]
    async fn grep_suffix_glob_and_limit() {
        let d = dir();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.rs"), "fn main() {}\n// todo here\n").unwrap();
        std::fs::write(d.join("sub").join("b.md"), "todo in md\n").unwrap();
        std::fs::create_dir_all(d.join("node_modules").join("pkg")).unwrap();
        std::fs::write(
            d.join("node_modules").join("pkg").join("c.rs"),
            "todo skip\n",
        )
        .unwrap();
        let tool: Arc<dyn Tool> = Arc::new(Grep { cwd: d.clone() });

        let (content, is_err) = run_tool(tool.clone(), "grep", json!({"pattern":"todo"})).await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("a.rs:2: // todo here"), "{}", content);
        assert!(content.contains("b.md:1: todo in md"), "{}", content);
        assert!(
            !content.contains("c.rs"),
            "node_modules 应跳过: {}",
            content
        );

        let (content, is_err) = run_tool(
            tool.clone(),
            "grep",
            json!({"pattern":"todo","glob":"*.rs"}),
        )
        .await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("a.rs:2:"), "{}", content);
        assert!(!content.contains("b.md"), "{}", content);

        let (content, is_err) = run_tool(tool, "grep", json!({"pattern":"x","glob":"?.rs"})).await;
        assert!(is_err, "{}", content);
        assert!(content.contains("*.ext"), "{}", content);
    }

    #[tokio::test]
    async fn list_dir_skips_noise_dirs() {
        let d = dir();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src").join("a.rs"), "x").unwrap();
        std::fs::create_dir_all(d.join("target").join("debug")).unwrap();
        std::fs::write(d.join("target").join("debug").join("bin"), "x").unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join(".git").join("HEAD"), "ref").unwrap();
        let tool: Arc<dyn Tool> = Arc::new(ListDir { cwd: d.clone() });

        let (content, is_err) =
            run_tool(tool.clone(), "list_dir", json!({"path": ".", "depth": 3})).await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("src/"), "{}", content);
        assert!(content.contains("a.rs"), "{}", content);
        assert!(!content.contains("target/"), "{}", content);
        assert!(!content.contains(".git"), "{}", content);

        let (content, is_err) = run_tool(tool, "list_dir", json!({"path": "."})).await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("src/"), "{}", content);
        assert!(!content.contains("a.rs"), "{}", content);
    }

    #[tokio::test]
    async fn shell_runs_and_reports_exit_code() {
        let d = dir();
        let tool: Arc<dyn Tool> = Arc::new(ShellTool { cwd: d.clone() });
        let cmd = if cfg!(windows) {
            "echo hi& exit /b 3"
        } else {
            "echo hi; exit 3"
        };
        let (content, is_err) =
            run_tool(tool, "shell", json!({"command": cmd, "timeout_secs": 10})).await;
        assert!(!is_err, "{}", content);
        assert!(content.contains("hi"), "{}", content);
        assert!(content.contains("退出码 3"), "{}", content);
    }

    /// 超大输出：只留前一段，结果照常返回并注明截断
    #[tokio::test]
    async fn shell_huge_output_is_capped() {
        let d = dir();
        std::fs::write(d.join("big.txt"), "x".repeat(SHELL_KEEP_BYTES * 3)).unwrap();
        let tool: Arc<dyn Tool> = Arc::new(ShellTool { cwd: d.clone() });
        let cmd = if cfg!(windows) { "type big.txt" } else { "cat big.txt" };
        let (content, is_err) =
            run_tool(tool, "shell", json!({"command": cmd, "timeout_secs": 30})).await;
        assert!(!is_err, "{}", &content[..200.min(content.len())]);
        assert!(content.contains("输出过长已截断"));
        assert!(content.chars().count() < OUTPUT_LIMIT + 100);
    }

    /// 先关掉输出再继续跑的命令：超时照样生效（以前只限制读输出，等退出不限时）
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_covers_process_exit() {
        let tool: Arc<dyn Tool> = Arc::new(ShellTool { cwd: dir() });
        let started = std::time::Instant::now();
        let (content, _) = run_tool(
            tool,
            "shell",
            json!({"command": "exec >/dev/null 2>&1; sleep 30", "timeout_secs": 1}),
        )
        .await;
        assert!(content.contains("进程已终止"), "{content}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let d = dir();
        let w: Arc<dyn Tool> = Arc::new(WriteFile { cwd: d.clone() });
        let (_, is_err) = run_tool(
            w,
            "write_file",
            json!({"path":"sub/n.txt","content":"hello"}),
        )
        .await;
        assert!(!is_err);
        assert_eq!(
            std::fs::read_to_string(d.join("sub").join("n.txt")).unwrap(),
            "hello"
        );
    }
}
