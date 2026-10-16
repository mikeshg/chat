// 托盘、关窗隐藏仅桌面
#[cfg(desktop)]
mod tray;
#[cfg(desktop)]
mod updater;
// MM4：附件直存下载目录（桌面走 reqwest，Android 走 Kotlin MediaStore 插件）
mod download;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(download::init());
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        None,
    ));
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    #[cfg(desktop)]
    let builder = builder
        .invoke_handler(tauri::generate_handler![
            download::download_to_downloads,
            updater::install_update
        ])
        .setup(|app| {
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(tray::on_window_event);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        download::download_to_downloads
    ]);
    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
