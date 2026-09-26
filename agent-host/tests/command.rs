// Agent A07：PATH + PATHEXT 解析，以及 .cmd 经 cmd.exe 启动（含 stdin）。

use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("a07-cmd-{}-{}", std::process::id(), uniq()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn uniq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test]
async fn pathext_resolves_cmd_and_runs_it() {
    let root = scratch();
    // 路径里带空格，确认 cmd.exe 的引号没把脚本拆开
    let dir = root.join("bin dir");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("hello.cmd");
    std::fs::write(&script, "@echo off\r\necho pong-from-cmd\r\n").unwrap();
    // 同时放一个不该被选中的同名前缀文件
    std::fs::write(dir.join("hello2.cmd"), "@echo off\r\necho wrong\r\n").unwrap();

    let path = dir.display().to_string();
    let pathext = ".COM;.EXE;.BAT;.CMD";
    let resolved =
        agent_host::command::resolve_in("hello", &path, pathext).expect("应解析到 hello.cmd");
    assert!(
        resolved.extension().unwrap().eq_ignore_ascii_case("cmd"),
        "{resolved:?}"
    );
    assert_eq!(
        resolved
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase(),
        "hello"
    );

    let mut cmd = agent_host::command::prepare_in("hello", &[], &path, pathext);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().await.expect("spawn");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("pong-from-cmd"),
        "status={:?} stdout={text} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// .exe 排在 PATHEXT 里 .cmd 前面时，应选 exe 而不是批处理。
#[test]
fn pathext_prefers_earlier_extension() {
    let root = scratch();
    std::fs::write(root.join("tool.cmd"), "cmd").unwrap();
    std::fs::write(root.join("tool.exe"), "exe").unwrap();
    let hit =
        agent_host::command::resolve_in("tool", &root.display().to_string(), ".EXE;.CMD").unwrap();
    assert!(
        hit.extension().unwrap().eq_ignore_ascii_case("exe"),
        "{hit:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 批处理要能读到管道里的 stdin（ACP 的 JSON-RPC 走 stdin）。
#[tokio::test]
async fn cmd_shim_forwards_stdin() {
    let root = scratch();
    let script = root.join("read.cmd");
    std::fs::write(&script, "@echo off\r\nset /p LINE=\r\necho got:%LINE%\r\n").unwrap();
    let mut cmd = agent_host::command::prepare_in(&script.display().to_string(), &[], "", ".CMD");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"ping\r\n").await.unwrap();
    }
    let out = child.wait_with_output().await.expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("got:ping"),
        "status={:?} stdout={text} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
