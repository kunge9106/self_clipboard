//! 托盘图标（迁移自 v1 Tray：关闭时后台运行）。
//!
//! 按需创建：勾选「关闭时后台运行」且用户关闭窗口时才创建；
//! 取消勾选时移除。菜单：显示 / 退出。

use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

static TRAY: Mutex<Option<tauri::tray::TrayIcon>> = Mutex::new(None);

pub fn create_tray(app: &AppHandle) {
    {
        let mut guard = TRAY.lock().unwrap();
        if guard.is_some() {
            return;
        }

        let show = MenuItem::with_id(app, "show", "显示轻贴", true, None::<&str>)
            .expect("创建托盘菜单项失败");
        let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)
            .expect("创建托盘菜单项失败");
        let menu = Menu::with_items(app, &[&show, &quit]).expect("创建托盘菜单失败");

        let icon = app.default_window_icon().cloned();
        let builder = TrayIconBuilder::with_id("main-tray")
            .tooltip("轻贴 — 剪贴板管理器")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| match event.id.as_ref() {
                "show" => show_main(app),
                "quit" => {
                    println!("[tray] 用户请求退出");
                    crate::clipboard::on_app_exit();
                    app.exit(0);
                }
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                // 双击托盘图标 → 显示主窗口（同 v1）
                if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                    show_main(tray.app_handle());
                }
            });

        let tray = match icon {
            Some(icon) => builder.icon(icon).build(app),
            None => builder.build(app),
        }
        .expect("创建托盘失败");

        *guard = Some(tray);
    }
    println!("[tray] 已创建托盘图标");
}

pub fn remove_tray() {
    let mut guard = TRAY.lock().unwrap();
    if guard.take().is_some() {
        println!("[tray] 已移除托盘图标");
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}
