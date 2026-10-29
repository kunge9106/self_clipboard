//! 任务栏按钮的「再点一次收回」。
//!
//! 为什么不能用消息拦截：Windows 对「点当前前台窗口的任务栏按钮」在不同版本上
//! 行为不同 —— 有的版本发 `WM_SYSCOMMAND`/`SC_MINIMIZE`，有的（Win11 的一部分
//! 版本）**什么消息都不发**，点击只是把窗口保持在前台。本机实测：拦
//! `WM_SYSCOMMAND/SC_MINIMIZE` 与 `WM_SIZE/SIZE_MINIMIZED` 都没被触发（日志里
//! 一条记录都没有），说明系统根本没让窗口进入最小化，拦消息无从谈起。
//!
//! 所以改成自己判断「用户点了任务栏」：
//!   1. 轮询左键的按下 / 抬起沿（`GetAsyncKeyState`，读内存，代价可忽略）；
//!   2. 按下时若光标落在任务栏矩形内，记为「疑似点了任务栏」；
//!   3. 抬起时若仍在任务栏内、且**前台窗口仍然是我们** → 说明点的是我们自己的
//!      按钮（点了别的程序，前台早就换人了）→ 收进托盘。
//!
//! 为什么不在「按下」时判断：按下瞬间前台还没切换，点别的程序也会是我们；
//! 抬起时前台已经定下来了，这时才能分辨。
//!
//! 与「面板失焦不收起」不冲突：点了别的程序时前台不是我们，不会触发收起，
//! 窗口去留仍由那个开关决定。

use tauri::Manager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const TASKBAR_CLASS: &str = "Shell_TrayWnd";
const VK_LBUTTON: i32 = 0x01;
/// 轮询间隔（毫秒）。50ms 足以让「按下 → 抬起」被完整捕获，代价可以忽略。
const POLL_MS: u64 = 50;

// 窗口矩形与光标点复用 `effects` 里的定义：同形状的结构重复定义会让两处
// `GetWindowRect` / `GetCursorPos` 的 extern 声明互相打架（effects.rs 里已记过这个坑）。
use crate::effects::Rect;

extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
    fn GetCursorPos(p: *mut Rect) -> i32;
    fn FindWindowW(class: *const u16, title: *const u16) -> isize;
    fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn GetForegroundWindow() -> isize;
    fn IsWindow(hwnd: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON) as u16) & 0x8000 != 0 }
}

fn cursor_in_taskbar() -> bool {
    unsafe {
        let class = wide(TASKBAR_CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd == 0 {
            return false;
        }
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return false;
        }
        let mut pt = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetCursorPos(&mut pt) == 0 {
            return false;
        }
        pt.left >= rect.left
            && pt.left < rect.right
            && pt.top >= rect.top
            && pt.top < rect.bottom
    }
}

/// 主窗口当前的 hwnd。后台释放会销毁并重建窗口，所以每次取，不缓存。
fn main_hwnd(app: &tauri::AppHandle) -> Option<isize> {
    let w = app.get_webview_window("main")?;
    let h = w.hwnd().ok()?;
    Some(h.0 as isize)
}

/// 启动监视线程。
pub fn spawn_taskbar_click_watcher(app: tauri::AppHandle) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let mut was_down = false;
        let mut pressed_on_taskbar = false;
        loop {
            let down = left_button_down();
            if down && !was_down {
                // 按下：记下是否落在任务栏上
                pressed_on_taskbar = cursor_in_taskbar();
            } else if !down && was_down && pressed_on_taskbar {
                // 抬起：仍在任务栏上、且前台仍是我们 → 点的是我们自己的按钮
                pressed_on_taskbar = false;
                if cursor_in_taskbar() {
                    let foreground = unsafe { GetForegroundWindow() };
                    let ours = main_hwnd(&app);
                    if let Some(h) = ours {
                        if unsafe { IsWindow(h) } != 0 && foreground == h {
                            crate::log_print!("[taskbar] 再次点击任务栏图标 → 收进托盘");
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.hide();
                            }
                        }
                    }
                }
            }
            was_down = down;
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    });
}
