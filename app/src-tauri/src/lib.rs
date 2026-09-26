// Agent A01：宿主命令与状态仅在桌面平台编译（Android 不启动宿主）
#[cfg(desktop)]
mod agent_host;
// Agent A09：托盘、关窗隐藏仅桌面
#[cfg(desktop)]
mod tray;

/// macOS 从 Finder / Dock 启动的程序只拿到 launchd 的精简 PATH（/usr/bin:/bin:…），
/// Homebrew / nvm 装的 node、claude 都找不到：登记表会误报「需要先安装 Node.js」，
/// 自定义命令也起不来。启动时向登录 shell 要一份完整 PATH。
#[cfg(desktop)]
fn import_login_path() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(shell)
            .args(["-ilc", "printf '__CHAT_PATH__%s' \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    // ponytail: rc 文件卡住（等交互输入）就放弃，沿用原 PATH
    let Ok(Ok(out)) = rx.recv_timeout(std::time::Duration::from_secs(3)) else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some((_, path)) = text.rsplit_once("__CHAT_PATH__") {
        if !path.trim().is_empty() {
            std::env::set_var("PATH", path.trim());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(desktop)]
    import_login_path();
    #[cfg(desktop)]
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .with_writer(std::io::stderr)
        .try_init();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init());
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        None,
    ));
    #[cfg(desktop)]
    let builder = builder
        .manage(agent_host::AgentHostState::new())
        .invoke_handler(tauri::generate_handler![
            agent_host::agent_host_start,
            agent_host::agent_host_stop,
            agent_host::agent_host_ticket,
        ])
        .setup(|app| {
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(tray::on_window_event);
    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
