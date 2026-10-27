//! 低占用模式：主窗口闲置一段时间后释放它的 WebView。
//!
//! 轻贴是托盘常驻应用，窗口绝大部分时间不可见，而隐藏时 WebView2 的整组
//! Chromium 进程（主进程 / 渲染 / GPU / 网络 / 存储）仍然全在跑 —— 实测
//! 411 MB 里有 373 MB 属于它们，自己的 Rust 代码只占 38 MB。
//! 闲置时把主窗口销毁掉，只留托盘与后端线程，就能回到 40 MB 上下。
//!
//! # 为什么用 `destroy()` 而不是 `close()`
//!
//! `close()` 会走 close-requested 流程，而轻贴在那个流程里挂了
//! 「关闭时后台运行」的 `prevent_close()`，所以 `close()` 永远关不掉。
//! `destroy()` 直接移除窗口，不走该流程 —— Tauri 文档也是这么要求的。
//!
//! # 销毁后为什么不会退出应用
//!
//! Tauri 在 Windows/Linux 上的默认行为是「最后一个窗口关闭即退出」。
//! 销毁主窗口会触发 `RunEvent::ExitRequested`，若那时不调用
//! `prevent_exit()` 应用就真退出了。是否阻止由 `is_released()` 决定：
//! 只有我们自己释放的情况才拦，用户主动退出必须放行（见 `lib.rs`）。
//!
//! # 重建为什么先隐藏再显示
//!
//! 照 Clash Verge Rev 的做法：`visible(false)` 建窗，等
//! `PageLoadEvent::Finished` 再 `show()`。直接建就显示会闪一下白屏或
//! 错误主题色，因为页面还没渲染完。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

/// 主窗口 label。
pub(crate) const MAIN_LABEL: &str = "main";

/// 主窗口当前是否处于「已释放」状态。
///
/// `RunEvent::ExitRequested` 靠它区分两种关闭：
/// 真值 = 我们自己销毁的，应用要继续跑；假值 = 用户要退出，放行。
static RELEASED: AtomicBool = AtomicBool::new(false);

/// 计时器代次。每次重新计时 +1，旧线程醒来发现代次变了就放弃，
/// 比「取消一个定时器」简单，也不必维护线程句柄。
static GEN: AtomicU64 = AtomicU64::new(0);

/// 释放前记下的窗口几何（x, y, width, height，逻辑像素）。
///
/// 项目没有窗口状态持久化（未用 window-state 插件），不记下来重建后
/// 窗口会跳回 `tauri.conf.json` 里的默认位置 —— 用户调过的位置就丢了。
static GEOM: Mutex<Option<(f64, f64, f64, f64)>> = Mutex::new(None);

/// 空闲多久后释放（秒）。0 = 从不释放。
fn idle_secs() -> u64 {
    crate::settings::get().release_idle_secs as u64
}

/// 窗口转入隐藏：按设置启动释放倒计时。
pub(crate) fn note_hidden(app: tauri::AppHandle) {
    let secs = idle_secs();
    if secs == 0 {
        return;
    }
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        // 期间窗口又显示过（代次变了）就放弃
        if GEN.load(Ordering::SeqCst) != gen {
            return;
        }
        // 用户可能在这段时间里把开关关掉了
        if idle_secs() == 0 {
            return;
        }
        release_now(&app);
    });
    crate::log_print!("[idle] 窗口已隐藏，{secs} 秒后释放界面");
}

/// 窗口转入显示：取消释放倒计时，必要时重建窗口。
pub(crate) fn note_shown() {
    GEN.fetch_add(1, Ordering::SeqCst);
}

/// 立即释放：销毁主窗口，只留后端与托盘。
pub(crate) fn release_now(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    // 不释放仍可见的窗口：那是用户正在用的界面
    if w.is_visible().unwrap_or(false) {
        return;
    }
    save_geometry(&w);
    // destroy 不走 close-requested，不会被「关闭时后台运行」拦下
    let _ = w.destroy();
    RELEASED.store(true, Ordering::SeqCst);
    crate::log_print!("[idle] 界面已释放（WebView 进程退出）");
}

/// 主窗口是否已被释放。`ExitRequested` 与重建逻辑都看它。
pub(crate) fn is_released() -> bool {
    RELEASED.load(Ordering::SeqCst)
}

/// 确保主窗口存在；已被释放则重建。
///
/// 托盘点击、单例唤醒等唤起路径都应该走这里，而不是直接 `get_webview_window`。
pub(crate) fn ensure_window(app: &tauri::AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(MAIN_LABEL) {
        return Some(w);
    }
    if !is_released() {
        // 不是我们释放的（比如启动早期），别硬建
        return None;
    }

    let cfg = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_LABEL)?
        .clone();

    let win = WebviewWindowBuilder::from_config(app, &cfg)
        .ok()?
        // 先隐藏，等页面渲染完再显示，避免白屏与主题色闪一下
        .visible(false)
        .on_page_load(|w, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            let _ = w.show();
            let _ = w.set_focus();
        })
        .build()
        .ok()?;

    // 材质 / 圆角 / DPI / 焦点钩子 / 窗口事件监听，一次全部恢复
    crate::setup_effects(app);
    restore_geometry(&win);

    RELEASED.store(false, Ordering::SeqCst);
    crate::log_print!("[idle] 界面已重建");
    Some(win)
}

/// 记下窗口位置与大小。取不到就保持上次的值，不覆盖成 None。
fn save_geometry(w: &WebviewWindow) {
    let Ok(pos) = w.outer_position() else { return };
    let Ok(size) = w.inner_size() else { return };
    let scale = w.scale_factor().unwrap_or(1.0);
    let v = (
        pos.x as f64 / scale,
        pos.y as f64 / scale,
        size.width as f64 / scale,
        size.height as f64 / scale,
    );
    if let Ok(mut g) = GEOM.lock() {
        *g = Some(v);
    }
}

/// 按记下的几何摆回窗口。
fn restore_geometry(w: &WebviewWindow) {
    let v = match GEOM.lock() {
        Ok(g) => match *g {
            Some(v) => v,
            None => return,
        },
        Err(_) => return,
    };
    let _ = w.set_position(tauri::LogicalPosition::new(v.0, v.1));
    let _ = w.set_size(tauri::LogicalSize::new(v.2, v.3));
}
