use tauri::Theme;

#[cfg(windows)]
use tauri::Manager;

#[cfg(windows)]
mod effects;

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
        })
    }
    #[cfg(not(windows))]
    {
        serde_json::json!({ "tier": "solid", "build": 0, "maxRadius": 12 })
    }
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

    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::ThemeChanged(theme) => {
                println!("[effects] 系统主题切换 → {:?}", theme);
                effects::refresh_theme(hwnd, matches!(theme, Theme::Dark));
            }
            // SetWindowRgn 的区域不会随窗口缩放自动跟随，必须重设
            tauri::WindowEvent::Resized(_) => {
                effects::apply_rounded_corners(hwnd);
            }
            // 焦点切换会触发 frame 重算，把裁剪区域冲掉（实测：聚焦后圆角消失）
            tauri::WindowEvent::Focused(_) => {
                effects::apply_rounded_corners(hwnd);
            }
            // 跨屏拖动 DPI 变化后，半径要按新 DPI 重新换算
            tauri::WindowEvent::ScaleFactorChanged { .. } => {
                effects::apply_rounded_corners(hwnd);
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
    let builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![effect_info]);

    #[cfg(windows)]
    let builder = builder.setup(|app| {
        setup_effects(app.handle());
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
        }
        Ok(())
    });

    builder
        .run(tauri::generate_context!())
        .expect("轻贴启动失败");
}
