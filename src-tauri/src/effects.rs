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
const DWMSBT_NONE: u32 = 1;
const DWMSBT_TRANSIENTWINDOW: u32 = 3;

// DWM_WINDOW_CORNER_PREFERENCE
const DWMWCP_ROUND: u32 = 2;

// SetWindowCompositionAttribute
const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
/// accent_state = 0：彻底关闭系统 blur。窗口变成纯透明的 layered 窗口，
/// 四角是真透明，不再有 acrylic 铺满矩形留下的残留。见 `round_mode`。
const ACCENT_DISABLED: u32 = 0;

/// Win10 acrylic 的 tint，格式为 **0xAABBGGRR**（注意不是 RGBA）。
///
/// Fluent 规范：窗口级亚克力的 tint 透明度约 **70%–85%** 最自然——
/// 太低会大量透出壁纸脏色、观感发脏；太高会把模糊盖死、发闷发平。
/// 这里不写死，改由「材质浓度」配置（40–90%）动态计算，便于用户按壁纸微调。
const TINT_RGB_LIGHT: u32 = 0x00_FF_FF_FF; // 白
const TINT_RGB_DARK: u32 = 0x00_1C_1C_1E; // #1E1C1C

/// acrylic 的 tint alpha：**固定压到 8%**，不再由「材质浓度」控制。
///
/// 关键事实（社区实证，SO 63324480 等多处印证）：
/// **`SetWindowRgn` 对 SetWindowCompositionAttribute 的 acrylic 完全无效**——
/// acrylic 永远铺满整个窗口矩形，四角**切不掉**。tint alpha 越高，四角那圈
/// 「直角」越显眼（这正是用户反复反馈的直角来源）。
///
/// 所以策略改为：acrylic 只负责**模糊**（几乎不带色调），窗口主体的明暗与
/// 颜色交给 CSS 层（styles.css 的 --glass-alpha），这样四角的矩形残留就淡到
/// 看不出来了。副作用是多了一层「高亮度半透明」的观感，反而更接近 macOS。
const TINT_ALPHA: u32 = 0x14; // ≈8%

/// 计算 tint 值（格式 0xAABBGGRR）。alpha 固定为 TINT_ALPHA，参数保留仅为兼容调用方。
pub fn tint_for(dark: bool, _alpha_pct: u32) -> u32 {
    (TINT_ALPHA << 24) | if dark { TINT_RGB_DARK } else { TINT_RGB_LIGHT }
}

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
            crate::log_warn!("[effects] 加载 user32.dll 失败");
            return None;
        }
        let proc = GetProcAddress(
            dll,
            b"SetWindowCompositionAttribute\0".as_ptr() as *const i8,
        );
        if proc.is_null() {
            crate::log_warn!("[effects] user32 中找不到 SetWindowCompositionAttribute 导出");
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

    /// 前端 --radius 允许用多大（逻辑 px）。圆角现在纯由 CSS 绘制（带抗锯齿），
    /// 不再受窗口区域裁剪的限制，所以可以直接给到位。
    pub fn max_radius(&self) -> u32 {
        match self {
            Tier::Acrylic => 20,
            _ => 12,
        }
    }

    /// 窗口区域裁剪用的圆角（逻辑 px）。返回 0 表示不需要 RGN 裁剪。
    ///
    /// **现在全部返回 0——不再做任何区域裁剪。** 原因：
    ///   1. `SetWindowRgn` **管不住 acrylic**（实测 + 社区实证），acrylic 恒为
    ///      矩形，四角根本切不掉，所以它挡不住「直角」；
    ///   2. 1-bit 区域**没有抗锯齿**，圆角边缘是一排像素台阶
    ///      （用户反馈「圆角锯齿很重」的元凶）。
    /// 既然既挡不住 acrylic 又带来锯齿，就干脆不做裁剪：圆角完全交给 CSS 的
    /// border-radius（自带抗锯齿），四角残留的 acrylic 靠低 tint 压到看不见。
    pub fn region_radius(&self) -> u32 {
        0
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
pub fn detect_build() -> u32 {
    unsafe {
        let mut info: RtlOsVersionInfoExW = std::mem::zeroed();
        info.os_version_info_size = std::mem::size_of::<RtlOsVersionInfoExW>() as u32;
        let status = RtlGetVersion(&mut info as *mut _);
        if status < 0 {
            crate::log_warn!("[effects] RtlGetVersion 失败 status=0x{:08X}，按 solid 处理", status);
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

    crate::log_print!(
        "[effects] windows build={} → tier={} (radius≤{}px)",
        build,
        tier.as_str(),
        match tier.max_radius() {
            u32::MAX => 0,
            r => r,
        }
    );

    // 先清掉 WS_CAPTION | WS_THICKFRAME 等样式位——DWM 正是靠它们才
    // 沿窗口矩形画 1px 边框（SystemBackdrop 档需要 DWM 提供 backdrop 与圆角，不能动）
    if tier != Tier::SystemBackdrop {
        strip_window_frame(hwnd);
        // Tauri 的 4px 缩放边框子窗口会挡在 WebView 之上、抢走边缘鼠标事件，
        // 每次 apply 都确认一遍（窗口重建后子窗口句柄会变）
        disable_resize_border(hwnd);
    }

    // 标题栏/系统区域跟随深色
    set_immersive_dark(hwnd, dark);

    // 再关掉 DWM 的非客户区渲染（双保险；部分系统上该属性已不再支持）
    if tier != Tier::SystemBackdrop {
        disable_nc_rendering(hwnd);
    }

    let result = match tier {
        Tier::SystemBackdrop => apply_system_backdrop(hwnd),
        Tier::Mica => apply_mica(hwnd),
        Tier::Acrylic => apply_acrylic(hwnd, dark),
        Tier::Solid => Ok(()),
    };

    match result {
        Ok(()) => {
            crate::log_print!("[effects] 材质应用完成：{}", tier.as_str());
            tier
        }
        Err(code) => {
            // 开发原则 3：降级必须显式，不能假装成功
            crate::log_warn!(
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
///
/// 注意：这里刻意**不改窗口尺寸** —— 它会被 `apply`（圆角模式下走 `clear_accent`）
/// 和启动补涂流程反复调用，一旦真的改变尺寸，用户就会看到窗口抖动/闪烁。
/// 需要「逼 WebView2 重新布局」的场景请用 `relayout()`。
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
        // 原样重设一次位置：产生 WM_WINDOWPOSCHANGED，够 DWM 重算，且没有任何视觉变化
        SetWindowPos(hwnd, 0, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// 逼 WebView2 重新量一次尺寸（**会产生真实的 WM_SIZE**）。
///
/// 为什么需要它：隐藏到托盘走的是 `hide()`，窗口尺寸**没有变**，也就没有 WM_SIZE，
/// WebView2 的内容尺寸可能一直停在隐藏前的大小 —— 表现为「唤回来排版不对」。
/// （最小化不同：尺寸会归零再恢复，天然有一次 WM_SIZE，所以不会有这个问题。）
///
/// 因为要改尺寸，**必须在窗口不可见时调用**（隐藏期间或显示之前），否则会闪。
/// 最小化状态直接跳过：此时 `GetWindowRect` 返回的是任务栏上那个小图标矩形，
/// 拿它去改尺寸会破坏还原后的大小。
pub fn relayout(hwnd: isize) {
    unsafe {
        extern "system" {
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
            fn IsIconic(hwnd: isize) -> i32;
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

        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

        if IsIconic(hwnd) != 0 {
            return;
        }
        let mut r: Rect = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r as *mut _) == 0 {
            return;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return;
        }
        // 宽度 +1px 再还原 → 一次真实的 WM_SIZE
        SetWindowPos(hwnd, 0, r.left, r.top, w + 1, h, SWP_NOZORDER | SWP_NOACTIVATE);
        SetWindowPos(hwnd, 0, r.left, r.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
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
            fn IsIconic(hwnd: isize) -> i32;
            fn GetDpiForWindow(hwnd: isize) -> u32;
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
            fn SetWindowRgn(hwnd: isize, rgn: isize, redraw: i32) -> i32;
            fn DeleteObject(obj: isize) -> i32;
        }

        // 最小化时 GetWindowRect 返回的是**任务栏上那个小图标矩形**（约 160×28）。
        // 拿它去 SetWindowRgn，窗口就会被裁成一小块 —— 还原后只剩左上角一点可见，
        // 表现为「窗口变得很小」。relayout() 早已防了这一手，这里必须同样防。
        if IsIconic(hwnd) != 0 {
            return;
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
        // 兜底：正常窗口不可能小于最小尺寸的一半（逻辑 720×480）。真拿到更小的值
        // 只可能是窗口处于异常状态（被 DWM 收起、正在销毁），此时宁可不裁剪。
        if w < 320 || h < 200 {
            return;
        }

        let dpi = GetDpiForWindow(hwnd).max(96);
        let rad = ((radius as f64) * (dpi as f64 / 96.0)).round() as i32;
        let d = rad * 2; // 椭圆直径 = 2 × 半径

        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, d, d);
        if rgn == 0 {
            crate::log_warn!("[effects] CreateRoundRectRgn 失败");
            return;
        }
        if SetWindowRgn(hwnd, rgn, 1) == 0 {
            // 失败时 region 所有权还在我们手里，要释放
            DeleteObject(rgn);
            crate::log_warn!("[effects] SetWindowRgn 失败");
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

/// 清掉会让 DWM 绘制窗口 frame 的样式位，让窗口成为真正的无边框窗口。
///
/// tao 为了拿到 DWM 阴影，**保留了 WS_CAPTION | WS_THICKFRAME**（readest#6362
/// 有完整分析：无边框窗口靠这两个样式位换 DWM 阴影，代价是 DWM 会沿
/// **窗口矩形**画 1px 边框）。这圈边框是 DWM 合成层，**不受 SetWindowRgn
/// 影响**——所以四角被裁成圆角后，圆角外仍会留一圈「直角亮线」，
/// 这正是用户截图里那个去不掉的直角。
///
/// 我们不需要 DWM 的阴影和边框（材质、圆角、拖动、缩放都自己做），
/// 直接清掉这些样式位，DWM 就什么 frame 都不画了。
fn strip_window_frame(hwnd: isize) {
    unsafe {
        extern "system" {
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
        }
        const GWL_STYLE: i32 = -16;
        const WS_CAPTION: isize = 0x00C0_0000;
        const WS_THICKFRAME: isize = 0x0004_0000;
        const WS_MINIMIZEBOX: isize = 0x0002_0000;
        const WS_MAXIMIZEBOX: isize = 0x0001_0000;
        const WS_SYSMENU: isize = 0x0008_0000;
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOMOVE: u32 = 0x0002;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_FRAMECHANGED: u32 = 0x0020;

        let mask = WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU;
        let cur = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if cur == 0 {
            crate::log_warn!("[effects] 读取窗口样式失败");
            return;
        }
        let want = cur & !mask;
        if cur == want {
            return;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, want);
        // SWP_FRAMECHANGED 必须带上，否则样式改动不会立刻反映到窗口外观
        SetWindowPos(
            hwnd,
            0,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        crate::log_print!(
            "[effects] 已移除窗口 frame 样式位（0x{:X} → 0x{:X}）",
            cur, want
        );
    }
}

/// 关闭 DWM 对非客户区的渲染（标题栏边框 + 窗口阴影）。
///
/// 背景：tao 的无边框实现**保留了 WS_CAPTION 样式位**，只是用 WM_NCCALCSIZE
/// 把非客户区吞掉。于是 DWM 依然认为这是个带标题栏的窗口，会沿着
/// **窗口矩形**画一圈 1px 边框和投影——它是 DWM 合成层，**不受 SetWindowRgn
/// 影响**。结果就是：四角被 SetWindowRgn 裁成圆角，但圆角外还留着一圈
/// 直角的亮线/阴影（用户截图里那个「直角」就是这么来的）。
///
/// DWMNCRP_DISABLED 让 DWM 完全不管非客户区，窗口形状就只剩我们的区域裁剪。
fn disable_nc_rendering(hwnd: isize) {
    const DWMWA_NCRENDERING_POLICY: u32 = 2;
    const DWMNCRP_DISABLED: u32 = 1;
    unsafe {
        let policy: u32 = DWMNCRP_DISABLED;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            crate::log_warn!("[effects] 关闭 NC 渲染失败 hr=0x{:08X}", hr);
        }
    }
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
            crate::log_warn!("[effects] immersive dark 设置失败 hr=0x{:08X}", hr);
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

/// 是否处于「圆角模式」（圆角 + 无系统 blur，四角真透明）
pub fn is_round() -> bool {
    crate::settings::get().round_mode
}

/// 关闭系统模糊（圆角模式）。窗口退化为纯透明 layered 窗口：
/// 四角 100% 干净（没有 acrylic 的矩形残留），材质观感全部由 CSS 层负责。
fn clear_accent(hwnd: isize) {
    let _ = set_accent_state(hwnd, ACCENT_DISABLED, 0);
    nudge(hwnd);
}

/// Win10 1809–22H2：真·acrylic（tint 透明度取自「材质浓度」配置）
fn apply_acrylic(hwnd: isize, dark: bool) -> Result<(), i32> {
    if is_round() {
        // 圆角模式：不施加任何系统 blur
        clear_accent(hwnd);
        return Ok(());
    }
    // 直角模式才会走到这里（圆角模式在上面的分支直接 clear_accent 返回），
    // 所以 alpha() 取到的就是「直角档」那个值。
    let alpha = crate::settings::get().alpha();
    set_accent_state(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, tint_for(dark, alpha))
}

/// 切换「圆角模式」后立即生效（无需重启）
///
/// ⚠️ 不能无脑 `apply_acrylic`：Win11 走 **SystemBackdrop(Mica)** 档，正常材质是 Mica 而非 acrylic。
/// - **开启圆角**：必须关掉**所有**系统材质（Mica/SystemBackdrop 的 backdrop + accent 都要清），
///   否则系统材质的矩形 blur 仍在，圆角外会留直角残留；
/// - **关闭圆角**：必须按当前档位**完整恢复**（SystemBackdrop→Mica、Acrylic→acrylic），
///   只施加 acrylic 恢复不了 Mica，表现为「关了圆角材质还是圆角模式的样子」。
pub fn apply_round_mode(hwnd: isize, dark: bool) {
    if is_round() {
        clear_system_material(hwnd, current_tier());
        clear_accent(hwnd);
    } else {
        let _ = apply(hwnd, dark);
    }
}

// ---------------------------------------------------------------------------
// 强制圆角的窗口（托盘菜单）
// ---------------------------------------------------------------------------

/// 关掉窗口上**所有**系统材质，让四角真透明。
///
/// 系统材质（system backdrop / mica / accent acrylic）恒为**矩形**铺满整个窗口矩形，
/// CSS 的 border-radius 切不掉它。所以任何「必须用 CSS 圆角」的窗口都要先关干净，
/// 否则圆角外会留一圈直角残留 —— 这正是主窗口早先反复踩的坑。
fn clear_system_material(hwnd: isize, tier: Tier) {
    unsafe {
        match tier {
            Tier::SystemBackdrop => {
                let v: u32 = DWMSBT_NONE;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &v as *const _ as *const c_void,
                    std::mem::size_of::<u32>() as u32,
                );
            }
            Tier::Mica => {
                let v: u32 = 0;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_MICA_EFFECT,
                    &v as *const _ as *const c_void,
                    std::mem::size_of::<u32>() as u32,
                );
            }
            Tier::Acrylic => {
                let _ = set_accent_state(hwnd, ACCENT_DISABLED, 0);
            }
            Tier::Solid => {}
        }
    }
    nudge(hwnd);
}

/// 对**必须用 CSS 圆角**的窗口施加材质（托盘菜单专用）。
///
/// 托盘菜单固定使用圆角外观，与主窗口的「圆角模式」开关无关。因此不能直接复用
/// `apply()`——它在直角模式下会真的施加 acrylic（矩形），配上 CSS 圆角就是四角残留。
/// 这里做的恰恰相反：**先关掉系统材质**，观感完全交给 CSS 层。
///
/// `TIER` 由主窗口的 `apply()` 设置；万一托盘菜单先跑（正常不会），这里自己探测一次。
pub fn apply_rounded(hwnd: isize, dark: bool) {
    let tier = current_tier();

    // 托盘菜单恒为 CSS 圆角外观，必须先关干净系统材质（见 clear_system_material），
    // 所以**无论当前档位**都无条件 strip frame + 关 NC 渲染，观感完全交给 CSS 层。
    // 这里不能按 tier 判断跳过：回退「失焦保持材质」后，Win11 的 tier 又变回
    // SystemBackdrop，若是跳过 strip，菜单四角会再次残留直角、与 CSS 圆角打架。
    strip_window_frame(hwnd);
    disable_nc_rendering(hwnd);
    set_immersive_dark(hwnd, dark);
    clear_system_material(hwnd, tier);
}

/// 材质浓度变化后立即重刷 acrylic（设置页滑块用）
pub fn reapply_material(hwnd: isize, dark: bool) {
    if current_tier() == Tier::Acrylic {
        let _ = apply_acrylic(hwnd, dark);
        nudge(hwnd);
    }
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
/// ⚠️ 0x0201 是 **WM_LBUTTONDOWN**，0x0202 才是 **WM_LBUTTONUP**。
///
/// 这里曾经写成 0x0201，等于把「鼠标按下」当成「抬起」：
/// 拖动/缩放在按下的瞬间就被收尾（清状态 + ReleaseCapture），而真正的抬起
/// 反而没人处理 —— 状态与鼠标捕获会一直残留到**下一次按下**。下一次按下
/// 又会用上次的起点执行一次拖动/缩放，于是：
///   - 拉伸过一次之后，再点窗口任何地方（尤其外部）窗口会突然缩得很小；
///   - 鼠标捕获一直攥在手里，后续的 WM_LBUTTONDOWN 被我们吞掉、传不到 WebView，
///     前端边缘热区再也收不到 mousedown —— 表现为「边缘/四角拉不动」。
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
/// 系统要收回鼠标捕获时发送（按 Esc、弹出模态框、切换窗口等）
const WM_CANCELMODE: u32 = 0x001F;

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
        // 归零节流计时，保证第一次移动立即生效
        LAST_MOVE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        SetCapture(hwnd);
    }
}

/// 拖动移动节流（毫秒）。
///
/// Win10 上 acrylic 的已知性能 bug：高轮询率鼠标（250–1000Hz）会让**每个**
/// WM_MOUSEMOVE 都触发一次 SetWindowPos → DWM 逐帧重算模糊，事件队列迅速积压，
/// 表现为「窗口追不上鼠标」、松手后仍漂移（幽灵光标）。
/// 业界公认的绕过办法：把移动事件**节流到刷新率级别**（见 vscode-vibrancy#80）。
/// 因为我们是按「绝对光标坐标 − 起点」算目标位置，丢弃中间事件不影响落点精度。
const MOVE_THROTTLE_MS: u64 = 16; // ≈60Hz，匹配 DWM 合成节奏
static LAST_MOVE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 鼠标捕获是否仍在本窗口。
///
/// 自定义拖动 / 缩放靠 `SetCapture` 把鼠标消息引到本窗口。一旦 capture 丢掉
/// （失焦、被别的窗口抢走、用户点了窗口外部），我们**收不到 WM_LBUTTONUP**，
/// 复位逻辑就永远不会执行 —— 残留的起点会在下一次 `WM_MOUSEMOVE` 时用过期
/// 原点算位移，把窗口一次「拉」到与光标匹配的尺寸。
/// 实测症状：拉伸完点一下窗口外部，窗口会突然缩得很小。
/// 所以每次处理前都先校验一次，capture 不在就复位。
fn capture_is(hwnd: isize) -> bool {
    unsafe extern "system" {
        fn GetCapture() -> isize;
    }
    unsafe { GetCapture() == hwnd }
}

/// 把窗口移动到「光标起点 → 当前位置」的位移处
fn drag_move_to_cursor(hwnd: isize) {
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
        }
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

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
    }
}

/// 处理拖动期间的鼠标消息；返回 true 表示已处理（调用方应直接 return）
fn handle_drag_msg(hwnd: isize, msg: u32) -> bool {
    if !DRAG_ARMED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    // capture 丢了（点了外部 / 失焦）→ 复位，别再用过期起点改窗口位置
    if !capture_is(hwnd) {
        DRAG_ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    unsafe {
        extern "system" {
            fn ReleaseCapture() -> i32;
        }
        // 状态还活着却又来了一次「按下」→ 上一次的抬起丢了，丢弃旧起点
        if msg == WM_LBUTTONDOWN {
            DRAG_ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
            ReleaseCapture();
            return false;
        }
        if msg == WM_MOUSEMOVE {
            // 节流：过密的移动直接丢弃，避免 DWM 事件积压（落点精度不受影响）
            let now = now_ms();
            if now.saturating_sub(LAST_MOVE_MS.load(std::sync::atomic::Ordering::Relaxed))
                < MOVE_THROTTLE_MS
            {
                return true;
            }
            LAST_MOVE_MS.store(now, std::sync::atomic::Ordering::Relaxed);
            drag_move_to_cursor(hwnd);
            return true;
        }
        if msg == WM_LBUTTONUP {
            // 落点校正：确保最终位置精确对齐光标（最后一个 move 可能被节流丢弃）
            drag_move_to_cursor(hwnd);
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
// 自定义缩放（替代系统缩放循环 SC_SIZE）
//
// 系统缩放和系统移动循环是同一个病：Win10 acrylic 下每个 WM_SIZE 都要重算
// 模糊，事件在队列里积压 → 缩放严重滞后。移动已经用节流解决了，缩放同理
// 接管过来：前端在窗口边缘的热区按下 → resize_start(edge) → SetCapture，
// WndProc 里按 16ms 节流调 SetWindowPos 改「位置 + 尺寸」。
//
// 同时必须拦截 WM_NCHITTEST 返回 HTCLIENT：tao 对窗口边缘会返回
// HTLEFT/HTTOP 等，那会把鼠标交给**系统缩放循环**，节流就无从谈起了。
// ---------------------------------------------------------------------------

const WM_NCHITTEST: u32 = 0x0084;
const HTCLIENT: isize = 1;

/// 缩放方向位掩码：1=上 2=下 4=左 8=右（可组合，如左上 = 5）
static RESIZE_EDGE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// (起始光标 X, 起始光标 Y, 起始 left, 起始 top, 起始 right, 起始 bottom)
static RESIZE_ORIGIN: std::sync::Mutex<(i32, i32, i32, i32, i32, i32)> =
    std::sync::Mutex::new((0, 0, 0, 0, 0, 0));

/// 窗口最小尺寸（**逻辑 px**，运行时按窗口 DPI 换算成物理 px）。
/// 侧栏是固定的 208px，窗口再窄内容区就放不下设置页的标签 + 说明文字，
/// 会被挤成一列竖排（用户实测）。
const MIN_LOGICAL_W: i32 = 720;
const MIN_LOGICAL_H: i32 = 480;

/// 窗口与屏幕边缘之间留的余量（逻辑 px）
const FIT_EDGE: f64 = 12.0;

/// 「不跟随系统缩放」的运行时开关。放静态是因为自定义缩放的逐帧计算也要读它。
static IGNORE_DPI: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 设计尺寸 = 100% 缩放时窗口的物理尺寸
const DESIGN_W: f64 = 1060.0;
const DESIGN_H: f64 = 690.0;

pub fn ignore_dpi() -> bool {
    IGNORE_DPI.load(std::sync::atomic::Ordering::Relaxed)
}

/// 按「不跟随系统缩放」策略设置 webview 缩放与窗口尺寸，两个方向都走这里。
///
/// 开启时：webview 缩放设为 1/scale，窗口改用**物理**像素定尺寸 ——
/// 于是 1 CSS px = 1 物理像素，不管系统缩放怎么调，界面看上去都和 100% 时一样。
/// 关闭时：缩放回 1，窗口按逻辑单位并夹进工作区（就是常规行为）。
pub fn apply_dpi_policy(window: &tauri::WebviewWindow) {
    let ignore = crate::settings::get().ignore_dpi_scale;
    IGNORE_DPI.store(ignore, std::sync::atomic::Ordering::Relaxed);

    let Ok(Some(mon)) = window.current_monitor() else {
        let _ = window.set_zoom(1.0);
        return;
    };
    let scale = mon.scale_factor().max(0.1);

    if !ignore {
        let _ = window.set_zoom(1.0);
        fit_to_work_area(window);
        return;
    }

    let _ = window.set_zoom(1.0 / scale);

    let wa = mon.work_area().size;
    let max_w = (wa.width as f64 - FIT_EDGE * 2.0).max(320.0);
    let max_h = (wa.height as f64 - FIT_EDGE * 2.0).max(320.0);
    let w = DESIGN_W.min(max_w);
    let h = DESIGN_H.min(max_h);

    // 最小尺寸也按物理像素 —— 此模式下逻辑与物理是 1:1
    let _ = window.set_min_size(Some(tauri::Size::Physical(tauri::PhysicalSize::new(
        (MIN_LOGICAL_W as f64).min(max_w) as u32,
        (MIN_LOGICAL_H as f64).min(max_h) as u32,
    ))));

    if let Ok(cur) = window.inner_size() {
        if (cur.width as f64 - w).abs() > 1.0 || (cur.height as f64 - h).abs() > 1.0 {
            let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(
                w as u32, h as u32,
            )));
        }
    }
}

/// 把窗口夹进当前显示器的工作区（全部按**逻辑 px** 计算）。
///
/// 为什么需要：默认窗口 1060×690 逻辑 px。系统缩放调高后可用**逻辑**分辨率变小 ——
/// 1080p 屏在 150% 下只有 1280×720，扣掉任务栏约 680，690 高的窗口直接装不进去，
/// 底部状态栏被切掉，看起来就是「布局乱了」。
///
/// 顺带把最小尺寸也夹一遍：小屏 + 高缩放时 720×480 都可能超出工作区，
/// 那样窗口既放不下又缩不回去，只能拖来拖去。
pub fn fit_to_work_area(window: &tauri::WebviewWindow) {
    let Ok(Some(mon)) = window.current_monitor() else {
        return;
    };
    let scale = mon.scale_factor();
    if !(scale > 0.0) {
        return;
    }
    let wa = mon.work_area().size;
    // 下限 320 是个兜底：真拿到异常小的值时不至于把窗口压成一条
    let max_w = (wa.width as f64 / scale - FIT_EDGE * 2.0).max(320.0);
    let max_h = (wa.height as f64 / scale - FIT_EDGE * 2.0).max(320.0);

    let min_w = (MIN_LOGICAL_W as f64).min(max_w);
    let min_h = (MIN_LOGICAL_H as f64).min(max_h);
    let _ = window.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(
        min_w, min_h,
    ))));

    let Ok(cur) = window.inner_size() else {
        return;
    };
    let (cw, ch) = (cur.width as f64 / scale, cur.height as f64 / scale);
    let (mut nw, mut nh) = (cw, ch);
    if nw > max_w {
        nw = max_w;
    }
    if nh > max_h {
        nh = max_h;
    }
    if nw < min_w {
        nw = min_w;
    }
    if nh < min_h {
        nh = min_h;
    }
    // 只有真需要改才动窗口，避免每次启动都触发一次多余的 Resized
    if (nw - cw).abs() > 0.5 || (nh - ch).abs() > 0.5 {
        crate::log_print!(
            "[effects] 窗口超出工作区，按缩放 {scale} 夹到 {nw:.0}×{nh:.0}（原 {cw:.0}×{ch:.0}）"
        );
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(nw, nh)));
    }
}

pub fn begin_custom_resize(hwnd: isize, edge: u32) {
    if edge == 0 {
        return;
    }
    unsafe {
        extern "system" {
            fn GetCursorPos(pt: *mut Rect) -> i32;
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
            fn SetCapture(hwnd: isize) -> isize;
        }
        let mut cur: Rect = std::mem::zeroed();
        let mut wr: Rect = std::mem::zeroed();
        if GetCursorPos(&mut cur) == 0 || GetWindowRect(hwnd, &mut wr) == 0 {
            return;
        }
        if let Ok(mut o) = RESIZE_ORIGIN.lock() {
            *o = (cur.left, cur.top, wr.left, wr.top, wr.right, wr.bottom);
        }
        RESIZE_EDGE.store(edge, std::sync::atomic::Ordering::Relaxed);
        LAST_MOVE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        SetCapture(hwnd);
    }
}

fn resize_move_to_cursor(hwnd: isize) {
    let edge = RESIZE_EDGE.load(std::sync::atomic::Ordering::Relaxed);
    if edge == 0 {
        return;
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
            fn GetDpiForWindow(hwnd: isize) -> u32;
        }
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

        // 最小尺寸按逻辑 px 定义，这里按窗口 DPI 换算成物理像素。
        // 「不跟随系统缩放」时 1 CSS px = 1 物理 px，不再乘比例。
        let dpi = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
        let factor = if ignore_dpi() { 1.0 } else { dpi };
        let min_w = (MIN_LOGICAL_W as f64 * factor).round() as i32;
        let min_h = (MIN_LOGICAL_H as f64 * factor).round() as i32;

        let (sx, sy, l0, t0, r0, b0) = RESIZE_ORIGIN
            .lock()
            .map(|o| *o)
            .unwrap_or((0, 0, 0, 0, 0, 0));
        let mut cur: Rect = std::mem::zeroed();
        if GetCursorPos(&mut cur) == 0 {
            return;
        }
        let dx = cur.left - sx;
        let dy = cur.top - sy;

        let (mut l, mut t, mut r, mut b) = (l0, t0, r0, b0);
        if edge & 4 != 0 {
            l = l0 + dx; // 左
        }
        if edge & 8 != 0 {
            r = r0 + dx; // 右
        }
        if edge & 1 != 0 {
            t = t0 + dy; // 上
        }
        if edge & 2 != 0 {
            b = b0 + dy; // 下
        }

        // 最小尺寸：只夹住被拖动的那条边，另一侧保持不动
        if r - l < min_w {
            if edge & 4 != 0 {
                l = r - min_w;
            } else if edge & 8 != 0 {
                r = l + min_w;
            }
        }
        if b - t < min_h {
            if edge & 1 != 0 {
                t = b - min_h;
            } else if edge & 2 != 0 {
                b = t + min_h;
            }
        }

        SetWindowPos(hwnd, 0, l, t, r - l, b - t, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn handle_resize_msg(hwnd: isize, msg: u32) -> bool {
    if RESIZE_EDGE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return false;
    }
    // capture 丢了（点了外部 / 失焦 / 被别的窗口抢走）→ 复位。
    // 不这么做的话，残留的起点会在鼠标回到窗口时把窗口「拉」成极小尺寸。
    if !capture_is(hwnd) {
        RESIZE_EDGE.store(0, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    unsafe {
        extern "system" {
            fn ReleaseCapture() -> i32;
        }
        // 状态还活着却又来了一次「按下」→ 说明上一次的抬起丢了。
        // 丢掉旧起点、不执行本次缩放，否则会拿过期原点把窗口拉变形。
        if msg == WM_LBUTTONDOWN {
            RESIZE_EDGE.store(0, std::sync::atomic::Ordering::Relaxed);
            ReleaseCapture();
            return false;
        }
        if msg == WM_MOUSEMOVE {
            let now = now_ms();
            if now.saturating_sub(LAST_MOVE_MS.load(std::sync::atomic::Ordering::Relaxed))
                < MOVE_THROTTLE_MS
            {
                return true;
            }
            LAST_MOVE_MS.store(now, std::sync::atomic::Ordering::Relaxed);
            resize_move_to_cursor(hwnd);
            return true;
        }
        if msg == WM_LBUTTONUP {
            // 收尾校正一次，保证最终尺寸精确
            resize_move_to_cursor(hwnd);
            RESIZE_EDGE.store(0, std::sync::atomic::Ordering::Relaxed);
            ReleaseCapture();
            return true;
        }
    }
    false
}

/// 前端在窗口边缘热区 mousedown 时调用。edge 为方向位掩码（1上 2下 4左 8右）
#[tauri::command]
pub fn resize_start(app: tauri::AppHandle, edge: u32) -> Result<(), String> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(h) = w.hwnd() {
            begin_custom_resize(h.0 as isize, edge);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WndProc 子类：修焦点切换时的原生标题栏闪现
// ---------------------------------------------------------------------------

const WM_NCACTIVATE: u32 = 0x0086;
/// 鼠标捕获转移：lparam 为接手 capture 的窗口（0 = 没人持有）
const WM_CAPTURECHANGED: u32 = 0x0215;

static ORIG_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// tao 用「保留 WS_CAPTION 样式位 + WM_NCCALCSIZE 吞掉非客户区」实现无边框，
/// 但窗口**失焦/聚焦**时 Windows 发 WM_NCACTIVATE，仍会把原生标题栏画出来
/// （用户实测：一点外部就出现原生标题栏 + 白边）。
///
/// 修法是给窗口过程挂子类（subclass），拦截这条消息后用 `lparam = -1`
/// 调 DefWindowProc——微软文档认可的做法，含义是「激活状态变更，但
/// 不要重绘非客户区」。只拦这一条消息，其余全部原样转交 tao。
/// 子类过程里要用到 AppHandle（收到单实例唤醒消息时要唤回窗口并通知前端），
/// 而 `hook_proc` 是裸函数指针、拿不到闭包，所以把句柄存进全局。
static HOOK_APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

// ---------------------------------------------------------------------------
// 让 Tauri 自带的「拖拽 / 缩放边框」子窗口不再抢鼠标
// ---------------------------------------------------------------------------

/// tao 给无边框窗口创建的边框子窗口类名
const RESIZE_BORDER_CLASS: &str = "TAURI_DRAG_RESIZE_BORDERS";
const GWLP_WNDPROC_BORDER: i32 = -4;
/// 边缘判定容差（物理 px）。这个子窗口本身只有 4px 宽，留一点余量。
const RESIZE_EDGE_TOLERANCE: i32 = 6;

static BORDER_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static BORDER_ORIG: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 按光标相对窗口的位置算出缩放方向位掩码（1上 2下 4左 8右，可组合，如左上 = 5）
fn edge_from_cursor(main: isize) -> u32 {
    unsafe {
        extern "system" {
            fn GetCursorPos(pt: *mut Rect) -> i32;
            fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        }
        let mut cur: Rect = std::mem::zeroed();
        let mut wr: Rect = std::mem::zeroed();
        if GetCursorPos(&mut cur) == 0 || GetWindowRect(main, &mut wr) == 0 {
            return 0;
        }
        let t = RESIZE_EDGE_TOLERANCE;
        let mut e = 0u32;
        if cur.top <= wr.top + t {
            e |= 1;
        }
        if cur.top >= wr.bottom - t {
            e |= 2;
        }
        if cur.left <= wr.left + t {
            e |= 4;
        }
        if cur.left >= wr.right - t {
            e |= 8;
        }
        e
    }
}

/// Tauri v2（tao）给无边框窗口额外建了一个 4px 宽的边框子窗口，类名固定为
/// `TAURI_DRAG_RESIZE_BORDERS`，用来提供**系统级**的边缘拖拽/缩放。
///
/// 它盖在窗口最外圈、位于 WebView **之上**，会先拿到鼠标：前端那些贴着边框的
/// 缩放热区于是永远收不到 mousedown —— 实测边缘 0–3px 全被它吃掉，四角再叠加
/// 圆角，可点面积只剩几个像素，表现就是「边缘很难拉、四个角根本拉不动」。
/// 而它走的正是我们想绕开的那套系统缩放循环（Win10 acrylic 下严重滞后）。
///
/// 做法：给它挂子类，自己接管 ——
///   - `WM_NCHITTEST` 返回 HTCLIENT，让它自己收下边缘的鼠标消息
///     （**不能**返回 HTTRANSPARENT 指望穿透：该子窗口不在主线程上，
///      HTTRANSPARENT 只在同一线程的窗口之间生效，实测穿透后消息直接消失）；
///   - `WM_LBUTTONDOWN` 时按光标位置判定方向，交给主窗口的自定义缩放
///     （SetCapture 到主窗口，后续移动/抬起由主窗口的 hook 接管）。
pub fn disable_resize_border(hwnd: isize) {
    unsafe {
        extern "system" {
            fn EnumChildWindows(
                parent: isize,
                cb: unsafe extern "system" fn(isize, isize) -> i32,
                param: isize,
            ) -> i32;
            fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
            fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        }

        unsafe extern "system" fn enum_cb(child: isize, param: isize) -> i32 {
            let mut buf = [0u16; 128];
            let n = unsafe { GetClassNameW(child, buf.as_mut_ptr(), 128) };
            if n <= 0 {
                return 1;
            }
            if String::from_utf16_lossy(&buf[..n as usize]) == RESIZE_BORDER_CLASS {
                unsafe { *(param as *mut isize) = child };
                return 0; // 找到了，停止枚举
            }
            1
        }

        let mut found: isize = 0;
        EnumChildWindows(hwnd, enum_cb, &mut found as *mut isize as isize);
        if found == 0 {
            return;
        }
        // 窗口重建后子窗口句柄会变，所以每次都比一遍；没变就直接返回（幂等）
        if BORDER_HWND.load(std::sync::atomic::Ordering::Relaxed) == found {
            return;
        }
        let prev = SetWindowLongPtrW(found, GWLP_WNDPROC_BORDER, border_proc as isize);
        if prev == 0 {
            crate::log_warn!("[effects] 缩放边框子窗口子类化失败");
            return;
        }
        BORDER_ORIG.store(prev, std::sync::atomic::Ordering::Relaxed);
        BORDER_HWND.store(found, std::sync::atomic::Ordering::Relaxed);
        crate::log_warn!("[effects] 缩放边框子窗口已接管（hwnd={found}）");
    }
}

unsafe extern "system" fn border_proc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe extern "system" {
        fn CallWindowProcW(prev: isize, hwnd: isize, msg: u32, wparam: usize, lparam: isize)
            -> isize;
        fn GetAncestor(hwnd: isize, flags: u32) -> isize;
    }
    // 这个子窗口覆盖在窗口最外圈、位于 WebView 之上，边缘的鼠标事件只有它能收到。
    // 所以边缘缩放必须在这里起手：按下时按光标位置判定方向，交给主窗口的
    // 自定义缩放（SetCapture 到主窗口，后续 mousemove/up 由主窗口的 hook 接管）。
    if msg == WM_NCHITTEST {
        return HTCLIENT;
    }
    if msg == WM_LBUTTONDOWN {
        let main = unsafe { GetAncestor(hwnd, 2) }; // GA_ROOT
        let target = if main != 0 { main } else { hwnd };
        let edge = edge_from_cursor(target);
        if edge != 0 {
            begin_custom_resize(target, edge);
            return 0;
        }
    }
    let prev = BORDER_ORIG.load(std::sync::atomic::Ordering::Relaxed);
    unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) }
}

pub fn install_focus_hook(hwnd: isize, app: &tauri::AppHandle) {
    let _ = HOOK_APP.set(app.clone());

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

        // 第二个实例请求唤回：**在本进程里**走与托盘唤回完全相同的路径，
        // 而不是让对面跨进程直接操作窗口（那会把窗口尺寸/排版搞坏）。
        // 放后台线程做：show/emit 会跟事件循环打交道，不该在 WndProc 里同步执行。
        if msg == crate::singleton::WM_QINGTIE_WAKE {
            if let Some(app) = HOOK_APP.get() {
                use tauri::Emitter;
                let app = app.clone();
                std::thread::spawn(move || {
                    crate::tray::show_main(&app);
                    let _ = app.emit("singleton-wake", ());
                });
            }
            return 0;
        }
        if msg == WM_NCACTIVATE {
            // lparam = -1 → 禁止重绘非客户区，原生标题栏就永远画不出来
            return unsafe { DefWindowProcW(hwnd, msg, wparam, -1) };
        }
        // 鼠标捕获被抢走 / 释放（点了窗口外部、失焦、别的窗口 SetCapture）：
        // 此时收不到 WM_LBUTTONUP，必须在这里复位，否则拖动/缩放的「起点」
        // 会一直留着，下一次 mousemove 用过期原点把窗口拉到错误尺寸。
        if msg == WM_CAPTURECHANGED {
            if lparam != hwnd {
                RESIZE_EDGE.store(0, std::sync::atomic::Ordering::Relaxed);
                DRAG_ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        // 系统收回鼠标捕获（Esc、模态框、切换窗口）：同样必须复位
        if msg == WM_CANCELMODE {
            RESIZE_EDGE.store(0, std::sync::atomic::Ordering::Relaxed);
            DRAG_ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        // 剪贴板变化：事件驱动监听（替代轮询），读取并落盘后推给前端
        if msg == 0x031D {
            // WM_CLIPBOARDUPDATE
            crate::clipboard::on_clipboard_update();
            return 0;
        }
        // 命中测试统一返回 HTCLIENT：tao 会把窗口边缘判成 HTLEFT/HTTOP 等，
        // 那等于把边缘交给**系统缩放循环**（无法节流 → 缩放滞后）。
        // 这里收回边界判定，缩放改由前端热区 + 自定义缩放实现。
        if msg == WM_NCHITTEST {
            return HTCLIENT;
        }
        // 自定义缩放：接管鼠标移动/抬起
        if handle_resize_msg(hwnd, msg) {
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
        crate::log_warn!("[effects] WndProc 子类挂载失败（SetWindowLongPtrW 返回 0）");
        return;
    }
    ORIG_WNDPROC.store(orig, std::sync::atomic::Ordering::Relaxed);
    crate::log_print!("[effects] 焦点 hook 已挂载（WM_NCACTIVATE → 不重绘 NC 区）");
}
