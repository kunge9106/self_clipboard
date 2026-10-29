//! 任务栏按钮的「再点一次收回」。
//!
//! ## 为什么不能靠系统消息
//!
//! Windows 对「点当前前台窗口的任务栏按钮」在不同版本 / 不同设置下行为不同：
//! 合并任务栏按钮时是弹出缩略图而不最小化（Win11 的默认），不合并时才会最小化
//! （最小化那条路会发 `WM_SYSCOMMAND`/`SC_MINIMIZE`）。本机实测两种消息都没被
//! 触发 —— 系统根本没让窗口进入最小化，拦消息无从谈起。
//!
//! ## 用 Shell Hook 接管
//!
//! 官方机制：`RegisterShellHookWindow` + `RegisterWindowMessage("SHELLHOOK")`。
//! 收到 `HSHELL_RUDEAPPACTIVATED`（0x8004）表示「用户以不温和的方式激活了某个
//! 窗口」—— 典型场景就是点了任务栏上某个**已在运行**程序的按钮。lParam 直接给出
//! 那个窗口的句柄，所以「点的是不是我」这件事不用猜。
//!
//! 消息号是运行时注册的（`RegisterWindowMessage`），编译期没有常量，必须在
//! 窗口过程里按值比较 —— 这也是官方文档给出的写法。

use std::sync::atomic::{AtomicU32, Ordering};

/// `HSHELL_RUDEAPPACTIVATED`：用户点了任务栏上某个已运行程序的按钮。
pub const HSHELL_RUDEAPPACTIVATED: usize = 0x8004;
/// `RegisterWindowMessage("SHELLHOOK")` 的返回值。运行时才有，故用原子变量存。
static SHELLHOOK_MSG: AtomicU32 = AtomicU32::new(0);

extern "system" {
    fn RegisterWindowMessageW(lp: *const u16) -> u32;
    fn RegisterShellHookWindow(hwnd: isize) -> i32;
    fn DeregisterShellHookWindow(hwnd: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 取得 `SHELLHOOK` 消息号（全局同一个值，取一次即可）。
fn shellhook_message() -> u32 {
    let cached = SHELLHOOK_MSG.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let name = wide("SHELLHOOK");
    let id = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if id != 0 {
        SHELLHOOK_MSG.store(id, Ordering::Relaxed);
    }
    id
}

/// 窗口过程里用它判断「这条消息是不是 Shell Hook」。
pub fn is_shellhook(msg: u32) -> bool {
    let id = shellhook_message();
    id != 0 && msg == id
}

/// 把窗口注册为 Shell Hook 接收者。
///
/// 注册后，任务栏点击、前台窗口切换等 Shell 事件会以窗口消息的形式送到本窗口的
/// 窗口过程里（而不是回调函数），所以我们现有的子类钩子直接就能收到。
pub fn install_shell_hook(hwnd: isize) -> bool {
    let id = shellhook_message();
    if id == 0 {
        return false;
    }
    let ok = unsafe { RegisterShellHookWindow(hwnd) } != 0;
    if ok {
        crate::log_print!("[taskbar] 已注册 Shell Hook（消息号 {id}）");
    } else {
        crate::log_print!("[taskbar] 注册 Shell Hook 失败");
    }
    ok
}

/// 取消注册（窗口销毁时用）。
pub fn remove_shell_hook(hwnd: isize) {
    unsafe {
        DeregisterShellHookWindow(hwnd);
    }
}
