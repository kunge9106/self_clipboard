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
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};

static TRAY: Mutex<Option<tauri::tray::TrayIcon>> = Mutex::new(None);

use crate::popup::MENU_LABEL;
/// 以下三个都是**逻辑单位**（CSS px）。`inner_size` 收的就是逻辑值，会按 DPI 放大；
/// 而托盘图标坐标、显示器工作区都是**物理像素** —— 两套单位在 `open_menu` 里
/// 必须显式换算，否则高缩放下菜单会偏半个身位（150% 差 42px），还会被错误地
/// 夹到屏幕外。
///
/// 让托盘菜单套用**与主窗口同一套** DPI 策略。
///
/// 为什么菜单也要管：开关打开（不跟随系统缩放）时主界面按物理像素渲染、字号恒定，
/// 而菜单窗口如果还按默认 zoom 走，就会跟着系统放大 —— 两个窗口放一起看，
/// 字号一个恒定一个变大，明显不是一个东西。菜单是个 112×84 的小窗口，差异更刺眼。
///
/// 返回菜单当前的**物理**像素尺寸，供 `open_menu` 定位时统一单位。
/// - 不跟随：webview 缩到 1/scale，窗口建成物理尺寸 → 逻辑/物理 1:1，返回值就是常量本身
/// - 跟随：常规行为，窗口是逻辑尺寸，物理值要乘 scale
fn menu_physical_size(win: &tauri::WebviewWindow) -> (f64, f64) {
    let scale = win.scale_factor().unwrap_or(1.0).max(0.1);
    if crate::settings::get().ignore_dpi_scale {
        let _ = win.set_zoom(1.0 / scale);
        let _ = win.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(
            MENU_W as u32,
            MENU_H as u32,
        )));
        (MENU_W, MENU_H)
    } else {
        let _ = win.set_zoom(1.0);
        let _ = win.set_size(tauri::Size::Logical(tauri::LogicalSize::new(MENU_W, MENU_H)));
        (MENU_W * scale, MENU_H * scale)
    }
}

/// 贴合内容宽度：6px 菜单内边距×2 + 10px 项内边距×2 + 14px 图标 + 8px 间距
/// + 「显示轻贴」4 个汉字（13px 字号 ≈ 52px）+ 2px 描边 ≈ 108px。
///
/// 余量给到 12px 而不是贴着算：不同 Windows 版本的中文字体宽度有出入（缺字形回退到
/// 别的字体时会更宽），算得太紧就会把 34px 高的菜单项挤成两行 —— 用户同事在
/// 150% 缩放下实测过。文字另加 `white-space: nowrap` 兜底（宁可贴边也不换行）。
/// 以后菜单项变多/变长，把这里加回去即可。
const MENU_W: f64 = 120.0;
/// 两个 34px 菜单项 + 2px 项间距 + 6px 上下内边距×2 + 2px 描边
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
            } => {
                // 与上面同理：回调线程里同步建窗口会死锁，扔到独立线程。
                // 窗口本来就存在时只是 show，走线程也看不出差别。
                let app = tray.app_handle().clone();
                std::thread::spawn(move || show_main(&app));
            }
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
    // 创建形态（无边框 / 透明 / 置顶 / 失焦收起 / 共享 WebView2 进程）与快捷面板
    // 完全一致，统一走 `popup::ensure` —— 那份注释里记着「浏览器参数必须与主窗口
    // 一致，否则内存翻倍」这个坑，只该存在于一处。
    crate::popup::ensure(app, MENU_LABEL, "tray.html", "轻贴菜单", MENU_W, MENU_H)
}

fn open_menu(app: &AppHandle, pos: PhysicalPosition<f64>) {
    let Some(win) = ensure_menu_window(app) else {
        return;
    };

    // 先套 DPI 策略并拿到菜单的**物理**尺寸 —— 结果与下面的 pos / work_area 同单位，
    // 直接相减即可，不再混用两套单位（150% 下曾偏半个身位，差约 42px）。
    let (mw, mh) = menu_physical_size(&win);
    let gap = MENU_GAP * win.scale_factor().unwrap_or(1.0).max(0.1);

    // 托盘通常在任务栏上（多在屏幕下缘）→ 菜单优先弹在光标**上方**，
    // 放不下会翻到下方；工作区钳制与多屏处理都在 `popup::place` 里。
    let (x, y) = crate::popup::place(app, mw, mh, pos, true, gap);
    let _ = win.set_position(PhysicalPosition::new(x, y));

    // 失焦瞬间 Windows 会重绘非客户区，菜单四周围一圈 Win7 式边框闪一下
    //（用户截图实锤）——挂上 WM_NCACTIVATE 拦截。菜单窗口是复用的，
    // 函数内部按 hwnd 防重装。
    #[cfg(windows)]
    if let Ok(h) = win.hwnd() {
        crate::effects::install_menu_nc_hook(h.0 as isize);
    }

    // 菜单是懒创建的，setup 时它还不存在 —— 每次弹出时按设置钉一次主题，
    // 免得主界面深色、菜单浅色（用户手动选了浅色但系统是深色时最容易撞上）。
    crate::effects::apply_theme_mode(&win);

    // 材质必须在 show **之前**上好：菜单窗口同样是 hide→show 复用，显示后再改
    // 属性用户会看到「先灰、再变样」（主窗口 show_main 同一顺序）。
    // 菜单每次弹出都重刷一次，顺带跟随系统主题与高级材质开关变化。
    apply_material(&win);
    let _ = win.show();
    crate::popup::mark_shown(MENU_LABEL);
    let _ = win.set_focus();

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
        // 恒为 100%：菜单是非高级材质的 CSS 实底（见 apply_material），
        // 实底不透明才压得住底下的桌面 —— 这就是 2.6.10 菜单的观感。
        alpha: 1.0,
        dark: crate::effects::effective_dark(win),
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
    // 菜单**恒为非高级材质**（2026-11-06 用户实测拍板）：系统 backdrop 在这个
    // 弹出式小窗口上铺不稳（实测有几率不生效），不值得为它引入第二套状态机。
    // 先关干净系统材质，观感完全交给 CSS —— 100% 浓度实底 + CSS 圆角，
    // 与 2.6.10 的菜单观感一致。
    if let Ok(h) = win.hwnd() {
        let dark = crate::effects::effective_dark(win);
        crate::effects::apply_rounded(h.0 as isize, dark);
    }
}

#[cfg(not(windows))]
fn apply_material(_win: &WebviewWindow) {}

// ---------------------------------------------------------------------------
// 前端命令（菜单项）
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn tray_show_main(app: AppHandle) -> Result<(), String> {
    // 为什么必须是 async + spawn_blocking：Windows 上**同步命令里建窗口会死锁**。
    // 窗口创建要把活派回主事件循环再等它完成，而同步命令本身就跑在会挡住
    // 事件循环的线程上 —— 自己等自己（Tauri 文档 WebviewWindowBuilder::build
    // 的 Known issues 明确写了这一条）。
    //
    // 死锁的后果远不止这一个按钮：整个事件循环被卡住，之后所有 IPC 都不再
    // 响应。实测现象正是「菜单两个按钮都没反应」—— 第一个把循环堵死了。
    tauri::async_runtime::spawn_blocking(move || {
        crate::log_print!("[tray] 菜单点了「打开轻贴」");
        let had = app.get_webview_window("main").is_some();
        show_main(&app);
        let now = app.get_webview_window("main").is_some();
        crate::log_print!("[tray] 打开轻贴完成：主窗口 之前={had} 之后={now}");
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn tray_quit(app: AppHandle) -> Result<(), String> {
    crate::log_print!("[tray] 用户从托盘菜单退出");
    crate::idle_release::note_exiting();
    // 用户主动退出 →「关闭时清理数据」才有机会生效
    crate::clipboard::note_user_quit();
    crate::clipboard::on_app_exit();
    app.exit(0);
    Ok(())
}

/// 把主窗口唤回前台。托盘菜单与「重复启动」都用它，
/// 所以「补齐材质 + relayout」这一套只此一份，不会两处漂移。
/// 「收进托盘」的驻留方式：窗口移到屏幕外，**不 hide**。
///
/// hide() 会让 DWM 把宿主窗口透明（blur-behind）、NC 渲染策略、backdrop
/// 一次性全丢，唤回的第一帧必然是灰的，任何补涂都追不上 DWM 的合成节奏。
/// 屏幕外驻留时窗口对 DWM 依然可见，材质状态一帧都不会丢 ——
/// 唤回只是移回原位，这就是 2.6.10 式的丝滑（它靠的是「没材质所以看不出丢」，
/// 现在直接让状态不丢）。
///
/// 任务栏条目用 `set_skip_taskbar` 摘掉：tao 走的是 ITaskbarList
/// DeleteTab/AddTab，不改窗口样式、不闪、不影响可见性。
static PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PARK_POS: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

/// 收进托盘：记住当前位置 → 摘掉任务栏条目 → 移出屏幕。
/// 顺带把 z 序压到底并清掉线程焦点，别让键盘打进一个看不见的窗口。
pub fn park_main_window(w: &tauri::WebviewWindow) {
    use std::sync::atomic::Ordering;
    if let Ok(p) = w.outer_position() {
        if let Ok(mut g) = PARK_POS.lock() {
            *g = Some((p.x, p.y));
        }
    }
    let _ = w.set_skip_taskbar(true);
    let _ = w.set_position(tauri::PhysicalPosition::new(-32000, -32000));
    PARKED.store(true, Ordering::Relaxed);
    #[cfg(windows)]
    unsafe {
        extern "system" {
            fn SetWindowPos(
                hwnd: isize,
                after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
            fn SetFocus(hwnd: isize) -> isize;
        }
        if let Ok(h) = w.hwnd() {
            let hwnd = h.0 as isize;
            const HWND_BOTTOM: isize = 1;
            const SWP_NOMOVE: u32 = 0x2;
            const SWP_NOSIZE: u32 = 0x1;
            const SWP_NOACTIVATE: u32 = 0x10;
            SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            SetFocus(0);
        }
    }
}

/// 唤回：若窗口正驻留屏幕外则移回原位，返回 true（调用方不必再做任何材质复位）。
/// 没有驻留（最小化、刚重建等）返回 false，走常规路径。
fn unpark_main_window(w: &tauri::WebviewWindow) -> bool {
    use std::sync::atomic::Ordering;
    if !PARKED.swap(false, Ordering::Relaxed) {
        return false;
    }
    let pos = PARK_POS.lock().ok().and_then(|mut g| g.take());
    let _ = w.set_skip_taskbar(false);
    let (x, y) = pos.unwrap_or((120, 120)); // 位置没存到也必须移回来，给个保底
    let _ = w.set_position(tauri::PhysicalPosition::new(x, y));
    true
}

/// 窗口被销毁重建时（低占用释放后），驻留状态作废。
fn clear_park_state() {
    use std::sync::atomic::Ordering;
    PARKED.store(false, Ordering::Relaxed);
    if let Ok(mut g) = PARK_POS.lock() {
        *g = None;
    }
}

pub fn show_main(app: &AppHandle) {
    // 窗口要显示了：取消释放倒计时（用户正在用，不能被销毁）
    crate::idle_release::note_shown();
    // 低占用模式下窗口可能已被释放：先确保它存在，再走补齐流程
    let (w, just_built) = match app.get_webview_window("main") {
        Some(w) => (w, false),
        None => match crate::idle_release::ensure_window(app) {
            Some(w) => (w, true),
            None => return,
        },
    };
    // 刚重建的窗口页面还没加载完（`visible(false)` 建的，等 PageLoadEvent::Finished
    // 才由回调 show）。这里再 show 一次会显示一块空白区域 —— 用户看到的就是
    // 「点了没反应」或一片白。所以重建这条路直接交给回调，不在这里补。
    if just_built {
        // 新窗口是全新建的，驻留状态作废（skipTaskbar 默认就是 false）
        clear_park_state();
        crate::log_print!("[tray] 界面刚重建，交给页面加载完成后显示");
        return;
    }
    // 驻留恢复：窗口对 DWM 从未消失，材质一帧没丢 —— 直接移回原位 + 聚焦。
    // 这是「打开就是高级材质」的关键：任何复位都不需要，也不该做。
    if unpark_main_window(&w) {
        crate::log_print!("[tray] 从屏幕外驻留恢复（DWM 状态未丢，无需复位）");
        let _ = w.set_focus();
        // 保险：静默补涂一次（值没变时是纯 no-op，不闪）
        #[cfg(windows)]
        {
            let _ = crate::effects::reapply_visuals_for(&w, "unpark");
        }
        return;
    }
    {
        // 所有「补齐」动作都必须在窗口可见**之前**做完。显示之后再改材质或尺寸，
        // 用户会看到「先出现、再变样」的闪烁（上一版就是这么闪的）。
        #[cfg(windows)]
        {
            if let Ok(h) = w.hwnd() {
                let hwnd = h.0 as isize;
                // 隐藏期间 DWM 会丢掉 backdrop —— 先整份复位。
                // 单靠 apply_with 不够：它样式位没变就早退，不发 FRAMECHANGED。
                // 而且 hide→show 后 DWM 还会把 NC 渲染策略重置回 USEWINDOWSTYLE，
                // 那一项不补，backdrop 属性在也画不出来（见 restore_visuals 文档）。
                // restore_visuals_for = DWM 层 + WebView2 内容层，
                // 少任何一层都会变成「属性全对、画面全灰」
                let _ = crate::effects::restore_visuals_for(&w, "tray-show");
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
                for delay in [150u64, 500] {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    // 可见期间的兜底补涂走**静默**版：不拆 backdrop 重建。
                    // 旧版不闪正是这个语义；响亮版在这里跑就是那一下闪
                    let _ = crate::effects::reapply_visuals_for(&w2, "tray-show-delayed");
                }
            });
        }
    }
}
