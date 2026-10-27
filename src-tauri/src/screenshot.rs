//! 自研截图（S1）：冻结屏幕 → 覆盖层框选 → 复制进剪贴板。
//!
//! **为什么自己画覆盖层**：系统截图（`ms-screenclip:`）的工具条样式、取消方式、
//! 甚至 Win10 与 Win11 的窗口行为都各是一套，用户拿到的交互不受我们控制。
//! 详见 DESIGN §12.7 的分期与取舍。
//!
//! **S1 的范围刻意很小**：框选 + 尺寸 + 复制 + 退出，**不做任何标注**。
//! 一期就要能独立用，所以宁可少做，也不留半成品功能。
//!
//! 三个必须按顺序做的动作（顺序错了会截到自己或者截到旧画面）：
//!
//! 1. **先抓屏，再显示窗口**。反过来会把还在淡入的覆盖层截进图里。
//! 2. **抓的是鼠标所在的那块显示器**（物理坐标）。S1 不做跨屏选区：混合 DPI 下
//!    一个跨屏窗口只能取一个缩放比例，另一个屏的像素会错位。
//! 3. **窗口按物理像素铺满该显示器**，并把 webview 缩放设成 `1/scale`，
//!    使 1 CSS px = 1 物理 px —— 前端算出来的选区坐标才能直接当像素坐标用。

use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

pub const SHOT_LABEL: &str = "shot";

/// 虚拟屏抓取用的设备名。`CreateDC` 传 "DISPLAY" 即整个桌面。
fn display_name() -> Vec<u16> {
    "DISPLAY".encode_utf16().chain(std::iter::once(0)).collect()
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

/// `GetDIBits` 要求传入完整的 `BITMAPINFO`。32 位色不需要调色板项，
/// 但结构体要给调色板留出位置（系统按 `biBitCount` 决定读多少）。
#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 3],
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
}

extern "system" {
    fn CreateDCW(driver: *const u16, device: *const u16, port: *const u16, init: *const u16) -> isize;
    fn CreateCompatibleDC(hdc: isize) -> isize;
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
    fn SetWindowsHookExW(id: i32, proc: HookProc, module: isize, thread: u32) -> isize;
    fn UnhookWindowsHookEx(hook: isize) -> i32;
    fn CallNextHookEx(hook: isize, code: i32, w: usize, l: isize) -> isize;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GetLastError() -> u32;
    /// 64 位必须用 Ptr 版本：`SetWindowLongW` 只能改 32 位，窗口样式会被截断
    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
    fn SetWindowPos(
        hwnd: isize,
        after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    /// 返回值最高位 = 当前是否按下；低字节的「曾被按过」不可用于判断
    fn GetAsyncKeyState(vk: i32) -> i16;
}

const SRCCOPY: u32 = 0x00CC_0020;
const DIB_RGB_COLORS: u32 = 0;

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const WM_MOUSEMOVE: usize = 0x0200;
const WM_LBUTTONDOWN: usize = 0x0201;
const WM_LBUTTONUP: usize = 0x0202;
const WM_RBUTTONDOWN: usize = 0x0204;
const HC_ACTION: i32 = 0;
const WM_KEYDOWN: usize = 0x0100;
const WM_SYSKEYDOWN: usize = 0x0104;
const VK_ESCAPE: u32 = 0x1B;
const VK_RETURN: u32 = 0x0D;
const VK_LBUTTON: i32 = 0x01;

/// 让热键线程装上 Esc 兜底钩子（`WM_APP + 4`）
pub const WM_SHOT_GUARD_ON: u32 = 0x8000 + 4;
/// 卸掉 Esc 兜底钩子（`WM_APP + 5`）
pub const WM_SHOT_GUARD_OFF: u32 = 0x8000 + 5;

type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

/// 低级鼠标钩子的参数。只读 `pt`，但结构体按真实布局定义 —— 少写一个字段
/// 不影响读 `pt`，多写错一个字段就会读到垃圾。
#[repr(C)]
struct MsllHookStruct {
    pt: Point,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

/// Esc 兜底钩子句柄。0 = 未装。
static GUARD: AtomicIsize = AtomicIsize::new(0);
/// 钩子回调拿不到 `AppHandle`，这里存一份最近的。
static SHOT_APP: Mutex<Option<AppHandle>> = Mutex::new(None);

/// 最近一次抓屏的结果。
///
/// `pixels` 是**自上而下**的 BGRA（行 0 = 屏幕顶行），alpha 已补成 255。
/// GDI 出来的位图 alpha 通道是垃圾值，不补 255 的话粘到某些程序里会变半透明。
struct Shot {
    w: u32,
    h: u32,
    /// 该显示器左上角在虚拟屏坐标里的物理位置（用于把选区坐标换算成屏幕坐标，
    /// S1 只在单屏内框选，所以仅用于日志与后续扩展）
    left: i32,
    top: i32,
    pixels: Vec<u8>,
    /// 编码后的**预览图**（JPEG data URL）。**同一张图只编一次**：前端为了防
    /// 「事件早于页面加载」会主动拉一次图，可能连拉两次。
    preview: Option<String>,
    /// 光标下可选的顶层窗口矩形（虚拟屏物理坐标，按 z 序从高到低）。
    ///
    /// **必须在覆盖层显示之前枚举**：覆盖层一铺满整屏，
    /// `WindowFromPoint` 就只会返回它自己，之后再也问不出底下是谁。
    windows: Vec<[i32; 4]>,
}

static LAST: Mutex<Option<Shot>> = Mutex::new(None);

/// `Alt+Q` 的入口：抓屏 → 铺满鼠标所在显示器 → 通知前端画图。
pub fn start(app: &AppHandle) {
    START_MS.store(now_ms(), Ordering::SeqCst);
    // 新会话：把「上次怎么结束的」归零，否则重复提交的判据会把这一次也当成已提交
    LAST_END.store(END_NONE, Ordering::SeqCst);
    // 新会话：把上一轮记下的按下清掉，否则前端会拿几秒前的旧坐标当框选起点
    if let Ok(mut g) = LAST_DOWN.lock() {
        *g = None;
    }
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    crate::log_print!("[shot] 会话 #{seq} 开始");
    // 抓屏前确保覆盖层是隐藏的：它平时就该是隐藏的，但万一上一次异常退出没收拾
    // 干净，这里先收一下，避免把「自己的残影」截进图里。
    if let Some(w) = app.get_webview_window(SHOT_LABEL) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }

    let Some(mon) = monitor_at_cursor(app) else {
        crate::log_warn!("[shot] 找不到鼠标所在的显示器，截图取消");
        return;
    };
    let pos = mon.position();
    let size = mon.size();
    let (left, top, w, h) = (pos.x, pos.y, size.width as i32, size.height as i32);
    if w <= 0 || h <= 0 {
        crate::log_warn!("[shot] 显示器尺寸异常（{w}×{h}），截图取消");
        return;
    }

    let t0 = std::time::Instant::now();
    let Some(pixels) = (unsafe { grab(left, top, w, h) }) else {
        crate::log_warn!("[shot] 抓屏失败（BitBlt/GetDIBits），截图取消");
        return;
    };
    crate::log_print!(
        "[shot] 抓屏完成 {w}×{h} @{left},{top}，{} KB，耗时 {}ms",
        pixels.len() / 1024,
        t0.elapsed().as_millis()
    );

    // 兜底钩子要一路挂到本次截图结束：先存 AppHandle，再让热键线程装上
    if let Ok(mut g) = SHOT_APP.lock() {
        *g = Some(app.clone());
    }
    guard_on();

    // 赶在覆盖层显示之前把窗口列表取下来（见 `Shot::windows` 的说明）
    let windows = unsafe { list_windows() };
    crate::log_print!("[shot] 可识别的窗口 {} 个", windows.len());
    if let Ok(mut g) = LAST.lock() {
        *g = Some(Shot {
            w: w as u32,
            h: h as u32,
            left,
            top,
            pixels,
            preview: None,
            windows,
        });
    }

    let scale = mon.scale_factor().max(0.1);
    let app2 = app.clone();
    // 「首次打开后立刻按快捷键」慢，多半是没赶上预热（启动 3 秒后才建窗口）。
    // 记一行，看到它就能确定是预热没到，而不是抓屏变慢了。
    if app.get_webview_window(SHOT_LABEL).is_none() {
        crate::log_print!("[shot] 覆盖层尚未预热，本次现建（首次按下会慢一点）");
    }
    // 窗口必须在主线程创建/移动
    if let Err(e) = app.run_on_main_thread(move || prepare_overlay(&app2, left, top, w, h, scale)) {
        crate::log_warn!("[shot] 切到主线程失败：{e}");
        return;
    }

    // 通知前端自己去拉图；**图加载完再显示窗口**，否则会先闪一帧空白/白底。
    let _ = app.emit_to(SHOT_LABEL, "shot-open", ());

    // 安全兜底：前端若没能回调 `shot_show`（页面没加载、JS 报错、IPC 卡住），
    // **直接取消整次截图**，绝不把黑窗口亮给用户。
    //
    // 这一条是被真实事故换来的：早先的实现是「没就绪就照样 show」，结果前端加载失败时
    // 用户看到的是全屏纯黑、按 Esc 也没反应（Esc 由前端处理），只能杀进程。
    // 截图失败可以重来，把用户困在全屏黑窗里不行。
    //
    // 3 秒而不是 1 秒：**首次**截图要现建 WebView（约 1–2 秒），1.2 秒会把正常的首次
    // 截图误杀（用户实测撞到了）。这里宁可等久一点，也不要让「第一次按快捷键没反应」。
    let app3 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(3000));
        let pending = LAST.lock().map(|g| g.is_some()).unwrap_or(false);
        if !pending {
            return; // 已经 commit / cancel 过了
        }
        let visible = app3
            .get_webview_window(SHOT_LABEL)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if !visible {
            crate::log_warn!("[shot] 前端 3 秒内未就绪，取消本次截图（窗口保持隐藏）");
            LAST_END.store(END_TIMEOUT, Ordering::SeqCst);
            guard_off();
            if let Ok(mut g) = LAST.lock() {
                *g = None;
            }
        }
    });
}

fn monitor_at_cursor(app: &AppHandle) -> Option<tauri::Monitor> {
    // 跟随鼠标：截图几乎总是「截眼前这块屏」。拿不到光标位置就退回主显示器。
    let pt = unsafe {
        let mut p = crate::effects::Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetCursorPos(&mut p) == 0 {
            return app.primary_monitor().ok().flatten();
        }
        PhysicalPosition::new(p.left as f64, p.top as f64)
    };
    app.monitor_from_point(pt.x, pt.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
}

extern "system" {
    // 与 `panel` 里那份声明同签名：同类型声明不会互相打架，
    // 但两个模块都要用，各自声明一次比引入一个公共 win32 模块改动面小。
    fn GetCursorPos(pt: *mut crate::effects::Rect) -> i32;
}

fn prepare_overlay(app: &AppHandle, left: i32, top: i32, w: i32, h: i32, scale: f64) {
    let Some(win) = build_overlay(app) else {
        return;
    };

    fit_client_to_screen(&win, left, top, w, h);
    // 1 CSS px = 1 物理 px，前端算出的选区坐标就等同于像素坐标
    let _ = win.set_zoom(1.0 / scale);
    disable_window_transitions(&win);
    if let Ok(h) = win.hwnd() {
        never_focus(h.0 as isize);
        SHOT_HWND.store(h.0 as isize, Ordering::SeqCst);
    }
}

/// 让覆盖层**永远不被激活**（`WS_EX_NOACTIVATE` + `WS_EX_TOOLWINDOW`）。
///
/// 这是「点一次才能开始框选」的终极解法：不再和系统争前台，而是让窗口根本不需要前台。
///
/// 之前的思路全是「把前台抢过来」—— `set_focus`、`AttachThreadInput`、注入 Alt，
/// 但 `SetForegroundWindow` 对后台进程本就是**静默拒绝**，没有一条能保证。而窗口只要
/// 不是前台，第一次 `pointerdown` 就会被系统拿去激活，拖动自然没反应。
///
/// `WS_EX_NOACTIVATE` 是窗口**属性**而不是显示时的参数：系统从此不再激活它，
/// **连点击也不激活**。于是第一次点击直接变成 `pointerdown`，不再被吃掉。
/// 同类工具（winshotx 的 `TRAMPAS.md` 第 9 条）走的就是这条路。
///
/// 代价：永不获焦也就收不到键盘。Esc / Enter 因此下沉到 `key_proc`（全局键盘钩子），
/// 不依赖窗口焦点 —— 这本来也是「逃生通道不依赖最易坏链路」的同一条原则。
fn never_focus(hwnd: isize) {
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_NOACTIVATE: isize = 0x0800_0000;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    // SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED
    const SWP_FLAGS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0010 | 0x0020;
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let next = cur | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        if next != cur {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
        }
        // 改完扩展样式必须让系统重算一次窗口框架，否则新样式可能不生效 ——
        // 「明明设了 NOACTIVATE 却还被激活」十有八九是漏了这一步。
        SetWindowPos(hwnd, 0, 0, 0, 0, 0, SWP_FLAGS);
        let now = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let ok = now & WS_EX_NOACTIVATE != 0;
        crate::log_print!(
            "[shot] 覆盖层样式 {cur:#x} → {now:#x}（NOACTIVATE {}，TOOLWINDOW {}）",
            if ok { "已生效" } else { "⚠ 未生效" },
            if now & WS_EX_TOOLWINDOW != 0 { "有" } else { "无" }
        );
    }
}

/// 创建（或取出）覆盖层窗口。
///
/// 抽成独立函数是为了 `prewarm`：预热和真正截图走同一套创建参数，
/// 两边各写一份必然漂移（少一个 `transparent(false)` 就会变成半透明窗口）。
fn build_overlay(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(SHOT_LABEL) {
        return Some(w);
    }
    let mut b = WebviewWindowBuilder::new(app, SHOT_LABEL, WebviewUrl::App("shot.html".into()))
        .title("轻贴截图")
        .decorations(false)
        // 不透明：整个窗口就是那张冻结的屏幕图，透明反而会露出下面还在动的桌面
        .transparent(false)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        // 构造期不抢焦点。真正的保证在 `never_focus()`（窗口样式），这一项只在
        // 首次创建时起作用，复用窗口时无效 —— 所以不能只靠它。
        .focused(false);
    if let Some(args) = crate::popup::browser_args(app) {
        b = b.additional_browser_args(&args);
    }
    match b.build() {
        Ok(w) => {
            disable_window_transitions(&w);
            Some(w)
        }
        Err(e) => {
            crate::log_err!("screenshot", "创建截图覆盖层失败：{e}");
            None
        }
    }
}

/// 预热：启动后把覆盖层窗口建好并隐藏着，让第一次按快捷键不用等 WebView 创建。
///
/// 代价是常驻一个隐藏的 WebView。它与主窗口**共享同一个浏览器进程**（同 user data
/// 目录 + 同 browser args），所以增量内存远小于再起一个进程组；换来的是一次
/// 「按下就到」的首次体验 —— 否则第一次截图要等 1–2 秒，用户会以为没反应。
pub fn prewarm(app: &AppHandle) {
    if app.get_webview_window(SHOT_LABEL).is_some() {
        return;
    }
    if let Some(w) = build_overlay(app) {
        // 预热时就把窗口铺到主屏大小。否则首次截图要做一次 800×600 → 1920×1080
        // 的调整，WebView2 会重排整页 —— 实测这一下能吃掉一两秒，比别人慢一个量级。
        // 这里只定尺寸、不动位置，窗口还隐藏着，代价几乎为零。
        if let Ok(Some(mon)) = app.primary_monitor() {
            let s = mon.size();
            let _ = w.set_size(tauri::PhysicalSize::new(s.width, s.height));
            crate::log_print!("[shot] 覆盖层已预热（隐藏待命，{}×{}）", s.width, s.height);
        } else {
            crate::log_print!("[shot] 覆盖层已预热（隐藏待命）");
        }
    }
}

/// 启动后的延迟预热：**只在内置截图模式下做**。
///
/// 默认走系统截图时再常驻一个隐藏 WebView 是白花钱 —— 那套实现根本用不到覆盖层。
/// 延迟 3 秒是为了避开开机那一段资源高峰，不跟主窗口抢。
pub fn prewarm_delayed(app: AppHandle) {
    std::thread::spawn(move || {
        // 1.5 秒而不是 3 秒：日志显示「刚打开就按快捷键」时覆盖层常常还没建好，
        // 那一次要现建窗口（页面加载 + IPC 建立），慢到 1.3–2 秒 —— 用户的鼠标
        // 早就在拖了，第一下自然落空。1.5 秒仍避开了主窗口首帧那一波。
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if crate::settings::get().screenshot_mode != "builtin" {
            return;
        }
        let a = app.clone();
        let _ = app.run_on_main_thread(move || prewarm(&a));
    });
}

/// 立刻预热（用户在设置里切到内置截图时用）。
///
/// 为什么不能延后到「第一次按快捷键」：那正是要消除的那 1–2 秒等待。
/// 窗口必须在主线程创建，而命令跑在异步运行时上，所以这里转一次主线程。
pub fn prewarm_now(app: &AppHandle) {
    let a = app.clone();
    let _ = app.run_on_main_thread(move || prewarm(&a));
}

/// 让**客户区**严格等于屏幕矩形（而不是让外框等于屏幕）。
///
/// 为什么不能只 `set_size(w,h)`：那设的是外框，而无边框窗口在 Windows 上仍带一圈
/// 不可见的 resize 边框 —— 实测给 1920×1080 的外框，客户区只有 1904×1041。
/// 后果不是「图小一点」而是**整体偏移**：图片左上角落在屏幕 (8,31)，屏幕最左/最上
/// 那一窄条露出的是**还在动的真实桌面**，截图看着就是歪的。
///
/// 也不用 `set_fullscreen`：实测它同样不会让客户区铺满（视口仍是 1904×1041）。
/// 所以这里直接量：读外框与客户区的差，反向补偿尺寸；再读两者坐标差，反向补偿位置。
/// 迭代几轮是因为窗口尺寸改变后系统可能重新算边框。
///
/// 此刻窗口还是隐藏的（`prepare_overlay` 在 `reveal` 之前跑），所以多次重设不会闪。
fn fit_client_to_screen(win: &tauri::WebviewWindow, left: i32, top: i32, w: i32, h: i32) {
    for _ in 0..3 {
        let Ok(inner) = win.inner_size() else { return };
        let dw = w - inner.width as i32;
        let dh = h - inner.height as i32;
        if dw == 0 && dh == 0 {
            break;
        }
        let Ok(outer) = win.outer_size() else { return };
        let _ = win.set_size(PhysicalSize::new(
            (outer.width as i32 + dw).max(1) as u32,
            (outer.height as i32 + dh).max(1) as u32,
        ));
    }
    // 客户区左上角相对窗口左上角的偏移：把它反向加到目标位置上，
    // 客户区才会正好压在 (left, top) 上
    if let (Ok(op), Ok(ip)) = (win.outer_position(), win.inner_position()) {
        let (bx, by) = (ip.x - op.x, ip.y - op.y);
        let _ = win.set_position(PhysicalPosition::new(left - bx, top - by));
        if bx != 0 || by != 0 {
            crate::log_print!("[shot] 窗口边框补偿：位移 {bx},{by}");
        }
    } else {
        let _ = win.set_position(PhysicalPosition::new(left, top));
    }
}

/// 关掉这个窗口的显示/隐藏过渡动画。
///
/// Windows（尤其 Win11）给普通窗口的 show/hide 加了淡入淡出，全屏覆盖层上表现为
/// 「按快捷键闪一下」「点完成/取消闪一下」。微信截图没有这个问题，因为它用的是
/// layered window 直接贴图；我们用普通窗口 + WebView，只能把这个动画关掉。
///
/// 失败的代价为零（最坏就是保持默认动画），所以不检查返回值、也不记日志。
fn disable_window_transitions(win: &tauri::WebviewWindow) {
    const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
    let Ok(h) = win.hwnd() else { return };
    let on: u32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            h.0 as isize,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &on as *const u32 as *const std::ffi::c_void,
            4,
        );
    }
}

/// 真正把覆盖层亮出来并抢焦点。由前端在截图解码完成后调用（见 `shot_show`）。
///
/// 「按了快捷键，要再点一下鼠标才能框选」的两种可能：窗口出得慢（用户等不及先点了），
/// 或者窗口出来了但不是前台（第一次点击被系统拿去激活）。日志把两件事都记下来，
/// 下次再遇到直接看数据，不用猜。
fn reveal(app: &AppHandle) {
    let Some(win) = app.get_webview_window(SHOT_LABEL) else {
        return;
    };
    // **只显示，不激活**。窗口带 `WS_EX_NOACTIVATE`（见 `never_focus`），所以：
    // - 前台仍是用户原来的窗口，截完图不用再把焦点还回去；
    // - 第一次点击不会被系统拿去激活，直接就是 `pointerdown`。
    // 这里若再调 `set_focus` / `SetForegroundWindow`，就会把 NOACTIVATE 的好处抵消掉
    // 一半（NOACTIVATE 窗口仍可被程序显式激活），等于走回老路。
    let _ = win.show();
    let elapsed = now_ms().saturating_sub(START_MS.load(Ordering::SeqCst));
    crate::log_print!("[shot] 从按下快捷键到覆盖层可见：{elapsed}ms（不激活窗口）");
}

/// 本次截图开始的时刻（Unix 毫秒）。跨线程只能用整数时间戳，
/// `Instant` 不是 `Sync` 的全局可共享类型。
static START_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 开关都带上当前会话序号：热键线程是异步处理这两条消息的，连续两次截图时
/// 旧会话排队的「卸」很可能排在新会话的「装」后面 —— 一带序号就能识别出来并忽略。
fn guard_on() {
    crate::hotkey::post_msg_w(WM_SHOT_GUARD_ON, SEQ.load(Ordering::SeqCst) as usize);
}

fn guard_off() {
    crate::hotkey::post_msg_w(WM_SHOT_GUARD_OFF, SEQ.load(Ordering::SeqCst) as usize);
}

/// 覆盖层窗口句柄。钩子要用它判断「覆盖层是否已经可见」。
static SHOT_HWND: AtomicIsize = AtomicIsize::new(0);
/// 最近一次左键按下：屏幕物理坐标 + Unix 毫秒。
///
/// **不看窗口是否可见** —— 用户按下快捷键后手已经在拖，那一下发生在覆盖层出现
/// 之前，正是「还得再点一次」的根源，所以必须记下来。
static LAST_DOWN: Mutex<Option<(i32, i32, u64)>> = Mutex::new(None);
/// 上一次转发移动事件的时间（16ms 节流用）
static LAST_MOVE_MS: AtomicU64 = AtomicU64::new(0);
/// 首次按下补偿的鼠标钩子句柄（与 `GUARD` 成对装卸）
static MOUSE_GUARD: AtomicIsize = AtomicIsize::new(0);

/// 装上 **Esc 兜底**钩子：截图期间无论前端是否正常，按 Esc 都能退出。
///
/// 为什么必须有这一条：取消逻辑本来完全在前端（`keydown` → `shot_cancel`）。
/// 于是「页面没加载成功」时，用户面对的是一个全屏、置顶、点了没反应的窗口 ——
/// 这不是假设，是实测踩到的事故。逃生通道不能依赖那条最可能出问题的链路。
///
/// 与 `screenclip` 的鼠标钩子同源（都装在热键线程的消息泵上），但判据不同：
/// 那个拦截鼠标右键、只在系统截图覆盖层出现时生效；这个只认 Esc、只在本次截图期间存在。
pub fn guard_install(seq: u32) {
    // 旧会话排队的「装」不能作用到新会话上（序号一定落后），直接丢弃
    if seq != SEQ.load(Ordering::SeqCst) {
        crate::log_print!("[shot] 忽略旧会话 #{seq} 的钩子安装请求");
        return;
    }
    // **两个钩子各自判断、各装各的**。早先这里是「键盘钩子已装就直接 return」，
    // 于是「上一轮还在卸载、这一轮已经开始」的竞态下，鼠标钩子被整段跳过 ——
    // 那一轮里第一次拖动就完全没反应，用户只能再点一次。
    if GUARD.load(Ordering::SeqCst) == 0 {
        unsafe {
            let h =
                SetWindowsHookExW(WH_KEYBOARD_LL, key_proc, GetModuleHandleW(std::ptr::null()), 0);
            if h == 0 {
                crate::log_warn!("[shot] Esc 兜底钩子安装失败（错误码 {}）", GetLastError());
            } else {
                GUARD.store(h, Ordering::SeqCst);
                crate::log_print!("[shot] Esc / Enter 兜底已启用");
            }
        }
    }
    click_guard_install();
}

/// 装上**首次按下补偿**。
///
/// 覆盖层已经是 `WS_EX_NOACTIVATE`（点击不激活），但实测仍偶发「第一下拖不动」——
/// 说明 WebView2 自己在那儿挡了一下：宿主窗口不激活了，渲染进程仍会把第一次按下
/// 当成「先聚焦页面」。与其继续猜它内部怎么判，不如在这里补一手：
///
/// 钩子把这次按下**原样告诉前端**（坐标 + 时间戳），前端就地开始框选。
/// **不吞事件**：WebView 若也能收到，它的 `pointerdown` 会先一步把 `drag` 建好，
/// 前端靠 `drag` 已存在 + 时间戳去重，不会重复开始。两条路谁先到都行。
pub fn click_guard_install() {
    if MOUSE_GUARD.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        let h = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, GetModuleHandleW(std::ptr::null()), 0);
        if h == 0 {
            crate::log_warn!("[shot] 首次按下补偿钩子安装失败（错误码 {}）", GetLastError());
            return;
        }
        MOUSE_GUARD.store(h, Ordering::SeqCst);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, w: usize, l: isize) -> isize {
    // 右键也转给前端：「取消」要是点按钮/按 Esc 之外再多一条稳的路，
    // 而 `contextmenu` 在 NOACTIVATE 窗口里是不合成的，只能靠这里。
    if code == HC_ACTION && w == WM_RBUTTONDOWN {
        let hwnd = SHOT_HWND.load(Ordering::SeqCst);
        if hwnd != 0 && IsWindowVisible(hwnd) != 0 {
            let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                let _ = app.emit_to(SHOT_LABEL, "shot-right", ());
            }
        }
    }
    // 移动也转发（16ms 节流 ≈ 60Hz）：NOACTIVATE 窗口里 DOM 的 mousemove 同样
    // 不保证每次都到，而拖动时漏几次，用户就觉得「按下去没反应」。
    if code == HC_ACTION && w == WM_MOUSEMOVE {
        let hwnd = SHOT_HWND.load(Ordering::SeqCst);
        if hwnd != 0 && IsWindowVisible(hwnd) != 0 {
            let now = now_ms();
            let prev = LAST_MOVE_MS.load(Ordering::SeqCst);
            if now.saturating_sub(prev) >= 16 {
                LAST_MOVE_MS.store(now, Ordering::SeqCst);
                let ms = &*(l as *const MsllHookStruct);
                let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
                if let Some(app) = app {
                    let _ = app.emit_to(SHOT_LABEL, "shot-move", (ms.pt.x, ms.pt.y));
                }
            }
        }
    }
    if code == HC_ACTION && w == WM_LBUTTONUP {
        let hwnd = SHOT_HWND.load(Ordering::SeqCst);
        if hwnd != 0 && IsWindowVisible(hwnd) != 0 {
            let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                // 带 Unix 毫秒：前端要拿它和本次按下的时间比，判断这条是不是迟到的
                let _ = app.emit_to(SHOT_LABEL, "shot-up", now_ms());
            }
        }
    }
    if code == HC_ACTION && w == WM_LBUTTONDOWN {
        let hwnd = SHOT_HWND.load(Ordering::SeqCst);
        // **不论窗口是否可见都记一笔**：窗口出现前的那次按下正是「要再点一次」的
        // 根源，它必须能被 `shot_pointer` 取到（前端据此把起点补上）。
        // 钩子只在截图期间装着，所以这里记下的一定属于本轮。
        let ms = &*(l as *const MsllHookStruct);
        if let Ok(mut g) = LAST_DOWN.lock() {
            *g = Some((ms.pt.x, ms.pt.y, now_ms()));
        }
        // 但**转发给前端**仍然要求窗口可见：不可见时的点击属于下面的程序，
        // 不该被拿来开选区。
        if hwnd != 0 && IsWindowVisible(hwnd) != 0 {
            let ms = &*(l as *const MsllHookStruct);
            let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                // 时间戳用 Unix 毫秒而不是结构体里的 `time`：后者是开机以来的 tick 数，
                // 前端拿 `Date.now()` 减它只会得到一个天文数字，判不出「是否迟到」。
                let _ = app.emit_to(SHOT_LABEL, "shot-down", (ms.pt.x, ms.pt.y, now_ms()));
            }
        }
    }
    CallNextHookEx(MOUSE_GUARD.load(Ordering::SeqCst), code, w, l)
}

pub fn guard_uninstall(seq: u32) {
    // 旧会话的「卸」如果排到了新会话的「装」之后，会把新会话正在用的钩子拆掉 ——
    // 那一轮就会出现「拖不动、按钮点不动、Esc 也没反应」。序号不匹配就不动。
    if seq != SEQ.load(Ordering::SeqCst) {
        crate::log_print!("[shot] 忽略旧会话 #{seq} 的钩子卸载请求");
        return;
    }
    let h = GUARD.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            UnhookWindowsHookEx(h);
        }
        crate::log_print!("[shot] Esc / Enter 兜底已卸载");
    }
    // 鼠标钩子也必须在这里卸。漏掉它的后果是：截图结束之后，用户**每一次左键点击**
    // 都会被当成「本次截图的第一下」转发给前端，前端据此又建出一个选区 ——
    // 表现就是「点完完成/取消，却又回到截图状态」。
    let m = MOUSE_GUARD.swap(0, Ordering::SeqCst);
    if m != 0 {
        unsafe {
            UnhookWindowsHookEx(m);
        }
        crate::log_print!("[shot] 首次按下补偿已卸载");
    }
}

unsafe extern "system" fn key_proc(code: i32, w: usize, l: isize) -> isize {
    if code == HC_ACTION && (w == WM_KEYDOWN || w == WM_SYSKEYDOWN) {
        let kb = &*(l as *const KbdLlHookStruct);
        if kb.vk_code == VK_ESCAPE {
            let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                // 钩子回调必须尽快返回，收窗口的活交给独立线程
                std::thread::spawn(move || {
                    crate::log_print!("[shot] Esc（兜底）取消截图");
                    close(&app, END_CANCEL);
                });
            }
            // 吞掉这次 Esc：别让它同时落到下层窗口（比如编辑器里退出全屏）
            return 1;
        }
        // Enter 也得走这里：覆盖层是 `WS_EX_NOACTIVATE`，永远拿不到焦点，前端收不到
        // keydown。**只转发不裁决** —— 有没有选区、选区多小，都是前端更清楚，让它
        // 决定提交还是忽略。
        if kb.vk_code == VK_RETURN {
            let app = SHOT_APP.lock().ok().and_then(|g| g.clone());
            if let Some(app) = app {
                let _ = app.emit_to(SHOT_LABEL, "shot-enter", ());
            }
            return 1;
        }
    }
    CallNextHookEx(GUARD.load(Ordering::SeqCst), code, w, l)
}

/// 抓一块屏幕区域，返回自上而下的 BGRA。
unsafe fn grab(left: i32, top: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    let name = display_name();
    let screen = CreateDCW(name.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null());
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
                // 负数 = 自上而下。取正向的话缓冲区是倒的，裁剪代码得反着算行号，
                // 而 CF_DIB 那边又要求倒序，两处翻转极易写错。
                height: -h,
                planes: 1,
                bit_count: 32,
                compression: 0, // BI_RGB
                size_image: buf.len() as u32,
                ..Default::default()
            },
            colors: [0; 3],
        };
        let lines = GetDIBits(mem, bmp, 0, h as u32, buf.as_mut_ptr(), &mut bi, DIB_RGB_COLORS);
        ok = lines == h;
    }

    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    DeleteDC(screen);

    if !ok {
        return None;
    }
    // GDI 不维护 alpha，取到的第 4 字节是未初始化值；统一补 255，
    // 否则粘到接受 alpha 的程序里会出现整张图半透明或黑底
    for px in buf.chunks_exact_mut(4) {
        px[3] = 255;
    }
    Some(buf)
}

/// 把 `Shot` 编码成 PNG data URL 交给前端显示。
///
/// 走 data URL 而不是临时文件：省掉「写盘 → 权限 → 清理」三件事，
/// 也不会在用户磁盘上留一堆截图残片。代价是 IPC 会传一段 base64（1080p 约 1–2MB）。
/// 把一帧 BGRA 编码成 JPEG data URL（**纯 CPU 活，不碰任何锁**）。
///
/// 单独抽出来是为了能扔进 `spawn_blocking`：这一步 300ms 上下，绝不能占主线程。
fn encode_preview(pixels: &[u8], w: u32, h: u32) -> Option<String> {
    // 预览用 **JPEG** 而不是 PNG：1080p 的 PNG 编码实测 143–203ms，占了
    // 「按下快捷键 → 看到画面」的大头，而这段时间里用户的鼠标已经在按了 ——
    // 画面晚到一步，第一下就落在下层窗口上，表现为「还得再点一次」。
    //
    // 画质不受影响：**最终裁的是 `shot.pixels` 里的原始像素**，预览图只负责
    // 给用户看「我在哪、框哪块」，不参与出图。
    let count = w as usize * h as usize;
    let mut rgb = Vec::with_capacity(count * 3);
    for px in pixels.chunks_exact(4) {
        // BGRA → RGB：JPEG 没有 alpha 通道，少转一个也少占 1/4 内存
        rgb.push(px[2]);
        rgb.push(px[1]);
        rgb.push(px[0]);
    }
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
    enc.encode(&rgb, w, h, image::ExtendedColorType::Rgb8).ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        crate::clipboard::base64_encode(&out)
    ))
}

/// 把选区从冻结的位图里裁出来，拼成 CF_DIB（BITMAPINFOHEADER + 自下而上 BGRA）。
fn crop_to_dib(shot: &Shot, x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>, String> {
    // 钳到图内：前端理论上不会给出越界坐标，但这是「一次算错就写出垃圾像素」的地方，
    // 在这里兜一次比事后查崩溃便宜
    let x = x.clamp(0, shot.w as i32) as u32;
    let y = y.clamp(0, shot.h as i32) as u32;
    let w = (w.max(0) as u32).min(shot.w - x);
    let h = (h.max(0) as u32).min(shot.h - y);
    if w == 0 || h == 0 {
        return Err("选区为空".into());
    }

    let row_bytes = (w * 4) as usize;
    let size_image = (row_bytes * h as usize) as u32;
    let mut dib = Vec::with_capacity(40 + row_bytes * h as usize);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight：正值 = 自下而上
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
        let sy = y + row;
        let start = ((sy * shot.w + x) * 4) as usize;
        dib.extend_from_slice(&shot.pixels[start..start + row_bytes]);
    }
    Ok(dib)
}

/// 前端就绪后拉取本次截图的位图。
///
/// **`async` 不是为了并发，是为了别占主线程**：Tauri 的**同步**命令跑在主线程上，
/// 而这里要做 300ms 上下的 JPEG 编码 —— 期间事件循环被堵死，窗口的尺寸调整、
/// 显示、收起全部排队。日志里「按下快捷键 → 覆盖层可见 2473ms」的那两秒空白，
/// 就是这么来的：用户等不到画面，手先动了。
#[tauri::command]
pub async fn shot_image() -> Result<String, String> {
    // 取数据这一段很快（8MB 拷贝 1–2ms），留在原线程，避免把锁带进阻塞线程
    let (w, h, pixels) = {
        let mut g = LAST.lock().map_err(|_| "截图状态锁失效")?;
        let shot = g.as_mut().ok_or("没有待处理的截图")?;
        if let Some(p) = shot.preview.clone() {
            crate::log_print!("[shot] 前端取图（命中缓存，{} KB）", p.len() / 1024);
            return Ok(p);
        }
        (shot.w, shot.h, shot.pixels.clone())
    };
    let t0 = std::time::Instant::now();
    let encoded = tauri::async_runtime::spawn_blocking(move || encode_preview(&pixels, w, h))
        .await
        .map_err(|e| format!("预览编码线程失败：{e}"))?;
    let Some(encoded) = encoded else {
        crate::log_warn!("[shot] 预览编码失败（{w}×{h}）");
        return Err("截图编码失败".into());
    };
    crate::log_print!(
        "[shot] 前端取图（JPEG {} KB，耗时 {}ms）",
        encoded.len() / 1024,
        t0.elapsed().as_millis()
    );
    if let Ok(mut g) = LAST.lock() {
        if let Some(shot) = g.as_mut() {
            shot.preview = Some(encoded.clone());
        }
    }
    Ok(encoded)
}

/// 覆盖层刚显示时，左键是否**已经按着**？按着就返回当时的屏幕坐标。
///
/// 这是「20% 概率要点两次」的直接解法：抓屏 + 编码 + 解码要 150–300ms（dev 下更久），
/// 用户按下快捷键后手指已经在拖了，等他看到画面时那一下早就按下去了 —— 那一下落在
/// 下面的窗口上，等于白按。这里让前端在显示之后立刻问一句「是不是已经在按着」，
/// 是的话就把**当前光标位置**当成框选起点，用户的手感是「一出现就跟着我走」。
#[tauri::command]
/// 返回值：`(x, y, age_ms, still_down)`。
///
/// `still_down` 必须给前端：拿旧坐标建了拖拽、而手其实早就松开的话，前端会一直
/// 等一个永远不来的 `mouseup`，`drag` 卡死，后面每次点击都被挡住。
pub fn shot_pointer() -> Option<(i32, i32, u64, bool)> {
    // 1) **优先用钩子记下的那次按下**。这是关键：用户按下快捷键后手已经在拖，
    //    那一下发生在覆盖层出现之前（抓屏 + 编码 200–400ms），落在下面的窗口上。
    //    等他看到画面时早就松手了，`GetAsyncKeyState` 查不到 —— 于是只能再点一次。
    //    钩子在截图期间一直记着最后一次按下，这里把它取出来当框选起点。
    let held = unsafe { GetAsyncKeyState(VK_LBUTTON) & 0x8000u16 as i16 != 0 };
    let recorded = LAST_DOWN.lock().ok().and_then(|g| *g);
    if let Some((x, y, t)) = recorded {
        let age = now_ms().saturating_sub(t);
        // 3 秒内才算「这一下的」：更早的是上次操作留下的，不能拿来开框
        if age <= 3000 {
            return Some((x, y, age, held));
        }
    }
    unsafe {
        // 2) 没有记录但左键正按着 → 用当前光标位置
        //    高字节是「当前是否按下」，不能用 != 0 判断，得取最高位
        if !held {
            return None;
        }
        // 复用 `effects::Rect` 接 POINT：两者前两个字段都是 i32，系统只写前 8 字节
        let mut r = crate::effects::Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetCursorPos(&mut r) == 0 {
            return None;
        }
        Some((r.left, r.top, 0, true))
    }
}

/// 光标下的窗口候选列表（给前端做「悬停高亮、单击截整窗」）。
///
/// 返回虚拟屏**物理**坐标，按 z 序从高到低；前端用几何判断命中了哪个，
/// 而不是逐次问后端 —— 鼠标移动是每秒上百次的事件，一次 IPC 往返就够掉帧了。
#[tauri::command]
pub fn shot_windows() -> Result<(i32, i32, Vec<[i32; 4]>), String> {
    let g = LAST.lock().map_err(|_| "截图状态锁失效")?;
    let shot = g.as_ref().ok_or("没有待处理的截图")?;
    Ok((shot.left, shot.top, shot.windows.clone()))
}

// ── 窗口枚举 ────────────────────────────────────────────────────────────

const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW: isize = 0x0080;
const DWMWA_CLOAKED: u32 = 14;
const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;

/// 与 `singleton` 里那份声明保持**同一签名**：同一个符号用两种类型声明，
/// 链接时只会给一条 "redeclared with a different signature" 警告，容易淹没真问题。
type EnumProc = extern "system" fn(isize, isize) -> i32;

extern "system" {
    fn EnumWindows(proc: EnumProc, param: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn GetWindowTextLengthW(hwnd: isize) -> i32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut crate::effects::Rect) -> i32;
    fn DwmGetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

/// 枚举「用户看得见、点得到」的顶层窗口。
///
/// 过滤掉的东西，每一条都对应一种会让高亮框乱跳的情况：
/// - 不可见 / 最小化 → 矩形还在，但屏幕上看不到；
/// - `WS_EX_TOOLWINDOW` → 输入法候选框、悬浮工具条之类，不是用户想截的「窗口」；
/// - UWP 的 cloaked 窗口 → 系统保留的幽灵窗口，矩形能覆盖半个屏幕；
/// - 本进程自己的窗口 → 覆盖层自己和轻贴主窗口，选中它们毫无意义；
/// - 没有标题 → 大量隐藏的宿主窗口都是无标题的。
unsafe fn list_windows() -> Vec<[i32; 4]> {
    // 只跳过**覆盖层自己**：轻贴的主窗口、快捷面板都是正经窗口，用户完全可能想截它们
    // （「快速识别不到轻贴本身」就是早先按进程过滤带来的）。
    let mut ctx = EnumCtx {
        out: Vec::new(),
        skip: SHOT_HWND.load(Ordering::SeqCst),
    };
    EnumWindows(enum_proc, &mut ctx as *mut EnumCtx as isize);
    ctx.out
}

struct EnumCtx {
    out: Vec<[i32; 4]>,
    skip: isize,
}

extern "system" fn enum_proc(hwnd: isize, param: isize) -> i32 {
    unsafe {
        let ctx = &mut *(param as *mut EnumCtx);
        if hwnd == ctx.skip {
            return 1;
        }
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return 1;
        }
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        // 桌面和任务栏也是「有标题的可见顶层窗口」，但高亮它们没有意义 ——
        // 光标停在桌面空白处就会框住整个屏幕，点一下变成截全屏，和你想要的
        // 「点一下不截」正相反。
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
        // 优先用 DWM 的可见边框：`GetWindowRect` 会把 Win10+ 那圈不可见的
        // 缩放边框和阴影一起算进去，截出来四周多几条边。
        let mut r = crate::effects::Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut crate::effects::Rect as *mut std::ffi::c_void,
            16,
        ) == 0;
        if !ok && GetWindowRect(hwnd, &mut r) == 0 {
            return 1;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return 1;
        }
        ctx.out.push([r.left, r.top, r.right, r.bottom]);
        1 // 继续枚举
    }
}

/// 前端把截图**解码完成**后调用，这时才显示窗口。
///
/// 顺序反了会闪：先 show 再加载图，用户按下快捷键的第一眼是白底或黑底，
/// 第二眼才变成桌面 —— 看起来像卡了一下。
#[tauri::command]
pub fn shot_show(app: AppHandle) {
    // 同步命令本身就在主线程执行，直接调用即可。早先这里多转了一次
    // `run_on_main_thread`，等于把「显示」推迟到事件循环的下一拍 ——
    // 用户等画面的时候，这一拍也算在延迟里。
    reveal(&app);
}

/// 双击 / `Enter`：裁图 → 写剪贴板（会被采集线程记成一条图片）→ 收起覆盖层。
#[tauri::command]
pub async fn shot_commit(app: AppHandle, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    // 顺带把「抓的是哪块屏」记进日志：多屏下选区坐标看着正常但内容错位时，
    // 第一个要排除的就是「抓屏原点不对」
    // 裁剪要搬几 MB 像素，写剪贴板又要等别的程序响应 —— 两件都放后台线程。
    // 同步命令会把它俩压在主线程上，用户点完绿勾看到的是「窗口一动不动」，
    // 于是再点一次、再点一次。
    let framed = {
        let g = LAST.lock().map_err(|_| "截图状态锁失效")?;
        match g.as_ref() {
            Some(shot) => Some((crop_to_dib(shot, x, y, w, h)?, shot.left, shot.top)),
            None if LAST_END.load(Ordering::SeqCst) == END_COMMIT => None,
            None => {
                crate::log_warn!(
                    "[shot] 提交时没有待处理截图（最近一次结束原因 {}，最近会话 #{}）",
                    end_reason_name(LAST_END.load(Ordering::SeqCst)),
                    SEQ.load(Ordering::SeqCst)
                );
                return Err("没有待处理的截图".into());
            }
        }
    };
    // `None` = 上一次提交已经完成、这是**重复提交**。双击完成、连按 Enter、手快
    // 连点两下绿勾都会走到这里：图早写进剪贴板了，此时再报「复制失败」只会让用户
    // 以为没成功，于是换个位置再点一次。当成功处理。
    let Some((dib, left, top)) = framed else {
        crate::log_print!("[shot] 重复提交，已忽略（图已在剪贴板）");
        return Ok(());
    };
    tauri::async_runtime::spawn_blocking(move || crate::clipboard::write_shot_to_clipboard(&dib))
        .await
        .map_err(|e| format!("写剪贴板线程失败：{e}"))??;
    crate::log_print!("[shot] 已复制选区 {w}×{h}（屏幕原点 {left},{top}）");
    close(&app, END_COMMIT);
    Ok(())
}

/// 右键 / `Esc` / 点空白：直接收起，**不碰剪贴板**。
///
/// 不碰剪贴板是硬要求：用户可能正拿剪贴板里的东西在干别的，截个图再取消不该把它冲掉。
#[tauri::command]
pub fn shot_cancel(app: AppHandle) {
    crate::log_print!("[shot] 已取消（剪贴板未改动）");
    close(&app, END_CANCEL);
}

/// 最近一次截图是怎么结束的。用来区分「重复提交」与「压根没有截图」——
/// 两者在 `LAST` 里都是 `None`，但对用户的意义完全相反。
const END_NONE: u8 = 0;
const END_COMMIT: u8 = 1;
const END_CANCEL: u8 = 2;
const END_TIMEOUT: u8 = 3;

static LAST_END: AtomicU8 = AtomicU8::new(END_NONE);
/// 会话序号，只用于日志对齐（一次截图 = 一个序号）
static SEQ: AtomicU32 = AtomicU32::new(0);

fn end_reason_name(code: u8) -> &'static str {
    match code {
        END_COMMIT => "已提交",
        END_CANCEL => "已取消",
        END_TIMEOUT => "前端未就绪被兜底取消",
        _ => "无（还没开始或异常）",
    }
}

fn close(app: &AppHandle, reason: u8) {
    LAST_END.store(reason, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window(SHOT_LABEL) {
        // 收起失败必须记下来：窗口是全屏不透明且置顶的，一旦没藏住，
        // 用户看到的就是「截完了却还停在冻结画面」，和「退不出去」一模一样。
        if let Err(e) = w.hide() {
            crate::log_err!("screenshot", "收起覆盖层失败：{e}");
        }
    }
    guard_off();
    // 抓屏缓冲有几 MB（1080p 约 8MB），不用了立刻放掉
    if let Ok(mut g) = LAST.lock() {
        *g = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(w: u32, h: u32) -> Shot {
        Shot {
            w,
            h,
            left: 0,
            top: 0,
            pixels: vec![7u8; (w as usize) * (h as usize) * 4],
            preview: None,
            windows: Vec::new(),
        }
    }

    /// 裁出来的 DIB 必须与头部声明的 `biSizeImage` 严格一致。
    ///
    /// 实测踩过：落盘的 BMP 比头部少了 4 字节，缩略图解码直接 `failed to fill
    /// whole buffer`，用户在列表里看到的是灰色占位图标。尺寸差一个像素就整张图作废。
    #[test]
    fn crop_matches_declared_size() {
        let s = fake(1920, 1080);
        for (x, y, w, h) in [(0, 0, 685, 289), (300, 250, 280, 200), (0, 0, 1920, 1080)] {
            let dib = crop_to_dib(&s, x, y, w, h).expect("裁剪应成功");
            let declared = u32::from_le_bytes([dib[20], dib[21], dib[22], dib[23]]) as usize;
            assert_eq!(declared, w as usize * h as usize * 4, "biSizeImage 与像素数不符");
            assert_eq!(dib.len(), 40 + w as usize * h as usize * 4, "DIB 实际长度不符");
        }
    }
}
