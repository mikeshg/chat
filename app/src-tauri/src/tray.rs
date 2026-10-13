// 桌面托盘。关窗口只是隐藏，继续收消息通知；只有菜单「退出」才结束进程。
// Android 不编译本模块。

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Window, WindowEvent};
use tauri_plugin_notification::NotificationExt;

static QUITTING: AtomicBool = AtomicBool::new(false);
static TRAY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

const TRAY_HINT: &str = "Chat 已最小化到托盘，会继续接收消息";

fn box_err(err: impl std::error::Error + Send + Sync + 'static) -> Box<dyn std::error::Error> {
    Box::new(err)
}

pub fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>).map_err(box_err)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>).map_err(box_err)?;
    let menu = Menu::with_items(app, &[&show, &quit]).map_err(box_err)?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Chat")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => reveal(app),
            "quit" => request_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if click_shows_window(&event) {
                reveal(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    // build 会把图标登记进应用资源表，这里丢掉局部变量不会卸掉托盘。
    builder.build(app).map_err(box_err)?;
    Ok(())
}

// Linux 下左键单击收不到事件：Tauri 的 TrayIconEvent 在 Linux 标注为
// Unsupported，因为后端 libayatana-appindicator 只有 Activate/ContextMenu
// 信号且两者都会弹出菜单，应用拿不到点击。所以 Linux 上左/右键行为都是弹
// 菜单，菜单首项「显示窗口」是入口；本函数只在 Windows/macOS 生效。
fn click_shows_window(event: &TrayIconEvent) -> bool {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => true,
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => true,
        _ => false,
    }
}

fn reveal(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// QUITTING 让随后的 CloseRequested 真正关掉窗口。
fn request_quit(app: &AppHandle) {
    if !QUITTING.swap(true, Ordering::SeqCst) {
        app.exit(0);
    }
}

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if QUITTING.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_close();
    let _ = window.hide();
    if TRAY_HINT_SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = window
        .notification()
        .builder()
        .title("Chat")
        .body(TRAY_HINT)
        .show();
}
