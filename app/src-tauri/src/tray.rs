// 桌面托盘。关窗口只是隐藏，继续收消息通知；只有菜单「退出」才结束进程。
// Android 不编译本模块。

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, Window, WindowEvent};
use tauri_plugin_notification::NotificationExt;

#[cfg(not(target_os = "linux"))]
use tauri::menu::{Menu, MenuItem};
#[cfg(not(target_os = "linux"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

static QUITTING: AtomicBool = AtomicBool::new(false);
static TRAY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

const TRAY_HINT: &str = "Chat 已最小化到托盘，会继续接收消息";

fn box_err(err: impl std::error::Error + Send + Sync + 'static) -> Box<dyn std::error::Error> {
    Box::new(err)
}

// Linux 的托盘走 libayatana-appindicator：Activate/ContextMenu 一律弹菜单，
// TrayIconEvent 一个也不发（tauri::tray::TrayIconEvent 文档注明 Linux Unsupported），
// 所以左键单击事件收不到——左/右键行为都是弹出菜单，菜单首项「显示窗口」即入口。
// ayatana 唯一还给应用的点击是 SecondaryActivate（中键）：可把一个菜单项设为目标，
// 中键时直接 gtk_widget_activate 它。用它让中键单击也能显示窗口。
#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::c_void;

    use tauri::AppHandle;

    use super::{box_err, on_menu_item, reveal};

    // TrayIcon 内部是 Rc，跨不了线程；托盘只在 GTK 主线程创建/使用，线程本地存着防 drop。
    thread_local! {
        static TRAY: std::cell::RefCell<Option<tray_icon::TrayIcon>> =
            const { std::cell::RefCell::new(None) };
    }

    pub fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let show = muda::MenuItem::with_id("show", "显示窗口", true, None);
        let quit = muda::MenuItem::with_id("quit", "退出", true, None);
        let menu = muda::Menu::with_items(&[&show, &quit]).map_err(box_err)?;
        let mut builder = tray_icon::TrayIconBuilder::new()
            .with_id("main")
            .with_tooltip("Chat")
            .with_menu(Box::new(menu));
        if let Some(icon) = app.default_window_icon() {
            if let Ok(icon) =
                tray_icon::Icon::from_rgba(icon.rgba().to_vec(), icon.width(), icon.height())
            {
                builder = builder.with_icon(icon);
            }
        }
        let tray = builder.build().map_err(box_err)?;
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
        // 菜单项事件由 muda 全局通道分发，Tauri 已接到 on_menu_event 监听器上。
        app.on_menu_event(|app, event| on_menu_item(app, event.id.as_ref()));
        bind_secondary_activate(app);
        Ok(())
    }

    fn bind_secondary_activate(app: &AppHandle) {
        use gtk::prelude::*;
        // 目标菜单项不进托盘菜单，只是「显示窗口」动作的载体；
        // ayatana 激活前会检查 visible/sensitive，所以先 show。
        let item = gtk::MenuItem::with_label("显示窗口");
        item.show();
        let handle = app.clone();
        item.connect_activate(move |_| reveal(&handle));
        unsafe {
            // libappindicator::AppIndicator 只有一个字段 air（C 侧 AppIndicator*），
            // repr(C) 垫片读出它，再 dlopen 拿 set_secondary_activate_target。
            #[repr(C)]
            struct AppIndicatorShim(*mut c_void);
            let indicator = TRAY.with(|t| {
                t.borrow()
                    .as_ref()
                    .map(|tray| (*(tray.app_indicator() as *const AppIndicatorShim)).0)
            });
            let Some(indicator) = indicator else {
                return;
            };
            let Ok(lib) = libloading::Library::new("libayatana-appindicator3.so.1")
                .or_else(|_| libloading::Library::new("libappindicator3.so.1"))
            else {
                return;
            };
            type Setter = unsafe extern "C" fn(*mut c_void, *mut c_void);
            let Ok(set_target) =
                lib.get::<Setter>(b"app_indicator_set_secondary_activate_target\0")
            else {
                return;
            };
            set_target(
                indicator,
                gtk::glib::translate::ToGlibPtr::<*mut gtk::ffi::GtkMenuItem>::to_glib_none(&item)
                    .0
                    .cast(),
            );
            // 托盘、菜单项、动态库都要与进程同寿，主动泄漏防 drop。
            std::mem::forget((lib, item));
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::install;

#[cfg(not(target_os = "linux"))]
pub fn install(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>).map_err(box_err)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>).map_err(box_err)?;
    let menu = Menu::with_items(app, &[&show, &quit]).map_err(box_err)?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Chat")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu_item(app, event.id.as_ref()))
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

#[cfg(not(target_os = "linux"))]
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

fn on_menu_item(app: &AppHandle, id: &str) {
    match id {
        "show" => reveal(app),
        "quit" => request_quit(app),
        _ => {}
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
