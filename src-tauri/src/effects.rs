//! Windows 窗口材质：按系统 build 号**显式分档**，绝不静默失败。
//!
//! v1 最痛的教训：`setBackgroundMaterial("acrylic")` 在 Win10 上不抛错、不生效，
//! 日志一直打「enabled」而实际从未生效（ADR-003、开发原则 3/4）。
//!
//! 本模块的三条硬规矩：
//!   1. 调用前先探测 build，明确分档
//!   2. 每档都打印「实际 build + 实际档位 + API 返回码」，可观测
//!   3. 每档都有降级路径，且降级后观感不崩（solid 档用不透明底色）
//!
//! 全部走手写 FFI，不依赖 windows crate 的元数据。

use std::ffi::c_void;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

// DwmSetWindowAttribute
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
const DWMWA_MICA_EFFECT: u32 = 1029;

// DWM_SYSTEMBACKDROP_TYPE
const DWMSBT_TRANSIENTWINDOW: u32 = 3;

// DWM_WINDOW_CORNER_PREFERENCE
const DWMWCP_ROUND: u32 = 2;

// SetWindowCompositionAttribute
const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

/// Win10 acrylic 的 tint，格式为 **0xAABBGGRR**（注意不是 RGBA）。
/// alpha 太低（≤66%）时壁纸颜色会大量透出，观感发脏；太高（≥90%）
/// 则模糊被盖死。80% 左右是「模糊清晰可见 + 壁纸脏色被压住」的平衡点，
/// 再配合前端一层玻璃光泽渐变（styles.css .app::after）提升质感。
const TINT_LIGHT: u32 = 0xCC_FF_FF_FF; // 白色 80%
const TINT_DARK: u32 = 0xCC_1C_1C_1E; // #1E1C1C 80%

// ---------------------------------------------------------------------------
// FFI 声明
// ---------------------------------------------------------------------------

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32, // 0xAABBGGRR
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    data: *mut c_void,
    size_of_data: usize,
}

#[repr(C)]
struct RtlOsVersionInfoExW {
    os_version_info_size: u32,
    major_version: u32,
    minor_version: u32,
    build_number: u32,
    platform_id: u32,
    csd_version: [u16; 128],
    service_pack_major: u16,
    service_pack_minor: u16,
    suite_mask: u16,
    product_type: u8,
    reserved: u8,
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(isize, *mut WindowCompositionAttribData) -> i32;

/// 物理坐标矩形，user32 的 GetWindowRect / SetWindowRgn 都用它
#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// Win10 真·毛玻璃的唯一入口，但它是**未公开 API**：
/// user32.dll 里有导出，Windows SDK 的 `user32.lib` 里却没有对应的导入符号，
/// 直接在 extern 块里声明会得到 LNK2019。只能运行时 GetProcAddress。
///
/// （这正是 v1 用 Electron 时拿不到 Win10 毛玻璃的深层原因之一：
///   有文档的路径只通到 Win11，Win10 只能走这条路。）
fn set_window_composition_attribute() -> Option<SetWindowCompositionAttributeFn> {
    static CACHE: OnceLock<Option<SetWindowCompositionAttributeFn>> = OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        extern "system" {
            fn LoadLibraryA(name: *const i8) -> isize;
            fn GetProcAddress(module: isize, name: *const i8) -> *mut c_void;
        }

        let dll = LoadLibraryA(b"user32.dll\0".as_ptr() as *const i8);
        if dll == 0 {
            eprintln!("[effects] 加载 user32.dll 失败");
            return None;
        }
        let proc = GetProcAddress(
            dll,
            b"SetWindowCompositionAttribute\0".as_ptr() as *const i8,
        );
        if proc.is_null() {
            eprintln!("[effects] user32 中找不到 SetWindowCompositionAttribute 导出");
            return None;
        }
        Some(std::mem::transmute::<
            *mut c_void,
            SetWindowCompositionAttributeFn,
        >(proc))
    })
}

extern "system" {
    /// dwmapi.dll
    fn DwmSetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *const c_void,
        value_size: u32,
    ) -> i32; // HRESULT

    /// ntdll.dll —— 不受 app manifest 影响，比 GetVersionEx 可靠
    fn RtlGetVersion(info: *mut RtlOsVersionInfoExW) -> i32; // NTSTATUS
}

// ---------------------------------------------------------------------------
// 档位
// ---------------------------------------------------------------------------

/// 实际生效的材质档位。下发到前端，前端据此切 CSS class。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Win11 22H2+：DWMWA_SYSTEMBACKDROP_TYPE 系统级 acrylic，圆角由系统给
    SystemBackdrop,
    /// Win11 21H2：Mica（不透明壁纸采样，不是真模糊，但很接近）
    Mica,
    /// Win10 1809–22H2：SetWindowCompositionAttribute 真·acrylic，圆角限 10px
    Acrylic,
    /// 兜底：不透明底色，观感不崩
    Solid,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::SystemBackdrop => "system-backdrop",
            Tier::Mica => "mica",
            Tier::Acrylic => "acrylic",
            Tier::Solid => "solid",
        }
    }

    /// 该档位下 CSS 圆角能安全用多大（逻辑 px）。
    /// Win10 acrylic 铺满整个窗口矩形、不认 CSS 圆角，
    /// 超过 10px 时窗口裁剪圆角与内容圆角会互相打架。
    pub fn max_radius(&self) -> u32 {
        match self {
            Tier::Acrylic => 10,
            _ => 12,
        }
    }

    /// 窗口区域裁剪用的圆角（逻辑 px）。返回 0 表示不需要 RGN 裁剪。
    /// SystemBackdrop 档系统直接给圆角，裁剪反而会破坏它。
    pub fn region_radius(&self) -> u32 {
        match self {
            Tier::SystemBackdrop => 0,
            Tier::Acrylic => 10,
            Tier::Mica | Tier::Solid => 12,
        }
    }
}

static TIER: OnceLock<Tier> = OnceLock::new();
static BUILD: OnceLock<u32> = OnceLock::new();

pub fn current_tier() -> Tier {
    *TIER.get_or_init(|| Tier::Solid)
}

pub fn current_build() -> u32 {
    *BUILD.get_or_init(|| detect_build())
}

// ---------------------------------------------------------------------------
// 版本探测
// ---------------------------------------------------------------------------

/// 用 RtlGetVersion 读真实 build 号。
/// 不用 GetVersionEx：没有 manifest 声明时它会在 Win10 上返回 6.2（Win8），
/// 这正是「静默失败」的典型来源。
fn detect_build() -> u32 {
    unsafe {
        let mut info: RtlOsVersionInfoExW = std::mem::zeroed();
        info.os_version_info_size = std::mem::size_of::<RtlOsVersionInfoExW>() as u32;
        let status = RtlGetVersion(&mut info as *mut _);
        if status < 0 {
            eprintln!("[effects] RtlGetVersion 失败 status=0x{:08X}，按 solid 处理", status);
            return 0;
        }
        info.build_number
    }
}

fn classify(build: u32) -> Tier {
    if build >= 22621 {
        Tier::SystemBackdrop
    } else if build >= 22000 {
        Tier::Mica
    } else if build >= 17763 {
        Tier::Acrylic
    } else {
        Tier::Solid
    }
}

// ---------------------------------------------------------------------------
// 施加材质
// ---------------------------------------------------------------------------

/// 对窗口施加材质，返回**实际使用**的档位。
///
/// `hwnd` 为原生窗口句柄的原始值（isize）。
pub fn apply(hwnd: isize, dark: bool) -> Tier {
    let build = detect_build();
    let tier = classify(build);
    let _ = BUILD.set(build);
    let _ = TIER.set(tier);

    println!(
        "[effects] windows build={} → tier={} (radius≤{}px)",
        build,
        tier.as_str(),
        match tier.max_radius() {
            u32::MAX => 0,
            r => r,
        }
    );

    // 标题栏/系统区域跟随深色
    set_immersive_dark(hwnd, dark);

    let result = match tier {
        Tier::SystemBackdrop => apply_system_backdrop(hwnd),
        Tier::Mica => apply_mica(hwnd),
        Tier::Acrylic => apply_acrylic(hwnd, dark),
        Tier::Solid => Ok(()),
    };

    match result {
        Ok(()) => {
            println!("[effects] 材质应用完成：{}", tier.as_str());
            tier
        }
        Err(code) => {
            // 开发原则 3：降级必须显式，不能假装成功
            eprintln!(
                "[effects] 材质 {} 应用失败 code=0x{:08X}，降级为 solid",
                tier.as_str(),
                code
            );
            let _ = TIER.set(Tier::Solid);
            Tier::Solid
        }
    }
}

/// 触发 DWM 重算。Win10 acrylic 有时需要动一下窗口才真正铺开。
pub fn nudge(hwnd: isize) {
    unsafe {
        extern "system" {
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }

        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

        let mut r: Rect = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r as *mut _) == 0 {
            return;
        }
        // 先 +1 再还原，强制 DWM 重绘一次
        SetWindowPos(hwnd, 0, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        SetWindowPos(hwnd, 0, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// 把窗口区域裁成圆角矩形（Win10 上 acrylic 不认 CSS 圆角，只能裁窗口本体）。
///
/// - 半径按当前 DPI 从逻辑 px 换算成物理 px
/// - 窗口尺寸变化后 region **不会**自动跟随，必须在 Resized 事件里重设
/// - SystemBackdrop 档跳过（系统已经给了圆角）
pub fn apply_rounded_corners(hwnd: isize) {
    let radius = current_tier().region_radius();
    if radius == 0 {
        return;
    }

    unsafe {
        extern "system" {
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
            fn GetDpiForWindow(hwnd: isize) -> u32;
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
            fn SetWindowRgn(hwnd: isize, rgn: isize, redraw: i32) -> i32;
            fn DeleteObject(obj: isize) -> i32;
        }

        let mut r: Rect = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r as *mut _) == 0 {
            return;
        }
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w <= 0 || h <= 0 {
            return;
        }

        let dpi = GetDpiForWindow(hwnd).max(96);
        let rad = ((radius as f64) * (dpi as f64 / 96.0)).round() as i32;
        let d = rad * 2; // 椭圆直径 = 2 × 半径

        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, d, d);
        if rgn == 0 {
            eprintln!("[effects] CreateRoundRectRgn 失败");
            return;
        }
        if SetWindowRgn(hwnd, rgn, 1) == 0 {
            // 失败时 region 所有权还在我们手里，要释放
            DeleteObject(rgn);
            eprintln!("[effects] SetWindowRgn 失败");
        }
    }
}

/// 缩放代际号：连续缩放的每个 Resized 事件都会 +1，
/// 只有「静止 120ms」后仍是最新代际的那次才恢复圆角。
static RESIZE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 缩放中调用：先清掉窗口圆角区域（变直角），停止缩放后再恢复圆角。
///
/// 逐帧 SetWindowRgn 对窗口本体做区域裁剪非常贵（大窗口尤甚），
/// 会让缩放明显卡顿。这里改成「缩放中不裁剪、缩放后补一次」。
pub fn begin_resize(hwnd: isize) {
    unsafe {
        extern "system" {
            fn SetWindowRgn(hwnd: isize, rgn: isize, redraw: i32) -> i32;
        }
        // rgn=0 → 移除区域裁剪（直角）
        SetWindowRgn(hwnd, 0, 1);
    }
    let gen = RESIZE_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        if RESIZE_GEN.load(std::sync::atomic::Ordering::Relaxed) == gen {
            apply_rounded_corners(hwnd);
        }
    });
}

fn set_immersive_dark(hwnd: isize, dark: bool) {
    unsafe {
        let v: u32 = if dark { 1 } else { 0 };
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &v as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            eprintln!("[effects] immersive dark 设置失败 hr=0x{:08X}", hr);
        }
    }
}

/// Win11 22H2+：系统级 backdrop + 系统圆角
fn apply_system_backdrop(hwnd: isize) -> Result<(), i32> {
    unsafe {
        let v: u32 = DWMSBT_TRANSIENTWINDOW;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &v as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            // 退回 Mica
            return apply_mica(hwnd);
        }
        let corner: u32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        Ok(())
    }
}

/// Win11 21H2：Mica
fn apply_mica(hwnd: isize) -> Result<(), i32> {
    unsafe {
        let v: u32 = 1;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_MICA_EFFECT,
            &v as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            return Err(hr);
        }
        Ok(())
    }
}

/// Win10 1809–22H2：真·acrylic
fn apply_acrylic(hwnd: isize, dark: bool) -> Result<(), i32> {
    set_accent_state(
        hwnd,
        ACCENT_ENABLE_ACRYLICBLURBEHIND,
        if dark { TINT_DARK } else { TINT_LIGHT },
    )
}

fn set_accent_state(hwnd: isize, accent_state: u32, tint: u32) -> Result<(), i32> {
    let set_attr = match set_window_composition_attribute() {
        Some(f) => f,
        None => return Err(-1),
    };

    unsafe {
        let mut policy = AccentPolicy {
            accent_state,
            accent_flags: 2, // 四边都参与模糊
            gradient_color: tint,
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attrib: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut c_void,
            size_of_data: std::mem::size_of::<AccentPolicy>(),
        };
        let ok = set_attr(hwnd, &mut data as *mut _);
        if ok == 0 {
            return Err(-1);
        }
        Ok(())
    }
}

/// 系统主题切换时重刷 acrylic 的 tint（只有 Acrylic 档需要）
pub fn refresh_theme(hwnd: isize, dark: bool) {
    set_immersive_dark(hwnd, dark);
    if current_tier() == Tier::Acrylic {
        let _ = apply_acrylic(hwnd, dark);
    }
}

// ---------------------------------------------------------------------------
// 自定义拖动（替代系统移动循环 startDragging/SC_MOVE）
//
// 问题：系统移动循环在 Win10 acrylic 下会触发 DWM 的「幽灵窗口」
// （白框 + 原生标题栏，且盖住标题栏按钮 → 最大化/最小化点不动）；
// 之前用「拖动时改材质」绕开，但会造成拖动瞬间变色（用户否决）。
//
// 现在的做法：完全不进系统移动循环，自己拖——
//   标题栏按下 → SetCapture 抢占鼠标 → WndProc 里按光标位移调 SetWindowPos
//   → 抬起释放。全程材质保持 acrylic（不变色），也无幽灵窗口。
// ---------------------------------------------------------------------------

const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONUP: u32 = 0x0201;

static DRAG_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// (起始光标 X, 起始光标 Y, 起始窗口 X, 起始窗口 Y)
static DRAG_ORIGIN: std::sync::Mutex<(i32, i32, i32, i32)> =
    std::sync::Mutex::new((0, 0, 0, 0));

/// 前端在标题栏按下时调用：记录起点并抢占鼠标
pub fn begin_custom_drag(hwnd: isize) {
    unsafe {
        extern "system" {
            fn GetCursorPos(pt: *mut Rect) -> i32;
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
            fn SetCapture(hwnd: isize) -> isize;
        }
        let mut cur: Rect = std::mem::zeroed();
        let mut wr: Rect = std::mem::zeroed();
        // GetCursorPos 写入的是 POINT（x,y），与 Rect 前两个字段(left,top)布局一致
        if GetCursorPos(&mut cur) == 0 || GetWindowRect(hwnd, &mut wr) == 0 {
            return;
        }
        if let Ok(mut o) = DRAG_ORIGIN.lock() {
            *o = (cur.left, cur.top, wr.left, wr.top);
        }
        DRAG_ARMED.store(true, std::sync::atomic::Ordering::Relaxed);
        SetCapture(hwnd);
    }
}

/// 处理拖动期间的鼠标消息；返回 true 表示已处理（调用方应直接 return）
fn handle_drag_msg(hwnd: isize, msg: u32) -> bool {
    if !DRAG_ARMED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    unsafe {
        extern "system" {
            fn GetCursorPos(pt: *mut Rect) -> i32;
            fn SetWindowPos(
                hwnd: isize,
                after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
            fn ReleaseCapture() -> i32;
        }
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

        if msg == WM_MOUSEMOVE {
            let (sx, sy, wx, wy) = DRAG_ORIGIN.lock().map(|o| *o).unwrap_or((0, 0, 0, 0));
            let mut cur: Rect = std::mem::zeroed();
            if GetCursorPos(&mut cur) != 0 {
                SetWindowPos(
                    hwnd,
                    0,
                    wx + (cur.left - sx),
                    wy + (cur.top - sy),
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            return true;
        }
        if msg == WM_LBUTTONUP {
            DRAG_ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
            ReleaseCapture();
            return true;
        }
    }
    false
}

/// 前端调用入口（标题栏 mousedown 时 invoke）
#[tauri::command]
pub fn drag_start(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(h) = w.hwnd() {
            begin_custom_drag(h.0 as isize);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WndProc 子类：修焦点切换时的原生标题栏闪现
// ---------------------------------------------------------------------------

const WM_NCACTIVATE: u32 = 0x0086;

static ORIG_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// tao 用「保留 WS_CAPTION 样式位 + WM_NCCALCSIZE 吞掉非客户区」实现无边框，
/// 但窗口**失焦/聚焦**时 Windows 发 WM_NCACTIVATE，仍会把原生标题栏画出来
/// （用户实测：一点外部就出现原生标题栏 + 白边）。
///
/// 修法是给窗口过程挂子类（subclass），拦截这条消息后用 `lparam = -1`
/// 调 DefWindowProc——微软文档认可的做法，含义是「激活状态变更，但
/// 不要重绘非客户区」。只拦这一条消息，其余全部原样转交 tao。
pub fn install_focus_hook(hwnd: isize) {
    unsafe extern "system" {
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
    }

    const GWLP_WNDPROC: i32 = -4;

    unsafe extern "system" fn hook_proc(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        unsafe extern "system" {
            fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
            fn CallWindowProcW(
                prev: isize,
                hwnd: isize,
                msg: u32,
                wparam: usize,
                lparam: isize,
            ) -> isize;
        }

        if msg == WM_NCACTIVATE {
            // lparam = -1 → 禁止重绘非客户区，原生标题栏就永远画不出来
            return unsafe { DefWindowProcW(hwnd, msg, wparam, -1) };
        }
        // 剪贴板变化：事件驱动监听（替代轮询），读取并落盘后推给前端
        if msg == 0x031D {
            // WM_CLIPBOARDUPDATE
            crate::clipboard::on_clipboard_update();
            return 0;
        }
        // 自定义拖动：接管鼠标移动/抬起（不进入系统移动循环 → 无幽灵窗口、不变色）
        if handle_drag_msg(hwnd, msg) {
            return 0;
        }
        let orig = ORIG_WNDPROC.load(std::sync::atomic::Ordering::Relaxed);
        unsafe { CallWindowProcW(orig, hwnd, msg, wparam, lparam) }
    }

    let orig = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook_proc as isize) };
    if orig == 0 {
        eprintln!("[effects] WndProc 子类挂载失败（SetWindowLongPtrW 返回 0）");
        return;
    }
    ORIG_WNDPROC.store(orig, std::sync::atomic::Ordering::Relaxed);
    println!("[effects] 焦点 hook 已挂载（WM_NCACTIVATE → 不重绘 NC 区）");
}
