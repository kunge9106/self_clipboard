//! 系统截图（`ms-screenclip:`）与「右键取消」。
//!
//! **为什么用系统截图而不是自己画选区**：选区框选、多屏拼接、DPI 缩放适配这几件事
//! Windows 已经做得很稳，自己实现至少要两三天，而且很容易在 150% 缩放的多屏上错位。
//! 截完图会进剪贴板，采集线程随即把它记成图片条目 —— 用户拿到的体验仍是
//! 「按一下、框一块、进轻贴」。
//!
//! **右键取消为什么要自己补**：Win11 的系统截图覆盖层对右键**完全不响应**（实测），
//! 只能按 Esc 或点工具条上的 ×；很多人习惯右键取消，右键按下去毫无反应会以为卡了。
//! 这里挂一个**临时**低级鼠标钩子，把「覆盖层上的右键」翻译成 Esc 再吞掉。
//!
//! 钩子装在 `hotkey` 那个 Win32 消息线程上，而不是自己再起一个消息循环：
//! 低级钩子的回调要靠消息泵派发，项目里已经有一个专职的消息线程，再建一个就是
//! 两份 `MSG` 定义加两套循环，出问题时要同时看两个线程。
//!
//! 钩子的生命周期严格跟着**一次**截图：等到覆盖层出现 → 等它消失（或用户右键取消）
//! → 立刻卸载。最坏情况 180 秒兜底，绝不会在后台一直挂着。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use crate::effects::Rect;

/// 让热键线程安装钩子（`WM_APP + 2`）
pub const WM_INSTALL_HOOK: u32 = 0x8000 + 2;
/// 让热键线程卸载钩子（`WM_APP + 3`）
pub const WM_REMOVE_HOOK: u32 = 0x8000 + 3;

const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const WM_RBUTTONDOWN: usize = 0x0204;
const VK_ESCAPE: u16 = 0x1B;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

/// 钩子是否已装上（同一时刻只留一个：连按两次快捷键不该挂两个）
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 用户已经用右键取消过，监视线程可以提前收工
static CANCELLED: AtomicBool = AtomicBool::new(false);
/// 钩子句柄，回调里转发给下一个钩子时要用
static HOOK: AtomicIsize = AtomicIsize::new(0);

type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;

extern "system" {
    fn SetWindowsHookExW(id: i32, proc: HookProc, module: isize, thread: u32) -> isize;
    fn UnhookWindowsHookEx(hook: isize) -> i32;
    fn CallNextHookEx(hook: isize, code: i32, w: usize, l: isize) -> isize;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GetLastError() -> u32;
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn QueryFullProcessImageNameW(proc: isize, flags: u32, buf: *mut u16, size: *mut u32) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn ShellExecuteW(
        hwnd: isize,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
}

/// 唤起系统截图；如果开着「右键取消」，顺手挂上临时钩子。
pub fn launch() {
    unsafe {
        let op: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let target: Vec<u16> = "ms-screenclip:".encode_utf16().chain(std::iter::once(0)).collect();
        // 返回值 > 32 才算成功（<= 32 是错误码）
        let rc = ShellExecuteW(
            0,
            op.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
        if (rc as isize) <= 32 {
            crate::log_warn!(
                "[shot] 唤起系统截图失败（ShellExecuteW 返回 {rc}），需要 Win10 1809+ 且已安装「截图工具」"
            );
            return;
        }
    }
    if crate::settings::get().screenshot_right_click_cancel {
        post(WM_INSTALL_HOOK);
    }
}

fn post(msg: u32) {
    // 投递统一走 `hotkey::post_msg`：热键线程是本进程唯一的 Win32 消息线程，
    // 各模块自己 PostThreadMessage 迟早会出现「线程 ID 取法不一致」的幽灵问题。
    crate::hotkey::post_msg(msg);
}

/// 安装钩子。**必须在热键线程调用**（低级钩子与安装线程绑定）。
pub fn install_hook() {
    if ACTIVE.load(Ordering::SeqCst) {
        // 上一次的钩子还在（用户连按两次快捷键）—— 复用，别再挂第二个
        return;
    }
    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, GetModuleHandleW(std::ptr::null()), 0);
        if hook == 0 {
            crate::log_warn!(
                "[shot] 右键取消不可用：安装鼠标钩子失败（错误码 {}）",
                GetLastError()
            );
            return;
        }
        HOOK.store(hook, Ordering::SeqCst);
    }
    CANCELLED.store(false, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);
    std::thread::spawn(watch);
}

/// 卸载钩子。同样必须在热键线程调用。
pub fn remove_hook() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let hook = HOOK.swap(0, Ordering::SeqCst);
    if hook != 0 {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
        crate::log_print!("[shot] 右键取消钩子已卸载（一次截图结束）");
    }
}

/// 低级鼠标钩子回调。系统对它的要求是「快」：超时会直接把钩子摘掉。
///
/// 这里只做三件轻活（取前台窗口、查进程名、读窗口矩形），不碰文件、不打日志、
/// 不取配置锁 —— 一旦某次右键判定成立，才发 Esc 并记日志。
unsafe extern "system" fn mouse_proc(code: i32, w: usize, l: isize) -> isize {
    if code == HC_ACTION && w == WM_RBUTTONDOWN && overlay_in_front() {
        if !crate::input::send_keys(&[VK_ESCAPE]) {
            crate::log_warn!("[shot] 右键取消：Esc 注入失败（目标进程权限更高？）");
        }
        CANCELLED.store(true, Ordering::SeqCst);
        // 吞掉这次右键：覆盖层本来就不响应它，放过去只会让用户等一个永远不会来的反应
        return 1;
    }
    CallNextHookEx(HOOK.load(Ordering::SeqCst), code, w, l)
}

/// 监视线程：等覆盖层出现 → 等它消失 → 卸载钩子。
///
/// 之所以要「等它出现」再「等它消失」，是因为 `ShellExecuteW` 返回时覆盖层还没画出来
/// （实测约 1–2 秒），立刻检查会以为「已经结束了」而把钩子白卸掉。
fn watch() {
    let start = Instant::now();
    let mut seen = false;
    let mut miss = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if CANCELLED.load(Ordering::SeqCst) {
            crate::log_print!("[shot] 右键取消，已向截图覆盖层发送 Esc");
            break;
        }
        if unsafe { overlay_in_front() } {
            seen = true;
            miss = 0;
        } else if seen {
            // 连续 3 次（约 0.6 秒）都看不到才算结束。覆盖层关闭的瞬间前台会短暂
            // 变成别的窗口，只取一次样容易误判成「已经关掉了」。
            miss += 1;
            if miss >= 3 {
                break;
            }
        }
        if start.elapsed() > Duration::from_secs(180) {
            crate::log_warn!("[shot] 右键取消钩子超过 180 秒，自动卸载（截图可能一直开着）");
            break;
        }
    }
    post(WM_REMOVE_HOOK);
}

/// 前台窗口是不是「系统截图的覆盖层」。
///
/// 两个条件缺一不可：
/// 1. 进程名属于截图工具 —— Win11 是 `SnippingTool.exe`，Win10 是
///    `ScreenClippingHost.exe`（1809 起） / `ScreenSketch.exe`。
/// 2. 窗口**铺满整个虚拟屏幕**。截图编辑器是同一个 exe 的普通窗口，那里右键是有
///    菜单的，不区分的话会把编辑器的右键菜单一起吞掉。
unsafe fn overlay_in_front() -> bool {
    let fg = GetForegroundWindow();
    if fg == 0 || !is_snip_process(fg) {
        return false;
    }
    let mut r = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetWindowRect(fg, &mut r) == 0 {
        return false;
    }
    let (vw, vh) = (
        GetSystemMetrics(SM_CXVIRTUALSCREEN),
        GetSystemMetrics(SM_CYVIRTUALSCREEN),
    );
    // 留 16px 容差：覆盖层比虚拟屏幕略小一点点是正常的（DWM 边框/DPI 取整）
    (r.right - r.left) >= vw - 16 && (r.bottom - r.top) >= vh - 16
}

/// 前台窗口的进程名是否属于系统截图工具。
unsafe fn is_snip_process(hwnd: isize) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return false;
    }
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h == 0 {
        return false;
    }
    let mut buf = [0u16; 260];
    let mut n = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n);
    CloseHandle(h);
    if ok == 0 {
        return false;
    }
    let full = String::from_utf16_lossy(&buf[..n as usize]).to_ascii_lowercase();
    let name = full.rsplit('\\').next().unwrap_or("");
    matches!(
        name,
        "snippingtool.exe" | "screensketch.exe" | "screenclippinghost.exe"
    )
}
