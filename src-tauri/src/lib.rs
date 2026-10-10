use tauri::Theme;

#[cfg(windows)]
use tauri::Manager;

#[cfg(windows)]
mod effects;
#[cfg(windows)]
mod clipboard;
mod settings;
mod autostart;
mod tray;

/// 前端启动时调用，拿到实际生效的材质档位与圆角上限。
/// 注意：返回的是「实际生效」而非「尝试应用」的档位——这是 v1 静默失败的直接教训。
#[tauri::command]
fn effect_info() -> serde_json::Value {
    #[cfg(windows)]
    {
        let tier = effects::current_tier();
        serde_json::json!({
            "tier": tier.as_str(),
            "build": effects::current_build(),
            "maxRadius": tier.max_radius(),
            "round": settings::get().round_mode,
        })
    }
    #[cfg(not(windows))]
    {
        serde_json::json!({ "tier": "solid", "build": 0, "maxRadius": 12, "round": false })
    }
}

/// 圆角模式开关：圆角 + 不用系统 blur（四角真透明、100% 干净），立即生效
#[tauri::command]
fn set_round_mode(app: tauri::AppHandle, value: bool) -> Result<(), String> {
    settings::update(|c| c.round_mode = value);
    #[cfg(windows)]
    {
        use tauri::Manager;
        if let Some(w) = app.get_webview_window("main") {
            if let Ok(h) = w.hwnd() {
                let dark = matches!(w.theme().unwrap_or(Theme::Light), Theme::Dark);
                effects::apply_round_mode(h.0 as isize, dark);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = app;
    Ok(())
}

/// 材质浓度（设置页滑块）：更新 tint alpha 并立即重刷亚克力
#[tauri::command]
fn set_material_alpha(app: tauri::AppHandle, value: u32) -> Result<(), String> {
    settings::update(|c| c.material_alpha = value.clamp(40, 90));
    #[cfg(windows)]
    {
        use tauri::Manager;
        if let Some(w) = app.get_webview_window("main") {
            if let Ok(h) = w.hwnd() {
                let dark = matches!(w.theme().unwrap_or(Theme::Light), Theme::Dark);
                effects::reapply_material(h.0 as isize, dark);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = app;
    Ok(())
}

#[cfg(windows)]
fn setup_effects(app: &tauri::AppHandle) {
    // —— 诊断：确认运行时配置与窗口实际状态是否一致 ——
    let cfg_win = &app.config().app.windows;
    println!("[debug] config windows = {:?}", cfg_win);

    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => {
            eprintln!("[effects] 找不到 main 窗口，跳过材质设置");
            return;
        }
    };

    let hwnd = match window.hwnd() {
        Ok(h) => h.0 as isize,
        Err(e) => {
            eprintln!("[effects] 获取 hwnd 失败：{}，按 solid 处理", e);
            return;
        }
    };

    let dark = matches!(window.theme().unwrap_or(Theme::Light), Theme::Dark);

    // —— 诊断：窗口运行时状态 ——
    println!(
        "[debug] runtime: decorated={} resizable={} visible={} title={:?}",
        window.is_decorated().unwrap_or_default(),
        window.is_resizable().unwrap_or_default(),
        window.is_visible().unwrap_or_default(),
        window.title().unwrap_or_default(),
    );

    effects::apply(hwnd, dark);
    effects::apply_rounded_corners(hwnd);
    // 失焦/聚焦时 Windows 会发 WM_NCACTIVATE 重绘原生标题栏，必须拦截
    effects::install_focus_hook(hwnd);

    let app_for_events = app.clone();
    let window_for_events = window.clone();
    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::ThemeChanged(theme) => {
                println!("[effects] 系统主题切换 → {:?}", theme);
                effects::refresh_theme(hwnd, matches!(theme, Theme::Dark));
            }
            // SetWindowRgn 的区域不会随窗口缩放自动跟随，必须重设。
            // 但逐帧重建区域会让缩放卡顿 → 改为缩放中清区域、停止后恢复。
            tauri::WindowEvent::Resized(_) => {
                effects::begin_resize(hwnd);
            }
            // 焦点切换会触发 frame 重算，把裁剪区域冲掉（实测：聚焦后圆角消失）；
            // 另外窗口隐藏（托盘）/最小化再恢复时，DWM 可能把 accent 材质丢掉，
            // 表现为「回来变成一层没有毛玻璃的半透明」。两者都幂等，恢复时补刷一次。
            tauri::WindowEvent::Focused(focused) => {
                effects::apply_rounded_corners(hwnd);
                if *focused {
                    let dark =
                        matches!(window_for_events.theme().unwrap_or(Theme::Light), Theme::Dark);
                    effects::apply(hwnd, dark);
                }
            }
            // 跨屏拖动 DPI 变化后，半径要按新 DPI 重新换算
            tauri::WindowEvent::ScaleFactorChanged { .. } => {
                effects::apply_rounded_corners(hwnd);
            }
            // 「关闭时后台运行」：隐藏到托盘而不是退出（迁移自 v1 trayOnClose）
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if settings::get().tray_on_close {
                    api.prevent_close();
                    let _ = window_for_events.hide();
                    tray::create_tray(&app_for_events);
                    println!("[tray] 窗口已隐藏到托盘");
                }
                // 未开启托盘模式 → 放行关闭，退出清理在 RunEvent::Exit 统一处理
            }
            _ => {}
        }
    });

    // Win10 acrylic 在窗口刚创建时不一定立刻铺开，
    // 显示后补涂一次并 nudge 触发 DWM 重算（v1 里踩过：节流会吞掉必要的补涂）。
    std::thread::spawn(move || {
        for delay in [200u64, 600, 1200] {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            effects::apply(hwnd, dark);
            effects::apply_rounded_corners(hwnd);
            effects::nudge(hwnd);
        }
        println!("[effects] 补涂流程结束，最终档位：{}", effects::current_tier().as_str());
    });
}

pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            effect_info,
            set_material_alpha,
            set_round_mode,
            effects::drag_start,
            effects::resize_start,
            clipboard::get_history,
            clipboard::delete_item,
            clipboard::delete_items,
            clipboard::toggle_pin,
            clipboard::toggle_favorite,
            clipboard::clear_all,
            clipboard::get_image,
            clipboard::copy_back,
            clipboard::check_files_alive,
            clipboard::cache_info,
            clipboard::pick_cache_dir,
            clipboard::open_cache_dir,
            settings::get_settings,
            settings::set_tray_on_close,
            settings::set_clean_on_close,
            settings::set_clean_include_pinned,
            settings::set_clean_confirm_exit,
            settings::exit_app,
            settings::set_retention_days,
            settings::set_autostart,
            settings::set_file_copy,
            tray::tray_show_main,
            tray::tray_quit,
            tray::tray_menu_look,
        ]);

    #[cfg(windows)]
    let builder = builder.setup(|app| {
        settings::init(app.handle());
        clipboard::init(app.handle());
        setup_effects(app.handle());
        // 托盘图标常驻：程序一运行就显示（不再依赖「关闭时后台运行」开关）
        tray::create_tray(app.handle());
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
        }
        Ok(())
    });

    // 退出前统一清理：勾选「关闭时清理缓存」时清空内容缓存与历史（同 v1 before-quit）
    match builder.build(tauri::generate_context!()) {
        Ok(app) => {
            app.run(|_app, event| {
                if let tauri::RunEvent::Exit = event {
                    clipboard::on_app_exit();
                }
            });
        }
        Err(e) => panic!("轻贴启动失败：{e}"),
    }
}
