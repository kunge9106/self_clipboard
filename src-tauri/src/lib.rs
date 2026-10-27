use tauri::Theme;

#[cfg(windows)]
use tauri::Manager;

#[cfg(windows)]
mod effects;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod singleton;
#[cfg(windows)]
mod about;
mod settings;
mod autostart;
mod tray;
mod logger;
mod hotkey;
mod popup;
mod panel;
mod input;
mod screenclip;
mod screenshot;
#[cfg(windows)]
mod purge;

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

/// 圆角模式开关：圆角 + 不用系统 blur（四角真透明、100% 干净），立即生效。
///
/// **只切模式本身，浓度一个字节都不动** —— 两个模式各记着自己那一档浓度
/// （`alpha_straight` / `alpha_round`，见 settings.rs），切模式只是换用另一档：
/// 直角下调到 68 再切圆角（用圆角那档，比如 45），切回直角还是 68。
/// 从前两个模式共用同一个值，切一次就被覆盖一次，用户调好的浓度存不下来。
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

/// 材质浓度（设置页滑块）：更新 tint alpha 并立即重刷亚克力。
///
/// 只写入**当前模式**那一档，另一个模式记住的值不受影响。
#[tauri::command]
fn set_material_alpha(app: tauri::AppHandle, value: u32) -> Result<(), String> {
    settings::update(|c| c.set_alpha(value));
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

/// 启动横幅：一行把「跑的是哪个版本、哪个 exe、数据在哪」钉进日志。
///
/// 用户报障时最常需要确认的就是这三件事：机器上装了两份（Program Files 与
/// debug 目录）、版本对不上、数据其实在另一个缓存目录。没有这条横幅，
/// 只能靠来回追问；有了它，用户把日志一贴就能定位。
fn log_startup_banner() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(取不到)".into());
    let dir = clipboard::cache_info().dir;
    crate::log_print!(
        "===== 轻贴 v{} 启动 | 构建={} | exe={} | 数据目录={} =====",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "debug" } else { "release" },
        exe,
        if dir.trim().is_empty() { "(未初始化)" } else { dir.as_str() }
    );
}

#[cfg(windows)]
fn setup_effects(app: &tauri::AppHandle) {
    // —— 诊断：确认运行时配置与窗口实际状态是否一致 ——
    let cfg_win = &app.config().app.windows;
    crate::log_print!("[debug] config windows = {:?}", cfg_win);

    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => {
            crate::log_warn!("[effects] 找不到 main 窗口，跳过材质设置");
            return;
        }
    };

    // 先按当前显示器把窗口夹进工作区，再套材质/圆角 —— 否则高缩放下窗口按
    // 1060×690 铺开、超出屏幕，圆角区域也会按错的尺寸算一遍。
    effects::apply_dpi_policy(&window);
    // 主题要在页面真正渲染前钉好，否则系统深色 + 用户选浅色时会先闪一下深色
    effects::apply_theme_mode(&window);

    let hwnd = match window.hwnd() {
        Ok(h) => h.0 as isize,
        Err(e) => {
            crate::log_warn!("[effects] 获取 hwnd 失败：{}，按 solid 处理", e);
            return;
        }
    };

    let dark = matches!(window.theme().unwrap_or(Theme::Light), Theme::Dark);

    // —— 诊断：窗口运行时状态 ——
    crate::log_print!(
        "[debug] runtime: decorated={} resizable={} visible={} title={:?}",
        window.is_decorated().unwrap_or_default(),
        window.is_resizable().unwrap_or_default(),
        window.is_visible().unwrap_or_default(),
        window.title().unwrap_or_default(),
    );

    effects::apply(hwnd, dark);
    effects::apply_rounded_corners(hwnd);
    // 失焦/聚焦时 Windows 会发 WM_NCACTIVATE 重绘原生标题栏，必须拦截
    effects::install_focus_hook(hwnd, app);

    let app_for_events = app.clone();
    let window_for_events = window.clone();
    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::ThemeChanged(theme) => {
                crate::log_print!("[effects] 系统主题切换 → {:?}", theme);
                effects::refresh_theme(hwnd, matches!(theme, Theme::Dark));
            }
            // SetWindowRgn 的区域不会随窗口缩放自动跟随，必须重设。
            // 但逐帧重建区域会让缩放卡顿 → 改为缩放中清区域、停止后恢复。
            tauri::WindowEvent::Resized(_) => {
                effects::begin_resize(hwnd);
            }
            // 焦点切换会触发 frame 重算，把裁剪区域冲掉（实测：聚焦后圆角消失）。
            tauri::WindowEvent::Focused(_active) => {
                effects::apply_rounded_corners(hwnd);
                // 实测（Win10/Win11 都有）：失焦/聚焦偶发把材质与 NC 渲染一起丢掉，
                // 四周围上一圈 Win7 式边框，而托盘唤回路径的补涂管不到这里。
                // 同步重刷会肉眼可见地闪（聚焦是高频动作），所以延迟 150ms 在后台
                // 补一次；apply 幂等，连续触发也只是多跑几遍。
                let w2 = window_for_events.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    #[cfg(windows)]
                    {
                        use tauri::Theme;
                        if let Ok(h) = w2.hwnd() {
                            let dark =
                                matches!(w2.theme().unwrap_or(Theme::Light), Theme::Dark);
                            effects::apply(h.0 as isize, dark);
                            effects::apply_rounded_corners(h.0 as isize);
                            effects::nudge(h.0 as isize);
                        }
                    }
                    #[cfg(not(windows))]
                    drop(w2);
                });
            }
            // 跨屏拖动 DPI 变化后，半径要按新 DPI 重新换算；
            // 同时窗口可能已经超出新显示器的工作区（缩放变了、可用逻辑分辨率也变了），
            // 一并夹一次，否则底部会被切掉。
            tauri::WindowEvent::ScaleFactorChanged { .. } => {
                effects::apply_dpi_policy(&window_for_events);
                effects::apply_rounded_corners(hwnd);
            }
            // 「关闭时后台运行」：隐藏到托盘而不是退出（迁移自 v1 trayOnClose）
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if settings::get().tray_on_close {
                    api.prevent_close();
                    let _ = window_for_events.hide();
                    tray::create_tray(&app_for_events);
                    crate::log_print!("[tray] 窗口已隐藏到托盘");
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
        crate::log_print!("[effects] 补涂流程结束，最终档位：{}", effects::current_tier().as_str());
    });
}

pub fn run() {
    // panic 钩子最先装上：这样连「单实例检测阶段的崩溃」也能落进日志。
    logger::install_hook();
    // 紧接着告诉日志系统配置文件在哪 —— 下面这两条日志（单实例、启动横幅）
    // 都发生在 `setup` 之前，那时缓存目录还没初始化。不知道 `cache_dir` 的话，
    // 它们会落到 %LOCALAPPDATA% 下的另一份 log 里，「启动即崩溃」的现场就被劈成两半。
    if let Some(p) = settings::default_config_path() {
        logger::hint_config_file(p);
    }

    // 单实例：同一版本只允许一个实例；测试版(debug)与正式版(release)锁名不同，可共存。
    // 检测到同版本已在运行 → **只给它发一条消息**，由它自己唤回窗口 + 弹应用内提示
    // （本进程绝不跨进程去动那个窗口，详见 singleton::notify_existing_instance）。
    // 必须排在日志钩子之后、其余初始化之前：既保证崩了有记录，
    // 又避免第二个实例为了写一行日志去抢剪贴板监听等资源。
    #[cfg(windows)]
    if !singleton::ensure_single_instance() {
        singleton::notify_existing_instance();
        std::process::exit(0);
    }

    logger::write_line(
        "INFO",
        "boot",
        &format!(
            "轻贴启动 v{}（debug={}）",
            about::version_display(),
            cfg!(debug_assertions)
        ),
    );

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            effect_info,
            set_material_alpha,
            set_round_mode,
            about::about_info,
            about::check_update,
            about::update_state,
            about::download_update,
            about::install_downloaded,
            about::open_external,
            effects::drag_start,
            effects::resize_start,
            clipboard::get_history,
            clipboard::copy_diagnostics,
            clipboard::delete_item,
            clipboard::delete_items,
            clipboard::toggle_pin,
            clipboard::toggle_favorite,
            clipboard::clear_all,
            clipboard::get_image,
            clipboard::get_image_thumb,
            clipboard::get_full_content,
            clipboard::set_clipboard_text,
            logger::frontend_log,
            clipboard::copy_back,
            clipboard::check_files_alive,
            clipboard::cache_info,
            clipboard::pick_cache_dir,
            clipboard::open_cache_dir,
            clipboard::open_log_dir,
            settings::get_settings,
            settings::dismiss_onboarding,
            settings::set_tray_on_close,
            settings::set_clean_on_close,
            settings::set_clean_include_pinned,
            settings::set_prune_include_pinned,
            settings::set_hotkey,
            settings::set_panel_stay_on_blur,
            settings::set_screenshot_hotkey,
            settings::set_screenshot_right_click_cancel,
            settings::set_screenshot_mode,
            screenshot::shot_image,
            screenshot::shot_show,
            screenshot::shot_windows,
            screenshot::shot_commit,
            screenshot::shot_cancel,
            settings::hotkey_suspend,
            settings::hotkey_resume,
            panel::panel_hide,
            panel::panel_paste,
            settings::set_clean_confirm_exit,
            settings::exit_app,
            settings::set_retention_days,
            settings::set_autostart,
            settings::set_file_copy,
            settings::set_delete_files_with_item,
            settings::set_delete_files_permanent,
            settings::set_pin_global,
            settings::set_pin_cancel_on_disable,
            settings::set_ignore_dpi_scale,
            settings::set_theme_mode,
            tray::tray_show_main,
            tray::tray_quit,
            tray::tray_menu_look,
        ]);

    #[cfg(windows)]
    let builder = builder.setup(|app| {
        // 先告诉日志系统配置文件在哪：这样「配置就绪」「实例锁」这些
        // 启动最早期（缓存目录还没确定）的日志也会落到用户真正在看的
        // 那个 log 目录里，而不是 %LOCALAPPDATA% 下的另一份。
        if let Ok(p) = app.path().app_data_dir() {
            logger::hint_config_file(p.join("config.json"));
        }
        settings::init(app.handle());
        clipboard::init(app.handle());
        log_startup_banner();
        // 自愈：旧版本把自启值按 UTF-8 写进宽字符 API，注册表里是乱码路径，
        // 开机启动失败。这里每次启动比对一次，内容不符就重写。
        autostart::repair_if_needed();
        setup_effects(app.handle());
        // 上次下载后用户取消了安装 → 包还在，保留；装完了 → 版本号已追平，删掉，
        // 免得「重新安装」一直挂着挡住检查更新。必须在 clipboard::init 之后
        // （要读缓存目录）。
        about::prune_stale_installers();
        // 托盘图标常驻：程序一运行就显示（不再依赖「关闭时后台运行」开关）
        tray::create_tray(app.handle());
        // 全局快捷键：独立线程跑消息循环，触发时复用托盘那套「唤回主窗口」
        hotkey::init(app.handle().clone());
        // 启动后自动检查一次更新：后台线程、静默 —— 有新版才发通知，
        // 检查失败或没有新版一律不打扰用户（前端其余情况只认手动点击）。
        about::auto_check(app.handle().clone());
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
        }
        // 预热截图覆盖层：首次截图现建 WebView 要 1–2 秒，用户会以为「按了没反应」
        // 甚至被兜底逻辑取消。只在内置模式下做（默认走系统截图时用不到覆盖层）；
        // 用户在设置里切到内置的那一次由 `set_screenshot_mode` 立刻补预热。
        screenshot::prewarm_delayed(app.handle().clone());
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
