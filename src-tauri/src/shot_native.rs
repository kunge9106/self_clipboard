//! 截图覆盖层：Win32 原生实现（S1）。
//!
//! ## 为什么不用 WebView
//!
//! 前一版把覆盖层做成 Tauri WebviewWindow，一天之内修了十三条分支（焦点、事件、
//! 时序、连按、焦点还回…），全是同一类病。根因是**让渲染进程参与输入**：
//!
//! - `WS_EX_NOACTIVATE` 窗口里的 Chromium 不合成 `click` / `dblclick` / `contextmenu`；
//! - `mousedown` / `mousemove` / `mouseup` 也不保证每次到达；
//! - 后台进程抢焦点（`SetForegroundWindow` / `AttachThreadInput` / Alt 注入）被系统静默拒绝。
//!
//! 截图覆盖层需要的只有两件事：稳定的输入、即时的重绘。这两件事 Win32 原生窗口都能直接做到，
//! 所以这里用**独立线程建窗 + 自跑消息循环 + GDI 双缓冲**，整条链路不经过渲染进程。
//!
//! ## 结构
//!
//! ```text
//! start()                    调用线程：抓屏 + 枚举窗口（都在脱离屏幕显示前完成）
//!   └─ spawn 线程            run_overlay()：注册窗口类 → 建窗 → 装键盘钩子 → 消息循环
//!        ├─ WM_PAINT        双缓冲：压暗版整屏 → 选区还原 → 边框/手柄/角标/工具条
//!        ├─ WM_LBUTTONDOWN  按命中结果决定：工具条按钮 / 手柄缩放 / 选区内平移 / 新框选
//!        ├─ WM_MOUSEMOVE    拖拽中更新选区；空闲时更新悬停窗口
//!        ├─ WM_LBUTTONUP    结束拖拽；位移 < 4px 视为单击 → 选中悬停窗口
//!        ├─ 双击 / Enter / 工具条绿勾 → commit()
//!        └─ 右键 / Esc / 工具条红叉 → 取消
//! ```
//!
//! 键盘靠**低级键盘钩子**转发：NOACTIVATE 窗口拿不到焦点，也就收不到 `WM_KEYDOWN`。
//!
//! ## 两条不能违反的时序
//!
//! 1. **先抓屏、再建窗**。反过来会把还在淡入的覆盖层截进图里。
//! 2. **在显示覆盖层之前枚举窗口**。窗口一铺满屏幕，`WindowFromPoint` 只会返回它自己。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::AppHandle;

// 消息结构复用 `hotkey` 那一份：同一个 `GetMessageW` 用两种结构体声明会得到
// "redeclared with a different signature" 警告，而那条警告会把真正的链接问题淹没。
use crate::hotkey::Msg;

// ───────────────────────────── Win32 常量 ─────────────────────────────

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

const CS_HREDRAW: u32 = 0x0001;
const CS_VREDRAW: u32 = 0x0002;
const CS_DBLCLKS: u32 = 0x0008;

const WM_DESTROY: u32 = 0x0002;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETCURSOR: u32 = 0x0020;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
/// 键盘钩子转发过来的「取消」/「完成」。取 `WM_APP` 段，避免与系统消息号冲突。
const WM_NATIVE_CANCEL: u32 = 0x8000 + 0x51;
const WM_NATIVE_COMMIT: u32 = 0x8000 + 0x52;

const HWND_TOPMOST: isize = -1;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;

const SRCCOPY: u32 = 0x00CC_0020;
const DIB_RGB_COLORS: u32 = 0;
const BI_RGB: u32 = 0;

const WH_KEYBOARD_LL: i32 = 13;
const HC_ACTION: i32 = 0;
const WM_KEYDOWN: usize = 0x0100;
const WM_SYSKEYDOWN: usize = 0x0104;
const VK_ESCAPE: u32 = 0x1B;
const VK_RETURN: u32 = 0x0D;

/// 每显示器 DPI 感知 v2 的上下文句柄（Windows 用负值表示预定义 context）。
///
/// **必须在 `CreateWindowExW` 之前设置**：窗口的 DPI awareness 在创建那一刻就
/// 冻结了（`GetWindowDpiAwarenessContext` 是窗口级状态），之后再改线程 context
/// 不会影响已建窗口。
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

/// 覆盖层线程统一钉到 per-monitor v2，让所有坐标都是**物理像素**。
///
/// 线程若不是 DPI 感知，Windows 会把 `GetCursorPos` / `GetMonitorInfo` 的返回值
/// "虚拟化"成 96 DPI 的逻辑坐标。结果就是 150% 的副屏上：框选的框和真正截到的
/// 区域错位（框一半、截另一半）。`start()` 与覆盖层线程各调一次 —— 前者管
/// 抓屏与取显示器矩形，后者管建窗。
fn pin_dpi_context() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

const DT_CENTER: u32 = 0x0000_0001;
const DT_VCENTER: u32 = 0x0000_0004;
const DT_SINGLELINE: u32 = 0x0000_0020;
const TRANSPARENT: i32 = 1;

// 光标资源号。`LoadCursorW` 接受 MAKEINTRESOURCE，即直接把序号当指针传。
const IDC_ARROW: usize = 32512;
const IDC_CROSS: usize = 32515;
const IDC_SIZEALL: usize = 32646;
const IDC_SIZENS: usize = 32645;
const IDC_SIZEWE: usize = 32644;
const IDC_SIZENWSE: usize = 32642;
const IDC_SIZENESW: usize = 32643;

/// 覆盖层是否已有一轮在跑。热键线程可能连按，这里做全局防抖。
static BUSY: AtomicBool = AtomicBool::new(false);
/// 键盘钩子要投递的目标窗口。钩子回调是自由函数，只能靠静态变量传递。
static KB_TARGET: AtomicIsize = AtomicIsize::new(0);
/// 会话序号，只用于日志对齐。
static SEQ: AtomicU32 = AtomicU32::new(0);

// ───────────────────────────── Win32 类型 ─────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `MonitorFromPoint` 按值收 `POINT`（8 字节）。不能用 `Rect`（16 字节）代替：
/// x64 ABI 下超过 8 字节的结构按引用传递，两个参数会整体错位。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 3],
}

#[repr(C)]
struct WndClassW {
    style: u32,
    wnd_proc: Option<extern "system" fn(isize, u32, usize, isize) -> isize>,
    cls_extra: i32,
    wnd_extra: i32,
    instance: isize,
    icon: isize,
    cursor: isize,
    background: isize,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct PaintStruct {
    hdc: isize,
    erase: i32,
    paint: WinRect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: WinRect,
    work: WinRect,
    flags: u32,
}

#[repr(C)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;
type EnumProc = extern "system" fn(isize, isize) -> i32;

#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn DwmFlush() -> i32;
}

#[link(name = "msimg32")]
extern "system" {
    /// 带 alpha 的位图合成。工具条的圆角与斜线靠它贴上去：GDI 的 `RoundRect`
    /// 与 `LineTo` 没有抗锯齿，76×38 的小图上锯齿一眼就能看出来。
    fn AlphaBlend(
        dst: isize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        src: isize,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        blend: BlendFunction,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BlendFunction {
    blend_op: u8,
    blend_flags: u8,
    source_constant_alpha: u8,
    alpha_format: u8,
}

extern "system" {
    // 窗口与消息
    fn RegisterClassW(class: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: isize,
        menu: isize,
        instance: isize,
        param: isize,
    ) -> isize;
    fn DestroyWindow(hwnd: isize) -> i32;
    fn DefWindowProcW(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn PostMessageW(hwnd: isize, msg: u32, w: usize, l: isize) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32)
        -> i32;
    fn InvalidateRect(hwnd: isize, rect: *const WinRect, erase: i32) -> i32;
    fn SetCapture(hwnd: isize) -> isize;
    fn ReleaseCapture() -> i32;
    fn GetCursorPos(pt: *mut crate::effects::Rect) -> i32;
    fn SetCursor(cursor: isize) -> isize;
    fn LoadCursorW(instance: isize, name: *const u16) -> isize;
    fn GetModuleHandleW(name: *const u16) -> isize;
    /// 线程级 DPI 上下文。必须在建窗之前设置，见 `pin_dpi_context`。
    fn SetThreadDpiAwarenessContext(ctx: isize) -> isize;
    /// 窗口当前所在显示器的 DPI（96 = 100%）。仅用于日志核对。
    fn GetDpiForWindow(hwnd: isize) -> u32;
    fn MonitorFromPoint(pt: Point, flags: u32) -> isize;
    fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    fn SetWindowsHookExW(id: i32, proc: HookProc, module: isize, thread: u32) -> isize;
    fn UnhookWindowsHookEx(hook: isize) -> i32;
    fn CallNextHookEx(hook: isize, code: i32, w: usize, l: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    fn EnumWindows(proc: EnumProc, param: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn GetWindowTextLengthW(hwnd: isize) -> i32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;

    // GDI
    fn CreateDCW(driver: *const u16, device: *const u16, port: *const u16, init: *const u16) -> isize;
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn CreateDIBSection(
        hdc: isize,
        bmi: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut std::ffi::c_void,
        section: isize,
        offset: u32,
    ) -> isize;
    fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
    fn SelectObject(hdc: isize, obj: isize) -> isize;
    fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32)
        -> i32;
    fn GetDIBits(
        hdc: isize,
        bmp: isize,
        start: u32,
        lines: u32,
        bits: *mut u8,
        bmi: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(obj: isize) -> i32;
    fn DeleteDC(hdc: isize) -> i32;
    fn BeginPaint(hwnd: isize, ps: *mut PaintStruct) -> isize;
    fn EndPaint(hwnd: isize, ps: *const PaintStruct) -> i32;
    fn CreateSolidBrush(color: u32) -> isize;
    fn FillRect(hdc: isize, rect: *const WinRect, brush: isize) -> i32;
    fn SetTextColor(hdc: isize, color: u32) -> u32;
    fn SetBkMode(hdc: isize, mode: i32) -> i32;
    fn DrawTextW(hdc: isize, text: *const u16, len: i32, rect: *mut WinRect, format: u32) -> i32;
}

// ───────────────────────────── 数据结构 ─────────────────────────────

/// 八向手柄。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
}

/// 选区（覆盖层客户区坐标，物理像素）。
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Sel {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Sel {
    fn w(&self) -> i32 {
        self.right - self.left
    }
    fn h(&self) -> i32 {
        self.bottom - self.top
    }
    fn valid(&self) -> bool {
        self.w() >= 1 && self.h() >= 1
    }
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
    fn normalized(l: i32, t: i32, r: i32, b: i32) -> Self {
        Self {
            left: l.min(r),
            top: t.min(b),
            right: l.max(r),
            bottom: t.max(b),
        }
    }
    /// 虚拟屏坐标 → 客户区坐标
    fn from_rect(r: [i32; 4], off_x: i32, off_y: i32) -> Self {
        Sel {
            left: r[0] - off_x,
            top: r[1] - off_y,
            right: r[2] - off_x,
            bottom: r[3] - off_y,
        }
    }
    fn to_win(&self) -> WinRect {
        WinRect {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    /// 没在拖。仍可能已有选区（松手后停在这里）。
    Idle,
    /// 正在拖出新选区。
    Create,
    /// 按住选区内部平移。
    Move { grab_x: i32, grab_y: i32, origin: Sel },
    /// 按住某个手柄缩放。
    Resize { handle: Handle, origin: Sel },
}

/// 一次截图会话的全部状态。
///
/// 放在全局 `Mutex` 里，因为窗口过程是 `extern "system"` 自由函数，拿不到 Rust 侧上下文。
struct Session {
    hwnd: isize,
    /// 覆盖层左上角在虚拟屏中的物理坐标。
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,

    /// 冻结帧（自上而下 BGRA）。出图时从这里裁，裁的是原始像素而非压暗版。
    pixels: Vec<u8>,
    /// 抓屏前枚举的顶层窗口矩形（虚拟屏物理坐标，按 z 序从高到低）。
    windows: Vec<[i32; 4]>,

    /// 整屏压暗版与原始帧，各配一个内存 DC。绘制时先铺压暗版，再把选区抠回原始帧。
    dim_bmp: isize,
    dim_dc: isize,
    frame_bmp: isize,
    frame_dc: isize,
    /// 双缓冲目标，防闪烁。
    mem_bmp: isize,
    mem_dc: isize,

    sel: Sel,
    mode: DragMode,
    /// 本次按下时的光标位置，用于区分「单击」与「拖拽」。
    down_x: i32,
    down_y: i32,
    /// 按下时悬停的窗口（单击选中的就是它）。
    down_window: Option<Sel>,
    /// 当前悬停的窗口（客户区坐标）。
    hover_window: Option<Sel>,
    /// 工具条上悬停的按钮：0 无、1 取消（左半）、2 完成（右半）。
    /// 悬停时该半区加底色并在工具条外侧显示气泡提示。
    hover_bar: u8,

    /// 已提交，防止 GUI 绿勾与 Enter 同时触发写两次剪贴板。
    committed: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.dim_bmp);
            DeleteObject(self.frame_bmp);
            DeleteObject(self.mem_bmp);
            DeleteDC(self.dim_dc);
            DeleteDC(self.frame_dc);
            DeleteDC(self.mem_dc);
        }
    }
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// 对会话做一次只读访问。拿不到锁就当没有会话 —— 调用方全是消息回调，
/// 拿不到状态时直接跳过这一帧即可，不值得为它 panic。
fn with_session<R>(f: impl FnOnce(&Session) -> R) -> Option<R> {
    let g = SESSION.lock().ok()?;
    g.as_ref().map(f)
}

fn with_session_mut<R>(f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    let mut g = SESSION.lock().ok()?;
    g.as_mut().map(f)
}

// ───────────────────────────── 入口 ─────────────────────────────

/// `Alt+Q` 的入口（内置截图模式）。
///
/// 抓屏与窗口枚举留在调用线程完成，之后再 spawn 覆盖层线程：这两步是「脱离屏幕显示前
/// 必须做完」的动作，放进新线程没有好处，只会让「按下 → 有画面」多绕一圈。
pub fn start(_app: &AppHandle) {
    if BUSY.swap(true, Ordering::SeqCst) {
        crate::log_print!("[shot] 忽略重复触发（上一轮尚未结束）");
        return;
    }

    // 抓屏与取显示器矩形都要用**物理像素**。线程若不是 DPI 感知，Windows 会把
    // `GetCursorPos` / `GetMonitorInfo` 虚拟化成 96 DPI 的逻辑坐标 —— 高 DPI 副屏上
    // 就成了「框这一半、截另一半」。awareness 在建窗时冻结，这里先钉好。
    pin_dpi_context();

    let Some((left, top, w, h)) = monitor_at_cursor() else {
        crate::log_warn!("[shot] 找不到鼠标所在的显示器，截图取消");
        BUSY.store(false, Ordering::SeqCst);
        return;
    };

    let t0 = std::time::Instant::now();
    let Some(pixels) = (unsafe { grab(left, top, w, h) }) else {
        crate::log_warn!("[shot] 抓屏失败（BitBlt/GetDIBits），截图取消");
        BUSY.store(false, Ordering::SeqCst);
        return;
    };
    crate::log_print!(
        "[shot] 抓屏完成 {w}×{h} @{left},{top}，{} KB，耗时 {}ms",
        pixels.len() / 1024,
        t0.elapsed().as_millis()
    );

    // 必须在覆盖层显示前枚举（见模块注释第 2 条时序）
    let windows = unsafe { list_windows() };
    crate::log_print!("[shot] 可识别的窗口 {} 个", windows.len());

    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    crate::log_print!("[shot] 会话 #{seq} 开始（原生覆盖层）");

    std::thread::spawn(move || {
        run_overlay(seq, left, top, w, h, pixels, windows);
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// 鼠标所在显示器的物理矩形（左, 上, 宽, 高）。
fn monitor_at_cursor() -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let mut pt = crate::effects::Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        let mon = MonitorFromPoint(
            Point {
                x: pt.left,
                y: pt.top,
            },
            2,
        ); // MONITOR_DEFAULTTONEAREST
        if mon == 0 {
            return None;
        }
        let mut info = MonitorInfo {
            size: std::mem::size_of::<MonitorInfo>() as u32,
            monitor: WinRect::default(),
            work: WinRect::default(),
            flags: 0,
        };
        if GetMonitorInfoW(mon, &mut info) == 0 {
            return None;
        }
        let r = info.monitor;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        Some((r.left, r.top, w, h))
    }
}

// ───────────────────────────── 覆盖层线程 ─────────────────────────────

fn run_overlay(
    seq: u32,
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
    windows: Vec<[i32; 4]>,
) {
    // 新建线程默认继承进程的 DPI 上下文，但窗口的 awareness 在 `CreateWindowExW`
    // 那一刻冻结，所以这里必须显式钉一次（父线程设过也重设，幂等且便宜）。
    pin_dpi_context();

    let Some(session) = (unsafe { build_session(left, top, w, h, pixels, windows) }) else {
        crate::log_warn!("[shot] 会话 #{seq} 建窗失败");
        return;
    };
    let hwnd = session.hwnd;
    if let Ok(mut g) = SESSION.lock() {
        *g = Some(session);
    }
    KB_TARGET.store(hwnd, Ordering::SeqCst);

    // 钩子必须在有消息循环的线程上装：回调就在该线程执行。
    let hook = unsafe { install_keyboard_hook() };

    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            left,
            top,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        // 让 DWM 把这一帧合成完再往下走，缩短「按下到看见」的延迟
        DwmFlush();
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }

    let t0 = std::time::Instant::now();
    // DPI 写进日志：多屏/缩放出问题时，先看这个数与显示器矩形是否自洽
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    crate::log_print!(
        "[shot] 会话 #{seq} 覆盖层已就位（{}ms），DPI={dpi}，{w}×{h} @{left},{top}",
        t0.elapsed().as_millis()
    );

    unsafe {
        let mut msg = std::mem::zeroed::<Msg>();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    if hook != 0 {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
    }
    KB_TARGET.store(0, Ordering::SeqCst);

    let committed = if let Ok(mut g) = SESSION.lock() {
        let c = g.as_ref().map(|s| s.committed).unwrap_or(false);
        // 取完状态才能丢：Drop 里释放 GDI 对象
        *g = None;
        c
    } else {
        false
    };
    crate::log_print!(
        "[shot] 会话 #{seq} 结束（{}，停留 {}ms）",
        if committed { "已复制" } else { "已取消" },
        t0.elapsed().as_millis()
    );
}

unsafe fn build_session(
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
    windows: Vec<[i32; 4]>,
) -> Option<Session> {
    let class_name = wide("QingTieShotNative");
    let title = wide("轻贴截图");

    // 窗口类只需注册一次；重复注册返回 0，无害。
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    let instance = GetModuleHandleW(std::ptr::null());
    if !REGISTERED.swap(true, Ordering::SeqCst) {
        let wc = WndClassW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            wnd_proc: Some(wnd_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        RegisterClassW(&wc);
    }

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class_name.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        left,
        top,
        w,
        h,
        0,
        0,
        instance,
        0,
    );
    if hwnd == 0 {
        return None;
    }

    let screen = CreateDCW(
        display_name().as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
    );
    if screen == 0 {
        DestroyWindow(hwnd);
        return None;
    }

    // 三张位图：原始帧、压暗版、双缓冲目标。任何一步失败都要把已建的一并清掉。
    let made = (|| -> Option<(isize, isize, isize, isize, isize, isize)> {
        let (frame_bmp, frame_dc) = make_bitmap(screen, &pixels, w, h)?;
        let dimmed = dim_copy(&pixels);
        // 注意：这一步失败时 frame 已经建好了，必须显式回收，不能用 `?` 直接返回
        let (dim_bmp, dim_dc) = match make_bitmap(screen, &dimmed, w, h) {
            Some(v) => v,
            None => {
                DeleteObject(frame_bmp);
                DeleteDC(frame_dc);
                return None;
            }
        };
        // 双缓冲目标用 DDB 而不是 DIB：AlphaBlend 在 32bpp DIB 目标上会连 alpha
        // 通道一起写（圆角外写成 0），之后 BitBlt 到窗口时那圈 alpha=0 的像素
        // 会显示成黑边。DDB 没有 alpha 通道，混合结果只剩 RGB，正是我们要的。
        let mem_bmp = CreateCompatibleBitmap(screen, w, h);
        if mem_bmp == 0 {
            DeleteObject(frame_bmp);
            DeleteDC(frame_dc);
            DeleteObject(dim_bmp);
            DeleteDC(dim_dc);
            return None;
        }
        let mem_dc = CreateCompatibleDC(screen);
        if mem_dc == 0 {
            DeleteObject(frame_bmp);
            DeleteDC(frame_dc);
            DeleteObject(dim_bmp);
            DeleteDC(dim_dc);
            DeleteObject(mem_bmp);
            return None;
        }
        SelectObject(mem_dc, mem_bmp);
        Some((frame_bmp, frame_dc, dim_bmp, dim_dc, mem_bmp, mem_dc))
    })();
    DeleteDC(screen);

    let Some((frame_bmp, frame_dc, dim_bmp, dim_dc, mem_bmp, mem_dc)) = made else {
        DestroyWindow(hwnd);
        return None;
    };

    Some(Session {
        hwnd,
        origin_x: left,
        origin_y: top,
        width: w,
        height: h,
        pixels,
        windows,
        dim_bmp,
        dim_dc,
        frame_bmp,
        frame_dc,
        mem_bmp,
        mem_dc,
        sel: Sel::default(),
        mode: DragMode::Idle,
        down_x: 0,
        down_y: 0,
        down_window: None,
        hover_window: None,
        hover_bar: 0,
        committed: false,
    })
}

/// 用 `CreateDIBSection` 建一张可直接写像素的 32 位位图，并配一个内存 DC。
unsafe fn make_bitmap(screen_dc: isize, pixels: &[u8], w: i32, h: i32) -> Option<(isize, isize)> {
    let bmi = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: w,
            // 负高 = 自上而下，与 pixels 行序一致，memcpy 即可
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            size_image: (w as usize * h as usize * 4) as u32,
            x_ppm: 0,
            y_ppm: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0; 3],
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
    if bmp == 0 || bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
    let dc = CreateCompatibleDC(screen_dc);
    if dc == 0 {
        DeleteObject(bmp);
        return None;
    }
    SelectObject(dc, bmp);
    Some((bmp, dc))
}

/// 生成压暗版：每个通道乘 0.55。
///
/// 不每次重绘都做 `AlphaBlend`：拖拽期间每秒要重绘几十次，混叠的代价会堆起来。
/// 预生成一份后，重绘退化成两次 `BitBlt`（整屏压暗 + 选区还原），代价恒定。
fn dim_copy(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for px in pixels.chunks_exact(4) {
        out.push((px[0] as u32 * 55 / 100) as u8);
        out.push((px[1] as u32 * 55 / 100) as u8);
        out.push((px[2] as u32 * 55 / 100) as u8);
        out.push(255);
    }
    out
}

// ───────────────────────────── 窗口过程 ─────────────────────────────

extern "system" fn wnd_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        // 自己擦背景（WM_PAINT 里整屏铺满），否则会先刷白再画，闪一帧
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, loword(lp), hiword(lp));
            0
        }
        WM_LBUTTONDOWN => {
            on_mouse_down(hwnd, loword(lp), hiword(lp));
            0
        }
        WM_LBUTTONUP => {
            on_mouse_up(hwnd, loword(lp), hiword(lp));
            0
        }
        WM_LBUTTONDBLCLK => {
            commit(hwnd);
            0
        }
        WM_RBUTTONDOWN | WM_NATIVE_CANCEL => {
            cancel(hwnd);
            0
        }
        WM_NATIVE_COMMIT => {
            commit(hwnd);
            0
        }
        WM_SETCURSOR => {
            let cursor = with_session(|s| {
                let mut pt = crate::effects::Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                unsafe { GetCursorPos(&mut pt) };
                // POINT 与 Rect 前两个字段同布局，取 left/top 即光标的 x/y
                cursor_for(s, pt.left - s.origin_x, pt.top - s.origin_y)
            })
            .unwrap_or(IDC_CROSS);
            unsafe {
                SetCursor(LoadCursorW(0, cursor as *const u16));
            }
            1
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn loword(l: isize) -> i32 {
    (l as u32 & 0xFFFF) as u16 as i16 as i32
}
fn hiword(l: isize) -> i32 {
    ((l as u32 >> 16) & 0xFFFF) as u16 as i16 as i32
}

// ───────────────────────────── 命中测试 ─────────────────────────────

/// 手柄命中范围：视觉 6px，命中放宽到 12px（交接文档的规格）。
const HANDLE_HIT: i32 = 12;
/// 手柄视觉半边长（合计 6px）。
const HANDLE_HALF: i32 = 3;

fn handle_points(sel: &Sel) -> [(Handle, i32, i32); 8] {
    let (l, t, r, b) = (sel.left, sel.top, sel.right, sel.bottom);
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    [
        (Handle::NW, l, t),
        (Handle::N, cx, t),
        (Handle::NE, r, t),
        (Handle::E, r, cy),
        (Handle::SE, r, b),
        (Handle::S, cx, b),
        (Handle::SW, l, b),
        (Handle::W, l, cy),
    ]
}

fn hit_handle(sel: &Sel, x: i32, y: i32) -> Option<Handle> {
    if !sel.valid() {
        return None;
    }
    for (h, hx, hy) in handle_points(sel) {
        if (x - hx).abs() <= HANDLE_HIT && (y - hy).abs() <= HANDLE_HIT {
            return Some(h);
        }
    }
    None
}

fn cursor_for(s: &Session, x: i32, y: i32) -> usize {
    if s.sel.valid() {
        // 工具条是按钮，不是选区的一部分：光标移过去要变成箭头，
        // 否则一路十字准星走到按钮上，看不出「这里可以点」
        let bar = toolbar_rect(&s.sel, s.width, s.height);
        if bar.contains(x, y) {
            return IDC_ARROW;
        }
        if let Some(h) = hit_handle(&s.sel, x, y) {
            return match h {
                Handle::N | Handle::S => IDC_SIZENS,
                Handle::E | Handle::W => IDC_SIZEWE,
                Handle::NW | Handle::SE => IDC_SIZENWSE,
                Handle::NE | Handle::SW => IDC_SIZENESW,
            };
        }
        if s.sel.contains(x, y) {
            return IDC_SIZEALL;
        }
    }
    IDC_CROSS
}

// ───────────────────────────── 鼠标 ─────────────────────────────

/// 按下之后要做的动作。先算完再执行，避免持锁调用 `SetCapture` / 销毁窗口。
enum DownAction {
    Nothing,
    Cancel,
    Commit,
}

fn on_mouse_move(hwnd: isize, x: i32, y: i32) {
    let Some(changed) = with_session_mut(|s| {
        match s.mode {
            DragMode::Create => {
                s.sel = Sel::normalized(s.down_x, s.down_y, x, y);
                true
            }
            DragMode::Move {
                grab_x,
                grab_y,
                origin,
            } => {
                let dx = x - grab_x;
                let dy = y - grab_y;
                // 平移不许把选区推出屏幕：夹到边界，尺寸不变
                let left = (origin.left + dx).clamp(0, (s.width - origin.w()).max(0));
                let top = (origin.top + dy).clamp(0, (s.height - origin.h()).max(0));
                s.sel = Sel {
                    left,
                    top,
                    right: left + origin.w(),
                    bottom: top + origin.h(),
                };
                true
            }
            DragMode::Resize { handle, origin } => {
                s.sel = resize_to(origin, handle, x, y, s.width, s.height);
                true
            }
            DragMode::Idle => {
                if s.sel.valid() {
                    // 已有选区时鼠标可能落在工具条上：更新按钮悬停态，驱动底色与气泡。
                    // 这时不再高亮悬停窗口 —— 选区已定，再高亮只会分散注意。
                    let bar = toolbar_rect(&s.sel, s.width, s.height);
                    let hb = if bar.contains(x, y) {
                        if x < (bar.left + bar.right) / 2 {
                            1
                        } else {
                            2
                        }
                    } else {
                        0
                    };
                    let changed = hb != s.hover_bar;
                    s.hover_bar = hb;
                    changed
                } else {
                    // 空闲时更新悬停窗口，供「单击选中整个窗口」
                    let sx = s.origin_x + x;
                    let sy = s.origin_y + y;
                    let hit = s
                        .windows
                        .iter()
                        .find(|r| sx >= r[0] && sx < r[2] && sy >= r[1] && sy < r[3])
                        .map(|r| Sel::from_rect(*r, s.origin_x, s.origin_y));
                    let changed = hit != s.hover_window;
                    s.hover_window = hit;
                    changed
                }
            }
        }
    }) else {
        return;
    };

    if changed {
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }
}

/// 按手柄方向改选区：对边固定，只动被拖的那条边/角。
fn resize_to(origin: Sel, handle: Handle, x: i32, y: i32, max_w: i32, max_h: i32) -> Sel {
    let x = x.clamp(0, max_w);
    let y = y.clamp(0, max_h);
    let (mut l, mut t, mut r, mut b) = (origin.left, origin.top, origin.right, origin.bottom);
    match handle {
        Handle::NW => {
            l = x;
            t = y;
        }
        Handle::N => t = y,
        Handle::NE => {
            r = x;
            t = y;
        }
        Handle::E => r = x,
        Handle::SE => {
            r = x;
            b = y;
        }
        Handle::S => b = y,
        Handle::SW => {
            l = x;
            b = y;
        }
        Handle::W => l = x,
    }
    Sel::normalized(l, t, r, b)
}

fn on_mouse_down(hwnd: isize, x: i32, y: i32) {
    let action = with_session_mut(|s| {
        s.down_x = x;
        s.down_y = y;

        if s.sel.valid() {
            // 1. 工具条（只在已有选区时存在）
            let bar = toolbar_rect(&s.sel, s.width, s.height);
            if bar.contains(x, y) {
                let mid = (bar.left + bar.right) / 2;
                return if x < mid {
                    DownAction::Cancel
                } else {
                    DownAction::Commit
                };
            }
            // 2. 手柄
            if let Some(h) = hit_handle(&s.sel, x, y) {
                s.mode = DragMode::Resize {
                    handle: h,
                    origin: s.sel,
                };
                return DownAction::Nothing;
            }
            // 3. 选区内平移
            if s.sel.contains(x, y) {
                s.mode = DragMode::Move {
                    grab_x: x,
                    grab_y: y,
                    origin: s.sel,
                };
                return DownAction::Nothing;
            }
        }

        // 4. 其余位置：开始新框选。记下悬停窗口，供「单击选中」使用
        s.down_window = s.hover_window;
        s.mode = DragMode::Create;
        s.sel = Sel::default();
        DownAction::Nothing
    })
    .unwrap_or(DownAction::Nothing);

    match action {
        DownAction::Cancel => cancel(hwnd),
        DownAction::Commit => commit(hwnd),
        DownAction::Nothing => unsafe {
            SetCapture(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        },
    }
}

fn on_mouse_up(hwnd: isize, x: i32, y: i32) {
    let changed = with_session_mut(|s| {
        unsafe {
            ReleaseCapture();
        }
        let moved = (x - s.down_x).abs() >= 4 || (y - s.down_y).abs() >= 4;
        let was = s.mode;
        s.mode = DragMode::Idle;

        if was == DragMode::Create {
            if moved {
                if !s.sel.valid() {
                    s.sel = Sel::default();
                }
            } else {
                // 位移 < 4px 视为单击：选中悬停的窗口，**不直接提交**
                s.sel = s.down_window.unwrap_or_default();
            }
        }
        s.down_window = None;
        true
    })
    .unwrap_or(false);

    if changed {
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }
}

// ───────────────────────────── 工具条几何 ─────────────────────────────

const BAR_W: i32 = 76;
const BAR_H: i32 = 38;
const BAR_GAP: i32 = 8;

/// 工具条位置：默认贴选区右下外侧；底部放不下翻到上方；再不行压到屏幕内。
fn toolbar_rect(sel: &Sel, sw: i32, sh: i32) -> Sel {
    let mut left = sel.right - BAR_W;
    let mut top = sel.bottom + BAR_GAP;
    if top + BAR_H > sh {
        top = sel.top - BAR_GAP - BAR_H;
    }
    if top < 0 {
        top = (sh - BAR_H).max(0);
    }
    left = left.clamp(0, (sw - BAR_W).max(0));
    Sel {
        left,
        top,
        right: left + BAR_W,
        bottom: top + BAR_H,
    }
}

// ───────────────────────────── 绘制 ─────────────────────────────

fn paint(hwnd: isize) {
    unsafe {
        let mut ps = std::mem::zeroed::<PaintStruct>();
        let hdc = BeginPaint(hwnd, &mut ps);
        // 借用只在这段绘制内有效；作用域结束即释放锁，不需要显式 drop
        if let Ok(g) = SESSION.lock() {
            if let Some(s) = g.as_ref() {
                draw_frame(hdc, s);
            }
        }
        EndPaint(hwnd, &ps);
    }
}

/// 实际的绘制。调用方持锁。
unsafe fn draw_frame(hdc: isize, s: &Session) {
    // ── 1. 整屏压暗 + 选区还原（两次 BitBlt，顺序即效果）──
    BitBlt(s.mem_dc, 0, 0, s.width, s.height, s.dim_dc, 0, 0, SRCCOPY);
    if s.sel.valid() {
        BitBlt(
            s.mem_dc,
            s.sel.left,
            s.sel.top,
            s.sel.w(),
            s.sel.h(),
            s.frame_dc,
            s.sel.left,
            s.sel.top,
            SRCCOPY,
        );
    }

    // ── 2. 空闲时高亮悬停窗口（还没有选区）──
    if !s.sel.valid() {
        if let Some(hw) = s.hover_window {
            if hw.valid() {
                BitBlt(
                    s.mem_dc,
                    hw.left,
                    hw.top,
                    hw.w(),
                    hw.h(),
                    s.frame_dc,
                    hw.left,
                    hw.top,
                    SRCCOPY,
                );
                stroke_rect(s.mem_dc, &hw, 2, 0x0000_7AFF);
            }
        }
    }

    // ── 3. 选区边框 + 八向手柄 ──
    if s.sel.valid() {
        // 5px 橙色边框（COLORREF 是 0x00BBGGRR，0x00007AFF = #FF7A00）。
        // 向内描 5px，出图裁的是 pixels 里的原始像素，边框不会进到截图里。
        stroke_rect(s.mem_dc, &s.sel, 5, 0x0000_7AFF);
        draw_handles(s.mem_dc, &s.sel);
        draw_size_label(s.mem_dc, &s.sel);
        draw_toolbar(s.mem_dc, &s.sel, s.width, s.height, s.hover_bar);
        draw_bar_tip(s.mem_dc, s);
    }

    // ── 4. 一次性贴到窗口 ──
    BitBlt(hdc, 0, 0, s.width, s.height, s.mem_dc, 0, 0, SRCCOPY);
}

/// 描一个矩形边。用四条 `FillRect` 拼，比 `FrameRect` 更可控（能指定线宽与颜色）。
unsafe fn stroke_rect(hdc: isize, r: &Sel, width: i32, color: u32) {
    let brush = CreateSolidBrush(color);
    let parts = [
        Sel {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.top + width,
        },
        Sel {
            left: r.left,
            top: r.bottom - width,
            right: r.right,
            bottom: r.bottom,
        },
        Sel {
            left: r.left,
            top: r.top,
            right: r.left + width,
            bottom: r.bottom,
        },
        Sel {
            left: r.right - width,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        },
    ];
    for p in parts.iter() {
        let wr = p.to_win();
        FillRect(hdc, &wr, brush);
    }
    DeleteObject(brush);
}

unsafe fn fill_rect(hdc: isize, r: &Sel, color: u32) {
    let brush = CreateSolidBrush(color);
    let wr = r.to_win();
    FillRect(hdc, &wr, brush);
    DeleteObject(brush);
}

/// 八个手柄：6px 白块 + 1px 深色描边，在亮底与暗底上都有边界。
unsafe fn draw_handles(hdc: isize, sel: &Sel) {
    for (_, cx, cy) in handle_points(sel) {
        let r = Sel {
            left: cx - HANDLE_HALF,
            top: cy - HANDLE_HALF,
            right: cx + HANDLE_HALF,
            bottom: cy + HANDLE_HALF,
        };
        fill_rect(hdc, &r, 0x00FF_FFFF);
        stroke_rect(hdc, &r, 1, 0x0040_4040);
    }
}

/// 选区上方的「宽 × 高」角标（物理像素）。贴顶边时压到选区内部顶端。
unsafe fn draw_size_label(hdc: isize, sel: &Sel) {
    let text = format!("{} × {}", sel.w(), sel.h());
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    buf.push(0);

    let box_w = (text.chars().count() as i32) * 8 + 12;
    let box_h = 20;
    let mut top = sel.top - box_h - 4;
    if top < 0 {
        top = sel.top + 4;
    }
    let r = Sel {
        left: sel.left,
        top,
        right: sel.left + box_w,
        bottom: top + box_h,
    };

    fill_rect(hdc, &r, 0x0020_2020);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, 0x00FF_FFFF);
    let mut wr = r.to_win();
    DrawTextW(
        hdc,
        buf.as_ptr(),
        -1,
        &mut wr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

/// 工具条：白底圆角，左半红叉取消、右半绿勾完成。
///
/// 整张图**逐像素生成**（含抗锯齿）后用 `AlphaBlend` 贴上去。
/// 不用 GDI 的 `RoundRect` + `LineTo`：这两条 API 都没有抗锯齿，圆角与 45° 斜线
/// 全是硬边，在 76×38 这个小尺寸上就是「歪歪斜斜、锯齿很重」的来源。
/// 把一张**预乘 alpha** 的 BGRA 图混合到目标 DC 的指定矩形。失败返回 false。
///
/// 预乘是硬要求：GDI 的 `AlphaBlend` 按预乘语义解释源位图，给 straight alpha
/// 会在边缘带出一圈黑边。目标必须是 DDB —— 32bpp DIB 目标会让 `AlphaBlend`
/// 连 alpha 通道一起写，之后 `BitBlt` 到窗口时那圈 alpha=0 显示成黑。
unsafe fn blend_argb(hdc: isize, r: &Sel, pixels: &[u8]) -> bool {
    let (w, h) = (r.w(), r.h());
    if w <= 0 || h <= 0 || pixels.len() < (w as usize * h as usize * 4) {
        return false;
    }
    let bmi = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: w,
            // 负高 = 自上而下，与 pixels 行序一致，memcpy 即可
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            size_image: (w as usize * h as usize * 4) as u32,
            x_ppm: 0,
            y_ppm: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0; 3],
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
    if bmp == 0 || bits.is_null() {
        return false;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());

    let dc = CreateCompatibleDC(hdc);
    if dc == 0 {
        DeleteObject(bmp);
        return false;
    }
    let old = SelectObject(dc, bmp);
    // AC_SRC_OVER + AC_SRC_ALPHA：按源图 alpha 混合，圆角边缘才能平滑过渡
    let bf = BlendFunction {
        blend_op: 0,
        blend_flags: 0,
        source_constant_alpha: 255,
        alpha_format: 1,
    };
    AlphaBlend(hdc, r.left, r.top, w, h, dc, 0, 0, w, h, bf);

    SelectObject(dc, old);
    DeleteDC(dc);
    DeleteObject(bmp);
    true
}

unsafe fn draw_toolbar(hdc: isize, sel: &Sel, sw: i32, sh: i32, hover: u8) {
    let bar = toolbar_rect(sel, sw, sh);
    if !blend_argb(hdc, &bar, toolbar_pixels(hover)) {
        // 兜底：退化成直角白块。工具条能点、只是不好看，比整块不画强
        fill_rect(hdc, &bar, 0x00FF_FFFF);
    }
}

// ───────────────────────────── 按钮气泡 ─────────────────────────────

const TIP_W: i32 = 46;
const TIP_H: i32 = 26;
const TIP_GAP: i32 = 6;

/// 气泡位置：贴在工具条外侧，水平居中于被悬停的半区。
/// 优先放工具条下方（不压选区），下方不够再翻到上方。
fn tip_rect(bar: &Sel, hover: u8, sw: i32, sh: i32) -> Sel {
    let mid = (bar.left + bar.right) / 2;
    let cx = if hover == 1 {
        (bar.left + mid) / 2
    } else {
        (mid + bar.right) / 2
    };
    let mut top = bar.bottom + TIP_GAP;
    if top + TIP_H > sh {
        top = bar.top - TIP_GAP - TIP_H;
    }
    if top < 0 {
        top = (sh - TIP_H).max(0);
    }
    let left = (cx - TIP_W / 2).clamp(0, (sw - TIP_W).max(0));
    Sel {
        left,
        top,
        right: left + TIP_W,
        bottom: top + TIP_H,
    }
}

/// 深色圆角气泡底：逐像素 + 预乘 alpha，与工具条同一套做法。
fn build_tip_pixels(w: i32, h: i32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    let radius = 7.0f32;
    // 0.88 不透明度：压在截图内容上要读得清，又不能像实心块那样突兀
    let alpha = 224u32;
    let base = 28u32;
    for y in 0..h {
        for x in 0..w {
            let cover = round_cover(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
            if cover <= 0.0 {
                continue;
            }
            let a = (cover * alpha as f32).round() as u32;
            let i = ((y * w + x) * 4) as usize;
            px[i] = ((base * a + 127) / 255) as u8;
            px[i + 1] = ((base * a + 127) / 255) as u8;
            px[i + 2] = ((base * a + 127) / 255) as u8;
            px[i + 3] = a as u8;
        }
    }
    px
}

fn tip_pixels() -> &'static [u8] {
    static CACHE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| build_tip_pixels(TIP_W, TIP_H))
}

/// 画按钮文字。气泡底色是逐像素生成的，文字只能交给 GDI 排版 ——
/// 中文字形没法像线段那样自己算覆盖率。
unsafe fn draw_tip_text(hdc: isize, tip: &Sel, text: &str) {
    unsafe extern "system" {
        #[allow(clippy::too_many_arguments)]
        fn CreateFontW(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strike: u32,
            charset: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_family: u32,
            face: *const u16,
        ) -> isize;
        fn SetTextColor(hdc: isize, color: u32) -> u32;
        fn SetBkMode(hdc: isize, mode: i32) -> i32;
        fn DrawTextW(
            hdc: isize,
            text: *const u16,
            len: i32,
            rect: *mut crate::effects::Rect,
            format: u32,
        ) -> i32;
    }
    let face: Vec<u16> = "Microsoft YaHei UI"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // -13：12px 在这个尺寸下偏小，两个汉字容易挤成一团
    let font = CreateFontW(-13, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    if font == 0 {
        return;
    }
    let old = SelectObject(hdc, font);
    SetBkMode(hdc, 1); // TRANSPARENT：否则文字背后会刷一块底色
    SetTextColor(hdc, 0x00FF_FFFF); // 白。COLORREF 是 0x00BBGGRR
    let mut rc = crate::effects::Rect {
        left: tip.left,
        top: tip.top,
        right: tip.right,
        bottom: tip.bottom,
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(
        hdc,
        units.as_ptr(),
        units.len() as i32,
        &mut rc,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, old);
    DeleteObject(font);
}

/// 悬停气泡：先铺底色再画字，顺序不能反。
unsafe fn draw_bar_tip(hdc: isize, s: &Session) {
    if s.hover_bar == 0 {
        return;
    }
    let bar = toolbar_rect(&s.sel, s.width, s.height);
    let tip = tip_rect(&bar, s.hover_bar, s.width, s.height);
    if !blend_argb(hdc, &tip, tip_pixels()) {
        // 底色都画不出来就别画字了，散落在截图上只会是脏点
        return;
    }
    let text = if s.hover_bar == 1 { "取消" } else { "完成" };
    draw_tip_text(hdc, &tip, text);
}

/// 工具条图像按三种悬停状态各生成一次：内容全是常量，尺寸也固定。
fn toolbar_pixels(hover: u8) -> &'static [u8] {
    static NONE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static LEFT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static RIGHT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    match hover {
        1 => LEFT.get_or_init(|| build_toolbar_pixels(BAR_W, BAR_H, 1)),
        2 => RIGHT.get_or_init(|| build_toolbar_pixels(BAR_W, BAR_H, 2)),
        _ => NONE.get_or_init(|| build_toolbar_pixels(BAR_W, BAR_H, 0)),
    }
}

/// 逐像素画工具条（BGRA，自上而下）。
///
/// 每一层都按覆盖率混合：圆角 8px、线宽 2px 都带 1px 过渡，观感接近超采样，
/// 但只在首次绘制时算一遍（随后走 `OnceLock` 缓存）。
fn build_toolbar_pixels(w: i32, h: i32, hover: u8) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    let radius = 8.0f32;
    let mid = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let arm = 7.0f32;
    // 图标落在各自半区的正中：按中缝偏移算会让两侧看起来不对称
    let lx = mid / 2.0;
    let rx = mid + mid / 2.0;
    let sep_top = 9.0f32;
    let sep_bot = h as f32 - 9.0;

    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let cover = round_cover(fx, fy, w as f32, h as f32, radius);
            if cover <= 0.0 {
                continue;
            }

            let (mut r, mut g, mut b) = (255.0f32, 255.0f32, 255.0f32);

            // 悬停底色：只铺在被悬停的那半区，左右各内缩 3px、圆角 6。
            // 色值取 236 而不是更深的灰 —— 图标本身是饱和红/绿，底色再重会抢掉它；
            // 同时 236 与纯白（255）的差在压暗背景上仍然一眼可辨。
            if hover != 0 {
                let (hl, hr) = if hover == 1 {
                    (3.0f32, mid - 1.5)
                } else {
                    (mid + 1.5, w as f32 - 3.0)
                };
                let c = round_cover(fx - hl, fy - 3.0, hr - hl, (h - 6) as f32, 6.0);
                if c > 0.0 {
                    r = lerp(r, 236.0, c);
                    g = lerp(g, 236.0, c);
                    b = lerp(b, 236.0, c);
                }
            }

            // 中缝分隔线
            let in_sep = if fy > sep_top && fy < sep_bot { 1.0 } else { 0.0 };
            let sep = stroke_cover((fx - mid).abs(), 0.5) * in_sep;
            if sep > 0.0 {
                r = lerp(r, 226.0, sep);
                g = lerp(g, 226.0, sep);
                b = lerp(b, 226.0, sep);
            }

            // 红叉：两条对角线
            let d = seg_dist(fx, fy, lx - arm, cy - arm, lx + arm, cy + arm).min(seg_dist(
                fx,
                fy,
                lx + arm,
                cy - arm,
                lx - arm,
                cy + arm,
            ));
            let t = stroke_cover(d, 1.0);
            if t > 0.0 {
                r = lerp(r, 224.0, t);
                g = lerp(g, 50.0, t);
                b = lerp(b, 50.0, t);
            }

            // 绿勾：两段折线
            let d = seg_dist(fx, fy, rx - 7.0, cy, rx - 2.0, cy + 5.0).min(seg_dist(
                fx,
                fy,
                rx - 2.0,
                cy + 5.0,
                rx + 7.0,
                cy - 6.0,
            ));
            let t = stroke_cover(d, 1.0);
            if t > 0.0 {
                r = lerp(r, 22.0, t);
                g = lerp(g, 158.0, t);
                b = lerp(b, 22.0, t);
            }

            let i = ((y * w + x) * 4) as usize;
            // **GDI 的 AlphaBlend 按「预乘 alpha」解释源位图**：RGB 必须先乘上 alpha。
            // 直接给 straight alpha（RGB 保持原值、alpha 单独给）时，边缘那些
            // alpha=0.5 的像素会被当成「已经预乘过」的亮值参与合成 —— 在深色背景上
            // 就是一圈黑边/暗晕。同理，全透明区域必须 RGB 清零。
            let a = (cover * 255.0).round() as u32;
            px[i] = ((b as u32 * a + 127) / 255) as u8;
            px[i + 1] = ((g as u32 * a + 127) / 255) as u8;
            px[i + 2] = ((r as u32 * a + 127) / 255) as u8;
            px[i + 3] = a as u8;
        }
    }
    px
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 圆角矩形覆盖率：离圆角圆心越远越透明，边缘留 1px 过渡。
fn round_cover(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let cx = x.clamp(r, w - r);
    let cy = y.clamp(r, h - r);
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    (r + 0.5 - d).clamp(0.0, 1.0)
}

/// 点到线段的距离。
fn seg_dist(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len2 = dx * dx + dy * dy;
    if len2 <= f32::EPSILON {
        return ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
    }
    let t = (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0);
    let (qx, qy) = (x1 + t * dx, y1 + t * dy);
    ((px - qx).powi(2) + (py - qy).powi(2)).sqrt()
}

/// 线宽 2（半宽 1）的覆盖率。
fn stroke_cover(d: f32, half: f32) -> f32 {
    (half + 0.5 - d).clamp(0.0, 1.0)
}

// ───────────────────────────── 提交 / 取消 ─────────────────────────────

fn commit(hwnd: isize) {
    let Some((dib, w, h)) = with_session_mut(|s| {
        if s.committed || !s.sel.valid() {
            return None;
        }
        match crop_to_dib(&s.pixels, s.width, s.height, &s.sel) {
            Ok(d) => {
                s.committed = true;
                Some((d, s.sel.w(), s.sel.h()))
            }
            Err(e) => {
                crate::log_warn!("[shot] 裁图失败：{e}");
                None
            }
        }
    })
    .flatten()
    else {
        return;
    };

    crate::log_print!("[shot] 已复制选区 {w}×{h}（{} KB DIB）", dib.len() / 1024);

    // 写剪贴板可能被别的程序短暂占用，放独立线程，别卡住消息循环
    std::thread::spawn(move || {
        // 稍等一帧让覆盖层先销毁：否则某些「跟随截图」的工具可能抓到它
        std::thread::sleep(std::time::Duration::from_millis(30));
        if let Err(e) = crate::clipboard::write_shot_to_clipboard(&dib) {
            crate::log_err!("shot", "写剪贴板失败：{e}");
        }
    });

    unsafe {
        DestroyWindow(hwnd);
    }
}

fn cancel(hwnd: isize) {
    crate::log_print!("[shot] 用户取消");
    unsafe {
        DestroyWindow(hwnd);
    }
}

/// 从冻结帧裁出选区，拼成 CF_DIB（BITMAPINFOHEADER + 自下而上 BGRA）。
///
/// 与 WebView 版共用同一条出图路径：裁的是原始像素，不是压暗过的预览图。
fn crop_to_dib(pixels: &[u8], img_w: i32, img_h: i32, sel: &Sel) -> Result<Vec<u8>, String> {
    let x = sel.left.clamp(0, img_w);
    let y = sel.top.clamp(0, img_h);
    let w = sel.w().clamp(0, img_w - x);
    let h = sel.h().clamp(0, img_h - y);
    if w <= 0 || h <= 0 {
        return Err("选区为空".into());
    }

    let row_bytes = (w as usize) * 4;
    let size_image = (row_bytes * h as usize) as u32;
    let mut dib = Vec::with_capacity(40 + row_bytes * h as usize);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight：正 = 自下而上
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&size_image.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // 源自上而下、DIB 自下而上 → 行倒序拷
    for row in (0..h).rev() {
        let sy = (y + row) as usize;
        let start = (sy * img_w as usize + x as usize) * 4;
        dib.extend_from_slice(&pixels[start..start + row_bytes]);
    }
    Ok(dib)
}

// ───────────────────────────── 键盘钩子 ─────────────────────────────

/// Esc / Enter 只能从低级键盘钩子截下来再 `PostMessageW` 回窗口：
/// NOACTIVATE 窗口没有焦点，永远收不到 `WM_KEYDOWN`。
unsafe fn install_keyboard_hook() -> isize {
    let module = GetModuleHandleW(std::ptr::null());
    SetWindowsHookExW(WH_KEYBOARD_LL, kb_proc, module, 0)
}

unsafe extern "system" fn kb_proc(code: i32, w: usize, l: isize) -> isize {
    if code == HC_ACTION {
        let k = &*(l as *const KbdLlHookStruct);
        let is_down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
        if is_down {
            let hwnd = KB_TARGET.load(Ordering::SeqCst);
            if hwnd != 0 {
                if k.vk_code == VK_ESCAPE {
                    PostMessageW(hwnd, WM_NATIVE_CANCEL, 0, 0);
                    return 1; // 吞掉，别让 Esc 再传给前台程序
                }
                if k.vk_code == VK_RETURN {
                    PostMessageW(hwnd, WM_NATIVE_COMMIT, 0, 0);
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(0, code, w, l)
}

// ───────────────────────────── 抓屏与窗口枚举 ─────────────────────────────

fn display_name() -> Vec<u16> {
    "DISPLAY".encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 抓一块屏幕区域，返回自上而下的 BGRA（alpha 补 255）。
unsafe fn grab(left: i32, top: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    let screen = CreateDCW(
        display_name().as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
    );
    if screen == 0 {
        return None;
    }
    let mem = CreateCompatibleDC(screen);
    if mem == 0 {
        DeleteDC(screen);
        return None;
    }
    let bmp = CreateCompatibleBitmap(screen, w, h);
    if bmp == 0 {
        DeleteDC(mem);
        DeleteDC(screen);
        return None;
    }
    let old = SelectObject(mem, bmp);

    // 源坐标用**虚拟屏**坐标：多屏时主屏可能不在 (0,0)
    let blit = BitBlt(mem, 0, 0, w, h, screen, left, top, SRCCOPY);

    let mut ok = false;
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    if blit != 0 {
        let mut bi = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: w,
                // 负高 = 自上而下。取正的话缓冲区是倒的，裁剪得反着算行号，
                // 而 CF_DIB 又要求倒序，两处翻转极易写错。
                height: -h,
                planes: 1,
                bit_count: 32,
                compression: BI_RGB,
                size_image: buf.len() as u32,
                x_ppm: 0,
                y_ppm: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0; 3],
        };
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr(),
            &mut bi,
            DIB_RGB_COLORS,
        );
        ok = lines == h;
    }

    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    DeleteDC(screen);

    if !ok {
        return None;
    }
    // GDI 不维护 alpha，第 4 字节是未初始化值；统一补 255，
    // 否则粘到接受 alpha 的程序里会整张半透明或黑底。
    for px in buf.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(buf)
}

const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW_ISIZE: isize = WS_EX_TOOLWINDOW as isize;
const DWMWA_CLOAKED: u32 = 14;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

struct EnumCtx {
    out: Vec<[i32; 4]>,
}

/// 枚举「用户看得见、点得到」的顶层窗口。过滤条件与 WebView 版一致：
/// 不可见 / 最小化、`WS_EX_TOOLWINDOW`、UWP cloaked、无标题、桌面与任务栏类。
///
/// 不按进程过滤：轻贴自己的主窗口与快捷面板都是用户可能想截的对象。
unsafe fn list_windows() -> Vec<[i32; 4]> {
    let mut ctx = EnumCtx { out: Vec::new() };
    EnumWindows(enum_proc, &mut ctx as *mut EnumCtx as isize);
    ctx.out
}

extern "system" fn enum_proc(hwnd: isize, param: isize) -> i32 {
    unsafe {
        let ctx = &mut *(param as *mut EnumCtx);
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return 1;
        }
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW_ISIZE != 0 {
            return 1;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n > 0 {
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if matches!(
                class.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            ) {
                return 1;
            }
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            4,
        ) == 0
            && cloaked != 0
        {
            return 1;
        }
        // 优先用 DWM 的可见边框：`GetWindowRect` 会把 Win10+ 那圈不可见缩放边框
        // 和阴影一起算进去，截出来四周多几条边。
        let mut r = WinRect::default();
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut WinRect as *mut std::ffi::c_void,
            16,
        ) == 0;
        if !ok && GetWindowRect(hwnd, &mut r) == 0 {
            return 1;
        }
        if r.right - r.left <= 0 || r.bottom - r.top <= 0 {
            return 1;
        }
        ctx.out.push([r.left, r.top, r.right, r.bottom]);
        1
    }
}
