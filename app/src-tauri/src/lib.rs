// Agent A01：宿主命令与状态仅在桌面平台编译（Android 不启动宿主）
#[cfg(desktop)]
mod agent_host;
// Agent A09：托盘、关窗隐藏仅桌面
#[cfg(desktop)]
mod tray;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
