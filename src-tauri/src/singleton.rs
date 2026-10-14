//! 单实例约束：**同一版本**只允许一个实例运行。
//!
//! 测试版(debug) 与正式版(release) 使用不同的锁名，因此两者可以共存、互不唤醒。
//! 这也是为什么不用 `tauri-plugin-single-instance`——那个插件按 app identifier
//! 判重，而 debug/release 共用同一份 `tauri.conf.json`（identifier 相同），
//! 要共存就必须手动区分锁名。
//!
//! 手写 Win32 FFI，不引入 windows crate（与项目既有风格一致）。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;

const ERROR_ALREADY_EXISTS: u32 = 183;
const SW_RESTORE: i32 = 9;

/// 主窗口标题（与 tauri.conf.json 一致）。托盘菜单窗口标题不同且很窄，
/// 用宽度下限过滤，避免误把托盘小窗带到前台。
const WINDOW_TITLE: &str = "轻贴";
const MIN_MAIN_WIDTH: i32 = 300;

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

extern "system" {
    fn CreateMutexW(attrs: *mut c_void, initial_owner: i32, name: *const u16) -> isize;
    fn CloseHandle(h: isize) -> i32;
    fn GetLastError() -> u32;
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 持有 mutex 句柄：进程存活期间锁名一直被占用，后续实例才能判定为"重复"。
static LOCK_HANDLE: OnceLock<isize> = OnceLock::new();

fn lock_name() -> &'static str {
    if cfg!(debug_assertions) {
        // 测试版独立锁名 → 与正式版互不干扰，可共存
        "Local\\com.kunge.selfclipboard.instance.debug"
    } else {
        "Local\\com.kunge.selfclipboard.instance"
    }
}

/// `true` = 本进程是当前唯一实例（正常继续启动）。
/// `false` = 同版本实例已在运行（调用方应激活其窗口后退出）。
pub fn ensure_single_instance() -> bool {
    let name = wide(lock_name());
    let h = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if h == 0 {
        // 连锁都建不出来（极端情况）：不因为单实例机制挡住用户使用
        return true;
    }
    if already {
        unsafe { CloseHandle(h) };
        false
    } else {
        let _ = LOCK_HANDLE.set(h);
        true
    }
}

struct SearchCtx {
    target: Vec<u16>,
    found: isize,
}

extern "system" fn enum_cb(hwnd: isize, param: isize) -> i32 {
    unsafe {
        let ctx = &mut *(param as *mut SearchCtx);
        if ctx.found != 0 {
            return 0; // 已找到，停止枚举
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), 256);
        if len <= 0 {
            return 1;
        }
        let len = len as usize;
        // target 末尾带 null 终止符：先比长度，否则短标题切长下标会越界 panic
        if len + 1 != ctx.target.len() {
            return 1;
        }
        if buf[..len] != ctx.target[..len] {
            return 1;
        }
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect) != 0 && (rect.right - rect.left) >= MIN_MAIN_WIDTH {
            ctx.found = hwnd;
            return 0;
        }
        1
    }
}

/// 把已运行实例的主窗口恢复到前台（第二次双击/启动时的反馈）。
pub fn activate_existing_instance() {
    let mut ctx = SearchCtx {
        target: wide(WINDOW_TITLE),
        found: 0,
    };
    unsafe {
        EnumWindows(enum_cb, &mut ctx as *mut SearchCtx as isize);
    }
    if ctx.found != 0 {
        unsafe {
            ShowWindow(ctx.found, SW_RESTORE);
            SetForegroundWindow(ctx.found);
        }
    }
}
