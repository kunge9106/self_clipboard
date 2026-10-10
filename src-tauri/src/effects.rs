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
/// accent_state = 0：彻底关闭系统 blur。窗口变成纯透明的 layered 窗口，
/// 四角是真透明，不再有 acrylic 铺满矩形留下的残留。见 `pure_mode`。
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

    // 先清掉 WS_CAPTION | WS_THICKFRAME 等样式位——DWM 正是靠它们才
    // 沿窗口矩形画 1px 边框（SystemBackdrop 档需要 DWM 提供 backdrop 与圆角，不能动）
    if tier != Tier::SystemBackdrop {
        strip_window_frame(hwnd);
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
            eprintln!("[effects] 读取窗口样式失败");
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
        println!(
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
            eprintln!("[effects] 关闭 NC 渲染失败 hr=0x{:08X}", hr);
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

/// 是否处于「纯净模式」（不用系统 blur，四角真透明）
pub fn is_pure() -> bool {
    crate::settings::get().pure_mode
}

/// 关闭系统模糊（纯净模式）。窗口退化为纯透明 layered 窗口：
/// 四角 100% 干净（没有 acrylic 的矩形残留），材质观感全部由 CSS 层负责。
fn clear_accent(hwnd: isize) {
    let _ = set_accent_state(hwnd, ACCENT_DISABLED, 0);
    nudge(hwnd);
}

/// Win10 1809–22H2：真·acrylic（tint 透明度取自「材质浓度」配置）
fn apply_acrylic(hwnd: isize, dark: bool) -> Result<(), i32> {
    if is_pure() {
        // 纯净模式：不施加任何系统 blur
        clear_accent(hwnd);
        return Ok(());
    }
    let alpha = crate::settings::get().material_alpha;
    set_accent_state(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, tint_for(dark, alpha))
}

/// 切换「纯净模式」后立即生效（无需重启）
pub fn apply_pure_mode(hwnd: isize, dark: bool) {
    let _ = apply_acrylic(hwnd, dark);
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
    unsafe {
        extern "system" {
            fn ReleaseCapture() -> i32;
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

        // 最小尺寸按逻辑 px 定义，这里按窗口 DPI 换算成物理像素
        let dpi = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
        let min_w = (MIN_LOGICAL_W as f64 * dpi).round() as i32;
        let min_h = (MIN_LOGICAL_H as f64 * dpi).round() as i32;

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
    unsafe {
        extern "system" {
            fn ReleaseCapture() -> i32;
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
        eprintln!("[effects] WndProc 子类挂载失败（SetWindowLongPtrW 返回 0）");
        return;
    }
    ORIG_WNDPROC.store(orig, std::sync::atomic::Ordering::Relaxed);
    println!("[effects] 焦点 hook 已挂载（WM_NCACTIVATE → 不重绘 NC 区）");
}
