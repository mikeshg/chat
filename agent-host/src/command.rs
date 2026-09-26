// 启动 Agent 进程前按 PATH + PATHEXT 解析命令。
// Windows 上 CreateProcess 不能直接跑 .cmd / .bat（参数会被吃掉），
// 解析到这两种扩展名时改经 cmd.exe /d /s /c 启动。自定义命令和登记表装出来的
// npm 垫片走同一条路。

use std::path::{Path, PathBuf};
use tokio::process::Command;

/// 在给定 PATH / PATHEXT 里找命令。找不到返回 None（调用方再退回原命令，让系统报错）。
pub fn resolve_in(command: &str, path_env: &str, pathext: &str) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let exts = pathext_list(pathext);
    let direct = command.contains('/') || command.contains('\\');
    if direct {
        return try_file(Path::new(command), &exts);
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_env.split(sep) {
        if dir.is_empty() {
            continue;
        }
        if let Some(hit) = try_file(&Path::new(dir).join(command), &exts) {
            return Some(hit);
        }
    }
    None
}

/// 用进程当前的 PATH / PATHEXT 组一条可 spawn 的命令（参数已放进去）。
pub fn prepare(command: &str, args: &[String]) -> Command {
    let path = std::env::var("PATH").unwrap_or_default();
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| default_pathext());
    prepare_in(command, args, &path, &pathext)
}

/// 同 `prepare`，但 PATH / PATHEXT 由调用方给定（测试用）。
pub fn prepare_in(command: &str, args: &[String], path_env: &str, pathext: &str) -> Command {
    match resolve_in(command, path_env, pathext) {
        Some(path) if is_batch(&path) => batch_command(&path, args),
        Some(path) => {
            let mut cmd = Command::new(path);
            cmd.args(args);
            cmd
        }
        None => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            cmd
        }
    }
}

fn default_pathext() -> String {
    if cfg!(windows) {
        ".COM;.EXE;.BAT;.CMD".into()
    } else {
        String::new()
    }
}

fn pathext_list(pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('.') {
                s.to_string()
            } else {
                format!(".{s}")
            }
        })
        .collect()
}

/// 无扩展名时按 PATHEXT 顺序试；已有扩展名则只认这个文件。
fn try_file(base: &Path, exts: &[String]) -> Option<PathBuf> {
    let name = base.file_name()?.to_string_lossy();
    let parent = base.parent().unwrap_or(Path::new(""));
    let has_ext = Path::new(name.as_ref()).extension().is_some();
    if has_ext {
        let exact = parent.join(name.as_ref());
        return exact.is_file().then_some(exact);
    }
    for ext in exts {
        let cand = parent.join(format!("{name}{ext}"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    let exact = parent.join(name.as_ref());
    exact.is_file().then_some(exact)
}

fn is_batch(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => {
            let e = e.to_ascii_lowercase();
            e == "cmd" || e == "bat"
        }
        None => false,
    }
}

/// Node 在 Windows 上跑 .cmd 的写法：`cmd.exe /d /s /c ""script" args"`。
/// `/s` 会剥掉最外层一对引号，所以脚本路径里的空格能留下来，stdin 仍交给脚本。
fn batch_command(script: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(if cfg!(windows) { "cmd.exe" } else { "sh" });
    #[cfg(windows)]
    {
        let mut line = String::from("/d /s /c \"");
        line.push('"');
        line.push_str(&script.display().to_string());
        line.push('"');
        for a in args {
            line.push(' ');
            line.push_str(&quote_cmd_arg(a));
        }
        line.push('"');
        cmd.raw_arg(line);
    }
    #[cfg(not(windows))]
    {
        cmd.arg(script);
        cmd.args(args);
    }
    cmd
}

#[cfg(windows)]
fn quote_cmd_arg(a: &str) -> String {
    if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", a.replace('"', "\\\""))
    } else {
        a.to_string()
    }
}
