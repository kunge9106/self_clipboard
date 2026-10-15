//! 托盘：**常驻图标** + **自绘右键菜单**。
//!
//! 1) 常驻：只要程序在运行，托盘图标就在。旧实现是「勾选关闭时后台运行 + 关闭窗口」
//!    时才创建，取消勾选就整个移除——用户看到的托盘时有时无。
//!    「关闭时后台运行」这个开关现在只决定「点关闭按钮」的行为（隐藏到托盘 / 直接退出）。
//!
//! 2) 自绘菜单：系统原生托盘菜单（`TrackPopupMenu`）的字体、配色、圆角都不可控，
//!    跟软件本体格格不入。这里改成弹一个无边框透明小窗口 `tray-menu`，
//!    页面是 `tray.html`，用**主界面同一套 CSS 变量**渲染，并施加与主窗口相同的
//!    acrylic 材质，所以观感与本体一致。
//!
//! 菜单窗口**懒创建**：只有第一次右键托盘时才建，之后只显示/隐藏。
//! 这样不右键就完全不占第二个 WebView2 窗口的内存。

use std::sync::Mutex;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

static TRAY: Mutex<Option<tauri::tray::TrayIcon>> = Mutex::new(None);

const MENU_LABEL: &str = "tray-menu";
/// 贴合内容宽度：6px 菜单内边距×2 + 10px 项内边距×2 + 14px 图标 + 8px 间距
/// + 「显示轻贴」4 个汉字（13px 字号 ≈ 52px）+ 2px 描边 ≈ 108px，留 4px 余量。
/// 以后菜单项变多/变长，把这里加回去即可。
const MENU_W: f64 = 112.0;
/// 两个 34px 菜单项 + 6px 上下内边距 + 2px 描边
const MENU_H: f64 = 84.0;
/// 菜单与鼠标（托盘图标）之间的间隙
const MENU_GAP: f64 = 8.0;

pub fn create_tray(app: &AppHandle) {
    let mut guard = TRAY.lock().unwrap();
    if guard.is_some() {
        return;
    }

    // 不挂原生 menu：右键由 open_menu 自己弹窗口
    let builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("轻贴 — 剪贴板管理器")
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Up,
                position,
                ..
            } => open_menu(tray.app_handle(), position),
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => show_main(tray.app_handle()),
            _ => {}
        });

    let built = match app.default_window_icon().cloned() {
        Some(icon) => builder.icon(icon).build(app),
        None => builder.build(app),
    };
    match built {
        Ok(t) => {
            *guard = Some(t);
            crate::log_print!("[tray] 托盘图标已常驻");
        }
        Err(e) => crate::log_warn!("[tray] 创建托盘失败：{e}"),
    }
}

// ---------------------------------------------------------------------------
// 菜单窗口
// ---------------------------------------------------------------------------

fn ensure_menu_window(app: &AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(MENU_LABEL) {
        return Some(w);
    }

    let mut b = WebviewWindowBuilder::new(app, MENU_LABEL, WebviewUrl::App("tray.html".into()))
        .title("轻贴菜单")
        .inner_size(MENU_W, MENU_H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false);

    // 必须与主窗口用**同一套**浏览器参数：WebView2 只有在「同一 user data 目录 +
    // 完全相同的一组参数」下才会共享同一个浏览器进程，否则会另起一个进程组，
    // 内存直接翻倍。这里直接复用主窗口的那串配置，避免两处漂移。
    if let Some(args) = app
        .config()
        .app
        .windows
        .first()
        .and_then(|w| w.additional_browser_args.clone())
    {
        b = b.additional_browser_args(&args);
    }

    let win = match b.build() {
        Ok(w) => w,
        Err(e) => {
            crate::log_warn!("[tray] 创建托盘菜单窗口失败：{e}");
            return None;
        }
    };

    // 失焦即收起（点别处 = 关菜单）；菜单没有关闭按钮，被要求关闭时也只隐藏
    let w = win.clone();
    win.on_window_event(move |e| match e {
        tauri::WindowEvent::Focused(false) => {
            let _ = w.hide();
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = w.hide();
        }
        _ => {}
    });

    Some(win)
}

fn open_menu(app: &AppHandle, pos: PhysicalPosition<f64>) {
    let Some(win) = ensure_menu_window(app) else {
        return;
    };

    // 托盘通常在任务栏上（多在屏幕下缘）→ 菜单弹在光标**上方**；再按显示器工作区夹一下，
    // 保证不会跑到屏幕外（多屏 / 负坐标显示器也算对）。
    let (mut x, mut y) = (pos.x, pos.y - MENU_H - MENU_GAP);
    if let Ok(Some(mon)) = app.monitor_from_point(pos.x, pos.y) {
        let wa = mon.work_area();
        let (ax, ay) = (wa.position.x as f64, wa.position.y as f64);
        let (aw, ah) = (wa.size.width as f64, wa.size.height as f64);
        if y < ay + 4.0 {
            // 上面放不下（例如任务栏在屏幕顶部）→ 翻到光标下方
            y = pos.y + MENU_GAP;
        }
        x = x.min(ax + aw - MENU_W - 4.0).max(ax + 4.0);
        y = y.min(ay + ah - MENU_H - 4.0).max(ay + 4.0);
    }

    let _ = win.set_position(PhysicalPosition::new(x, y));

    // 主窗口的材质是「显示之后」才铺得开（同 setup_effects 里的补涂），所以这里
    // 先 show 再上材质；菜单每次弹出都重刷一次，顺带跟随系统主题变化。
    let _ = win.show();
    let _ = win.set_focus();
    apply_material(&win);

    // 菜单页是常驻的，只在加载时读一次配置的话，用户改完「材质浓度 / 圆角模式」再右键
    // 还是旧的观感。所以每次弹出都把**当前**外观参数随事件推给页面。
    // 参数由 Rust 直接算好放在 payload 里（不让页面再发两次 IPC 回来查），
    // 页面收到即同步应用，基本不会出现旧样式闪一下。
    // 首次弹出时页面可能还没加载完、事件会丢——但那种情况下页面刚好在做首次读取，
    // 读到的本来就是最新值，没有影响。
    let _ = app.emit_to(MENU_LABEL, "tray-menu-open", look(&win));
}

/// 托盘菜单需要的外观参数（与主窗口同一份配置，保证观感一致）
#[derive(Clone, serde::Serialize)]
pub struct MenuLook {
    /// 没有真材质（solid 兜底档）→ 菜单必须给不透明底色
    solid: bool,
    /// 菜单**恒为圆角**：圆角是托盘菜单的固有外观，不跟随主窗口的「圆角模式」。
    /// Rust 侧的 `apply_material` 走 `effects::apply_rounded`，已把系统材质关干净，
    /// 所以这里给 CSS 圆角不会有四角残留。
    round: bool,
    /// 材质浓度 → --glass-alpha（0–1）
    alpha: f64,
    dark: bool,
}

fn look(win: &WebviewWindow) -> MenuLook {
    MenuLook {
        solid: crate::effects::current_tier().as_str() == "solid",
        round: true,
        // 菜单恒用**圆角模式**的样式与材质（DESIGN 7.2）：浓度固定取圆角模式的默认档 90%，
        // 不跟随设置页里调的浓度 —— 菜单是窄条、文字密度高，需要稳定厚实的底。
        alpha: crate::settings::ROUND_ALPHA as f64 / 100.0,
        dark: matches!(win.theme().unwrap_or(tauri::Theme::Light), tauri::Theme::Dark),
    }
}

/// 菜单页首次加载时拉一次当前外观参数
#[tauri::command]
pub fn tray_menu_look(app: AppHandle) -> Result<MenuLook, String> {
    let win = app.get_webview_window(MENU_LABEL).ok_or("菜单窗口不存在")?;
    Ok(look(&win))
}

#[cfg(windows)]
fn apply_material(win: &WebviewWindow) {
    use tauri::Theme;
    if let Ok(h) = win.hwnd() {
        let dark = matches!(win.theme().unwrap_or(Theme::Light), Theme::Dark);
        // 菜单**固定圆角**（与主窗口的圆角模式无关），所以必须走「先关系统材质」的
        // 路径：系统 acrylic/backdrop 是矩形，CSS 圆角配上它四角会留一圈直角残留。
        crate::effects::apply_rounded(h.0 as isize, dark);
    }
}

#[cfg(not(windows))]
fn apply_material(_win: &WebviewWindow) {}

// ---------------------------------------------------------------------------
// 前端命令（菜单项）
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn tray_show_main(app: AppHandle) -> Result<(), String> {
    show_main(&app);
    Ok(())
}

#[tauri::command]
pub fn tray_quit(app: AppHandle) -> Result<(), String> {
    crate::log_print!("[tray] 用户从托盘菜单退出");
    crate::clipboard::on_app_exit();
    app.exit(0);
    Ok(())
}

/// 把主窗口唤回前台。托盘菜单与「重复启动」都用它，
/// 所以「补齐材质 + relayout」这一套只此一份，不会两处漂移。
pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        // 所有「补齐」动作都必须在窗口可见**之前**做完。显示之后再改材质或尺寸，
        // 用户会看到「先出现、再变样」的闪烁（上一版就是这么闪的）。
        #[cfg(windows)]
        {
            use tauri::Theme;
            if let Ok(h) = w.hwnd() {
                let hwnd = h.0 as isize;
                // 隐藏期间 DWM 会把 accent 材质丢掉 —— 先补回来
                let dark = matches!(w.theme().unwrap_or(Theme::Light), Theme::Dark);
                crate::effects::apply(hwnd, dark);
                // hide() 不产生 WM_SIZE，WebView2 的内容尺寸可能停在隐藏前的大小
                // （表现为唤回来排版不对）。此刻窗口还不可见，尺寸微调看不见。
                crate::effects::relayout(hwnd);
            }
        }

        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();

        // 隐藏期间设置的 DWM 属性不一定粘得住：窗口重新可见时 DWM 会重算
        // NC 渲染 / accent，刚设置的值可能被丢掉（实测 Win10/Win11 都会出现
        // 托盘唤回后四周围上一圈 Win7 式边框）。这与启动时「创建后补涂」是
        // 同一批问题 —— 显示之后再延迟补涂两次。
        // 延迟放后台线程：立即补会跟 show 抢帧（先出现、再变样）；
        // 这里刻意不做 relayout，窗口已可见，动尺寸会闪。
        #[cfg(windows)]
        {
            let w2 = w.clone();
            std::thread::spawn(move || {
                use tauri::Theme;
                for delay in [150u64, 500] {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    if let Ok(h) = w2.hwnd() {
                        let dark = matches!(w2.theme().unwrap_or(Theme::Light), Theme::Dark);
                        crate::effects::apply(h.0 as isize, dark);
                        crate::effects::apply_rounded_corners(h.0 as isize);
                        crate::effects::nudge(h.0 as isize);
                    }
                }
            });
        }
    }
}
