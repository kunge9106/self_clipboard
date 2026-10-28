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

/// 主窗口标题（与 tauri.conf.json 一致）。托盘菜单窗口标题是「轻贴菜单」，
/// 长度都不等，所以**按标题精确匹配**就足以区分，不需要再比宽度。
const WINDOW_TITLE: &str = "轻贴";

/// 自定义窗口消息：第二个实例用它告诉已在运行的实例「又有人启动了」。
///
/// 取值落在 `WM_APP` 段（0x8000–0xBFFF，专供应用自定义），
/// 与系统消息、tao 自己的消息都不会撞。
pub const WM_QINGTIE_WAKE: u32 = 0x8000 + 0x51;

/// `OpenEventW` 的访问权限：只要求能 `SetEvent`。
const EVENT_MODIFY_STATE: u32 = 0x0002;
const INFINITE: u32 = 0xFFFF_FFFF;
const WAIT_OBJECT_0: u32 = 0;

/// 唤醒事件句柄：第一个实例创建后一直持有，监听线程用它等待。
static WAKE_HANDLE: OnceLock<isize> = OnceLock::new();

extern "system" {
    fn CreateMutexW(attrs: *mut c_void, initial_owner: i32, name: *const u16) -> isize;
    fn CloseHandle(h: isize) -> i32;
    fn GetLastError() -> u32;
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    // 低占用模式释放主窗口后，按标题已经找不到任何窗口：
    // 唤醒改为走命名事件（内核对象，与窗口无关）
    fn CreateEventW(
        attrs: *mut c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> isize;
    fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> isize;
    fn SetEvent(h: isize) -> i32;
    fn WaitForSingleObject(h: isize, millis: u32) -> u32;
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
        // 连锁都建不出来（极端情况）：不因为单实例机制挡住用户使用。
        // 但必须记：没有这把锁，就可能出现两个本版本实例**同时写同一份
        // index.json**，互相覆盖对方的条目 —— 表现为「记录莫名少了一半」。
        crate::log_warn!(
            "[singleton] 互斥体创建失败（锁名 {}），单实例保护失效：可能多实例同时写索引",
            lock_name()
        );
        return true;
    }
    if already {
        unsafe { CloseHandle(h) };
        crate::log_print!("[singleton] 已有同版本实例在运行（{}），本次唤回其窗口后退出", lock_name());
        false
    } else {
        let _ = LOCK_HANDLE.set(h);
        // 唤醒事件必须早于主窗口创建：低占用模式下窗口随时可能不存在，
        // 而事件得一直有人等
        let _ = WAKE_HANDLE.set(create_wake_event());
        // debug 与正式版用的是**不同锁名**：两者可以同时跑，而它们读写的是
        // **同一个缓存目录与同一份 index.json**。这条日志是为了让「多实例共存时
        // 数据被互相覆盖」这类事故一眼能看出来，而不是让人去猜。
        crate::log_print!(
            "[singleton] 已取得实例锁（{}），构建={}",
            lock_name(),
            if cfg!(debug_assertions) { "debug/开发" } else { "release/正式" }
        );
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
        ctx.found = hwnd;
        0
    }
}

fn find_main_window() -> isize {
    let mut ctx = SearchCtx {
        target: wide(WINDOW_TITLE),
        found: 0,
    };
    unsafe {
        EnumWindows(enum_cb, &mut ctx as *mut SearchCtx as isize);
    }
    ctx.found
}

/// 通知已运行的实例：请它把主窗口唤回前台，并弹一个应用内提示。
///
/// **只发一条消息，完全不碰窗口状态。** 上一版直接用
/// `ShowWindow(hwnd, SW_RESTORE)` 跨进程改窗口，有两个后果：
///   1. SW_RESTORE 的语义是「按窗口的还原矩形恢复」——窗口处于最大化/最小化，
///      或系统里记着的还原矩形比当前小时，就会被改回那个更小的尺寸
///      （用户实测：「重复打开时原有的窗口会缩小」）；
///   2. 它绕过了应用自己的唤回路径（材质补涂、WebView2 重新布局），
///      唤回后还会排版错位，而整个过程主进程毫不知情、无法自愈。
///
/// 现在由**主进程自己**在收到这条消息后处理（见 `effects::hook_proc`），
/// 走的是与托盘唤回完全相同的代码路径。
pub fn notify_existing_instance() -> bool {
    let hwnd = find_main_window();
    if hwnd == 0 {
        // 低占用模式已把主窗口连同 WebView 释放掉，按标题找不到任何窗口。
        // 退回命名事件，否则「再次双击」会毫无反应。
        return notify_via_event();
    }
    let posted = unsafe { PostMessageW(hwnd, WM_QINGTIE_WAKE, 0, 0) != 0 };
    if !posted {
        // 极少数情况下消息会被系统拦下（典型场景：对面以管理员身份运行，
        // UIPI 不允许低完整性进程往高完整性窗口投递消息）。
        // 这时退一步、至少把窗口带到前台——只改 Z 序、**不动尺寸**，安全。
        unsafe { SetForegroundWindow(hwnd) };
    }
    posted
}

/// 唤醒事件名：与锁名同样区分 debug/release，否则测试版会把正式版叫起来。
fn wake_name() -> &'static str {
    if cfg!(debug_assertions) {
        "Local\\com.kunge.selfclipboard.wake.debug"
    } else {
        "Local\\com.kunge.selfclipboard.wake"
    }
}

/// 创建唤醒事件：**自动重置**、初始无信号。只有第一个实例调用。
///
/// 自动重置送达即清，不必手动 `ResetEvent`；代价是两次送达挤在一起会合并成
/// 一次 —— 唤醒本身幂等（唤回窗口），无妨。
fn create_wake_event() -> isize {
    let name = wide(wake_name());
    let h = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, name.as_ptr()) };
    if h == 0 {
        crate::log_warn!("[singleton] 唤醒事件创建失败，释放界面后重复启动将无法唤回");
    }
    h
}

/// 用命名事件通知已运行的实例。返回是否送达。
fn notify_via_event() -> bool {
    let name = wide(wake_name());
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h == 0 {
        return false;
    }
    let ok = unsafe { SetEvent(h) } != 0;
    unsafe { CloseHandle(h) };
    ok
}

/// 起后台线程等唤醒事件：每次送达就唤回主窗口（窗口不存在时由 `show_main`
/// 负责重建）并通知前端。线程只做阻塞等待，空闲零 CPU。
pub fn spawn_wake_listener(app: tauri::AppHandle) {
    let event = match WAKE_HANDLE.get() {
        Some(h) if *h != 0 => *h,
        _ => return,
    };
    std::thread::spawn(move || loop {
        let r = unsafe { WaitForSingleObject(event, INFINITE) };
        if r != WAIT_OBJECT_0 {
            // WAIT_FAILED 之类：别空转把 CPU 打满
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        use tauri::Emitter;
        crate::tray::show_main(&app);
        let _ = app.emit("singleton-wake", ());
    });
}
