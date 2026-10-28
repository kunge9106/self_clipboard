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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

/// 主窗口 label。
pub(crate) const MAIN_LABEL: &str = "main";

// ── 三态状态机 ───────────────────────────────────────────────────────────
//
// 用 `AtomicU8` + CAS 而不是一个布尔量：释放倒计时到期与用户点托盘是两条
// 独立的线程路径，布尔量会让两边同时动手 —— 一边在销毁、一边在建窗，
// 结果是窗口对象错乱或同名 label 创建失败。CAS 保证同一时刻只有一方能推进。
//
// 这套状态机取自 Clash Verge Rev 的 lightweight mode，它同样是在托盘常驻
// 应用里销毁并重建主窗口。

/// 正常：窗口存在（可见或隐藏）
const ST_NORMAL: u8 = 0;
/// 已释放：窗口由我们销毁，当前不存在
const ST_RELEASED: u8 = 1;
/// 正在重建：窗口暂时不存在，建完会回到 NORMAL
const ST_REBUILDING: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(ST_NORMAL);

/// 应用是否正在**主动退出**。
///
/// `app.exit()` 与 `app.restart()` 同样会触发 `RunEvent::ExitRequested`，
/// 而低占用模式在那里默认 `prevent_exit()` —— 不先标记的话，释放在外时点
/// 「退出」会被自己拦住，程序退不掉；更糟的是 `on_app_exit()` 的清理已经
/// 先跑了（数据清了、程序还在）。关机与安装更新同理。
static EXITING: AtomicBool = AtomicBool::new(false);

/// 标记「接下来这次退出是用户/系统要求的」，必须放行。
pub(crate) fn note_exiting() {
    EXITING.store(true, Ordering::SeqCst);
}

pub(crate) fn is_exiting() -> bool {
    EXITING.load(Ordering::SeqCst)
}

/// 计时器代次。每次重新计时 +1，旧线程醒来发现代次变了就放弃，
/// 比「取消一个定时器」简单，也不必维护线程句柄。
static GEN: AtomicU64 = AtomicU64::new(0);

/// 释放前记下的窗口几何（x, y, width, height，**物理像素**）。
///
/// 项目没有窗口状态持久化（未用 window-state 插件），不记下来重建后
/// 窗口会跳回 `tauri.conf.json` 里的默认位置 —— 用户调过的位置就丢了。
///
/// 存物理而不是逻辑：重建出来的窗口最初落在**主屏**，若释放前在 150% 的
/// 副屏上，逻辑坐标会按主屏的 100% 换算，窗口直接错位。物理坐标与 DPI 无关。
static GEOM: Mutex<Option<(i32, i32, u32, u32)>> = Mutex::new(None);

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

/// 窗口转入显示：取消释放倒计时。
pub(crate) fn note_shown() {
    GEN.fetch_add(1, Ordering::SeqCst);
}

/// 立即释放：销毁主窗口，只留后端与托盘。
pub(crate) fn release_now(app: &tauri::AppHandle) {
    // 已在释放中或正在重建 → 让那一方完成，别抢
    if STATE
        .compare_exchange(ST_NORMAL, ST_RELEASED, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let destroyed = (|| {
        let w = app.get_webview_window(MAIN_LABEL)?;
        // 不释放仍可见的窗口：那是用户正在用的界面
        if w.is_visible().unwrap_or(false) {
            return None;
        }
        save_geometry(&w);
        // destroy 不走 close-requested，不会被「关闭时后台运行」拦下
        if let Err(e) = w.destroy() {
            crate::log_warn!("[idle] 销毁主窗口失败：{e}");
        }
        Some(())
    })();

    if destroyed.is_none() {
        // 没释放成功却停在 RELEASED，界面会永远被当成「已释放」——
        // 那样下一次 ensure_window 会去重建一个本来就存在的窗口，撞 label
        STATE.store(ST_NORMAL, Ordering::SeqCst);
        return;
    }
    // 托盘菜单与快捷面板是独立窗口，而且**只隐藏、从不销毁**（它们要能被
    // 反复唤起）。但 WebView2 的 browser / GPU / 网络 / 存储进程是整组共享的：
    // 只要还有一个 WebView 活着，这组进程就不会退，释放主窗口也省不下内存。
    // 这两个窗口本来就是懒创建的，这里一并销毁，下次用到会自动重建。
    for label in [crate::popup::MENU_LABEL, crate::popup::PANEL_LABEL] {
        if let Some(w) = app.get_webview_window(label) {
            let visible = w.is_visible().unwrap_or(false);
            if !visible {
                if let Err(e) = w.destroy() {
                    crate::log_warn!("[idle] 销毁 {label} 失败：{e}");
                }
            } else {
                crate::log_print!("[idle] {label} 仍可见，保留");
            }
        }
    }

    // 诊断：WebView2 的 browser / GPU / 网络 / 存储进程是整组共享的 ——
    // 只要还剩任何一个 WebView 窗口，这组进程就不会退。
    //
    // `destroy()` 是异步的：这一拍窗口仍留在 Tauri 的窗口表里，紧接着查会得到
    // 一个假的「还有 main」。所以推迟一拍再看，否则日志会误导排障
    // （实测：销毁后立刻查是 ["main"]，800ms 后是空，进程也确实退干净了）。
    let app_check = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        let left: Vec<String> = app_check.webview_windows().keys().cloned().collect();
        if left.is_empty() {
            crate::log_print!("[idle] 释放完成，已无 WebView 窗口");
        } else {
            crate::log_warn!("[idle] 释放后仍有 WebView 窗口：{left:?}（整组进程不会退出）");
        }
    });

    crate::log_print!("[idle] 界面已释放");
}

/// 主窗口是否处于「暂无窗口」的状态（已释放或正在重建）。
///
/// `RunEvent::ExitRequested` 靠它区分两种关闭：真值 = 我们自己销毁的，
/// 应用要继续跑；假值 = 用户要退出，放行。
pub(crate) fn is_released() -> bool {
    STATE.load(Ordering::SeqCst) != ST_NORMAL
}

/// 确保主窗口存在；已被释放则重建。
///
/// 托盘点击、单例唤醒等唤起路径都应该走这里，而不是直接 `get_webview_window`。
pub(crate) fn ensure_window(app: &tauri::AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(MAIN_LABEL) {
        return Some(w);
    }
    // 只有「已释放」才重建。正在重建时直接返回：那边建完自己会 show，
    // 这里再建一次只会撞同名 label。
    if STATE
        .compare_exchange(ST_RELEASED, ST_REBUILDING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return None;
    }

    match build_main_window(app) {
        Some(w) => {
            STATE.store(ST_NORMAL, Ordering::SeqCst);
            crate::log_print!("[idle] 界面已重建");
            Some(w)
        }
        None => {
            // 建窗失败：退回「已释放」，下次唤起还能再试
            STATE.store(ST_RELEASED, Ordering::SeqCst);
            crate::log_warn!("[idle] 界面重建失败");
            None
        }
    }
}

fn build_main_window(app: &tauri::AppHandle) -> Option<WebviewWindow> {
    // 用配置里的窗口定义重建，保证无边框、透明、尺寸等与原窗口一致；
    // 手写 builder 一定会漏掉某几项（装饰、transparent、min size…）
    let cfg = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_LABEL)?
        .clone();

    // 记下建窗起点：下面的 on_page_load 回调用它算出「从开始重建到界面可显示」
    // 的耗时，写进日志。优化重建速度前先有基线，否则只能凭感觉调。
    let started = std::time::Instant::now();
    let win = WebviewWindowBuilder::from_config(app, &cfg)
        .ok()?
        // 先隐藏，等页面渲染完再显示，避免白屏与主题色闪一下
        .visible(false)
        .on_page_load(move |w, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            let _ = w.show();
            let _ = w.set_focus();
            crate::log_print!(
                "[idle] 界面就绪，重建耗时 {} ms",
                started.elapsed().as_millis()
            );
        })
        .build()
        .ok()?;

    // 材质 / 圆角 / DPI 策略 / 焦点钩子 / 窗口事件监听，一次全部恢复。
    // 这几项原本只在启动时对 main 窗口做过一遍，重建的窗口是新的 hwnd，
    // 不重跑就等于换了个没有材质、没有缩放热区的裸窗口。
    crate::setup_effects(app);
    restore_geometry(&win);
    Some(win)
}

/// 记下窗口位置与大小。取不到就保持上次的值，不覆盖成 None。
fn save_geometry(w: &WebviewWindow) {
    let Ok(pos) = w.outer_position() else { return };
    let Ok(size) = w.inner_size() else { return };
    if let Ok(mut g) = GEOM.lock() {
        *g = Some((pos.x, pos.y, size.width, size.height));
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
    let _ = w.set_position(tauri::PhysicalPosition::new(v.0, v.1));
    let _ = w.set_size(tauri::PhysicalSize::new(v.2, v.3));
}
