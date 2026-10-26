//! 弹出式小窗口的公共骨架：托盘菜单与快捷面板。
//!
//! 两者形态一致（无边框、透明、置顶、不进任务栏、失焦即收起），差别只在内容和
//! 出现位置。这一层只放「创建方式」和「坐标钳制」—— 两件事都踩过坑：
//!
//! - **浏览器参数必须与主窗口完全一致**：WebView2 只有在「同一 user data 目录 +
//!   同一组参数」下才共享浏览器进程，否则会另起一个进程组，内存直接翻倍。
//! - **坐标要按显示器工作区钳制**：多屏、负坐标、任务栏在顶部都会让「贴光标弹出」
//!   跑到屏幕外。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// 快捷面板的窗口 label。
///
/// 放在这一层而不是各自 `const` 一份：失焦策略要按 label 区分（见 `closes_on_blur`），
/// 两边各写一个 `"panel"` 字面量，哪天改名字就会出现「策略认不出这是面板」的静默失效。
pub const PANEL_LABEL: &str = "panel";

/// 各弹出窗口「最近一次真正显示」的时刻。
///
/// 为什么需要：窗口刚 `show` 出来时，焦点会抖一下（系统先把焦点给同进程的其它窗口，
/// 我们再去抢）。若失焦就立刻收起，表现为「面板闪一下就没了」——实测踩过。
/// 显示后短时间内忽略失焦事件即可。
static SHOWN_AT: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());

/// 标记某窗口「刚刚显示」，宽限期内的失焦事件不处理。
pub fn mark_shown(label: &'static str) {
    if let Ok(mut g) = SHOWN_AT.lock() {
        g.retain(|(l, _)| *l != label);
        g.push((label, Instant::now()));
    }
}

fn in_grace(label: &str) -> bool {
    SHOWN_AT.lock().ok().is_some_and(|g| {
        g.iter()
            .any(|(l, t)| *l == label && t.elapsed() < Duration::from_millis(450))
    })
}

/// 按标准形态创建一个弹出窗口；已存在则直接返回。
///
/// `url` 是 `WebviewUrl::App` 的相对路径（如 `"tray.html"`）。创建失败返回 `None`，
/// 调用方自行决定要不要提示用户 —— 这里只记日志，因为失败原因基本只有资源耗尽。
pub fn ensure(app: &AppHandle, label: &'static str, url: &str, title: &str, w: f64, h: f64) -> Option<WebviewWindow> {
    if let Some(win) = app.get_webview_window(label) {
        return Some(win);
    }

    let mut b = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(w, h)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false);

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
            crate::log_warn!("[popup] 创建窗口 {label} 失败：{e}");
            return None;
        }
    };

    // 失焦即收起：点别处 = 关掉。没有关闭按钮，被要求关闭时也只隐藏，
    // 下次唤出不必重建 WebView（重建要多几百毫秒）。
    //
    // 宽限期内的失焦要忽略：窗口刚显示时焦点会先被同进程的其它窗口抢一下，
    // 不放宽就会「闪一下就消失」。
    let w2 = win.clone();
    win.on_window_event(move |e| match e {
        tauri::WindowEvent::Focused(false) => {
            if !in_grace(label) && closes_on_blur(label) {
                let _ = w2.hide();
            }
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = w2.hide();
        }
        _ => {}
    });

    Some(win)
}

/// 某窗口失焦时是否收起。
///
/// 托盘菜单**永远**收起：菜单的语义就是「点别处即关闭」，留着不走反而挡住它下面
/// 那块界面。快捷面板交给用户决定（`panel_stay_on_blur`）—— 见那个字段的注释。
///
/// 每次事件现读配置而不是缓存：这个开关随时可能在设置页被改，缓存就得同步失效，
/// 而读一次配置只是一把锁 + 一次 clone，代价远小于「改完要重启」的体验损失。
fn closes_on_blur(label: &str) -> bool {
    if label == PANEL_LABEL {
        !crate::settings::get().panel_stay_on_blur
    } else {
        true
    }
}

/// 把窗口摆到锚点附近，并钳制在锚点所在显示器的工作区内。
///
/// `above = true` 时优先摆在锚点上方（托盘菜单：托盘多在屏幕下缘），放不下就翻到下方；
/// `false` 时优先摆在下方（面板：跟随鼠标，往下打开更符合习惯）。
/// 返回最终的物理坐标，调用方直接 `set_position`。
pub fn place(
    app: &AppHandle,
    phys_w: f64,
    phys_h: f64,
    anchor: PhysicalPosition<f64>,
    above: bool,
    gap: f64,
) -> (f64, f64) {
    let (mut x, mut y) = if above {
        (anchor.x, anchor.y - phys_h - gap)
    } else {
        (anchor.x, anchor.y + gap)
    };

    if let Ok(Some(mon)) = app.monitor_from_point(anchor.x, anchor.y) {
        let wa = mon.work_area();
        let (ax, ay) = (wa.position.x as f64, wa.position.y as f64);
        let (aw, ah) = (wa.size.width as f64, wa.size.height as f64);
        if above && y < ay + 4.0 {
            // 上面放不下（例如任务栏在屏幕顶部）→ 翻到锚点下方
            y = anchor.y + gap;
        } else if !above && y + phys_h > ay + ah - 4.0 {
            // 下面放不下 → 翻到锚点上方
            y = anchor.y - phys_h - gap;
        }
        // 先 min 再 max：窗口比工作区还大时（极端高缩放），顺序反了会把坐标推到负值
        x = x.min(ax + aw - phys_w - 4.0).max(ax + 4.0);
        y = y.min(ay + ah - phys_h - 4.0).max(ay + 4.0);
    }
    (x, y)
}
