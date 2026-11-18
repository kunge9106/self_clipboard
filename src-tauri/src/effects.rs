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
use tauri::Manager;

// ---------------------------------------------------------------------------
// 材质：全部手写 DWM
//
// 曾短暂试过 Tauri 2 的 `set_effects`（内部是 window-vibrancy）：它按系统版本自动
// 选 Mica / Acrylic，省事，但**没有"清空 / 重建"的入口** —— 而本项目大量依赖
// 「先清后设」来强制 DWM 重建（见 `restore_visuals` 的注释）。那条路最后放弃了。
//
// 现在档位探测、单档施加、主题重刷、驻留挂起/恢复、看门狗兜底都在这一个文件里，
// 改动时注意：这些机制是互相咬合的（例如 `suspend_material` 清了材质，
// 就必须有 `resume_material` 补回来），不要单独改一处。
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

// DwmSetWindowAttribute
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
/// Win11 22000+：让非 UWP 窗口使用 host backdrop brush 语义。
/// 与 `SYSTEMBACKDROP_TYPE` 搭配时，backdrop 改从**窗口后方**采样并绘制到重定向表面，
/// 而不是从视觉后方采样 —— 表现是最大化/还原动画、以及被其他窗口遮挡时材质不再失效
/// （原生 Acrylic 在动画期间会短暂退回实色，这是 DWM 的既有行为）。
const DWMWA_MICA_EFFECT: u32 = 1029;

// DWM_SYSTEMBACKDROP_TYPE
const DWMSBT_NONE: u32 = 1;
/// 主窗口 backdrop（Mica 系）。与 `DWMSBT_TRANSIENTWINDOW` 同为**有效材质**，
/// 用来做「值切换」时中间帧不会露出灰底/透明。
const DWMSBT_MAINWINDOW: u32 = 2;
/// 瞬态窗口 backdrop（Acrylic 系）—— 本应用主窗口用的就是它。
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

/// `DwmExtendFrameIntoClientArea` 的边距。全 -1 = 整窗都当"玻璃"。
/// 字段名必须与 Win32 的 `MARGINS` 逐字一致 —— 这是 FFI 结构体，
/// 改名会让人以为可以「按 Rust 习惯重命名」，下次改布局时容易出事。
#[repr(C)]
#[allow(non_snake_case)]
struct MARGINS {
    cxLeftWidth: i32,
    cxRightWidth: i32,
    cyTopHeight: i32,
    cyBottomHeight: i32,
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

/// 物理坐标矩形，user32 的 GetWindowRect / SetWindowRgn 都用它。
///
/// `pub` 是给 `screenclip` 判断「截图覆盖层是否铺满全屏」用的：那里也要读窗口矩形，
/// 再定义一个同形状的结构会让两处 `GetWindowRect` 的 extern 声明互相打架。
#[repr(C)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
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

    /// dwmapi.dll —— 把 DWM 边框扩展进客户区（acrylic/mica 的前提）
    fn DwmExtendFrameIntoClientArea(hwnd: isize, margins: *const MARGINS) -> i32;

    /// dwmapi.dll —— 只用于把属性读回来打日志，判断「没设上」还是「没生效」
    fn DwmGetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *mut c_void,
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

}

static TIER: OnceLock<Tier> = OnceLock::new();
static BUILD: OnceLock<u32> = OnceLock::new();

pub fn current_tier() -> Tier {
    // **不能默认 Solid**：前端在页面加载早期就会问一次 `effect_info`，
    // 那时第一次 `apply_with` 可能还没跑。默认 Solid 会让前端套上
    // `body.tier-solid` → CSS 直接把窗口画成 94.6% 的不透明实色，
    // 而且**整个会话都不会再变**（`applyEffectInfo` 只在启动时调一次）。
    // 这正是「窗口像一块实色、开关一次高级材质才恢复」的根因之一。
    // 未初始化时现场探测一次，保证任何时候回答的都是真实档位。
    tier_now()
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
    // 按 Windows 版本选材质，优先级从新到旧：
    //   ≥22621 `SystemBackdrop`（DWM 系统 acrylic：圆角 + 噪声层都由系统给，观感最好）
    //   ≥22000 `Mica`
    //   ≥17763 `Acrylic`（Win10 的 `SetWindowCompositionAttribute`）
    //   更低    `Solid`（不施加材质，观感交给 CSS 实底）
    //
    // 已知代价（选 SystemBackdrop 就是接受它）：DWM 会在系统主题切换 / hide→show /
    // 非客户区重建之后把这层表面丢掉，此时属性回读仍是 3、hr=0 ——「属性在、画面没有」。
    // 恢复只能靠"再完整下发一次"（见 `refresh_theme_colors` → `restore_visuals`）。
    //
    // 曾评估过的替代方案：全部改用 Win10 accent acrylic（不依赖 DWM 表面，永不丢），
    // 代价是**圆角要自己裁、且没有系统噪声层**，实测观感与系统材质差距明显，故放弃。
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

/// 正在「屏幕外驻留」的主窗口 hwnd（0 = 没有驻留）。
///
/// 驻留期间窗口对 DWM 来说是不可见的：这时候既不该、也不必重新下发材质 ——
/// 那会把 [`suspend_material`] 刻意留下的「材质已清空」状态破坏掉，唤回时
/// 就没有「一次真实的值变化」可用了（见 `resume_material` 的注释）。
///
/// 只在 `park_main_window` / `resume_material` 这一对里改写。
static PARKED_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 这个窗口是否正处于「屏幕外驻留」。
fn is_suspended(hwnd: isize) -> bool {
    PARKED_HWND.load(std::sync::atomic::Ordering::Relaxed) == hwnd
}

/// 当前是否已有一个窗口驻留在屏幕外。
pub fn any_suspended() -> bool {
    PARKED_HWND.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// 清掉「驻留中」标记（窗口被销毁重建时用：旧 hwnd 已经不存在了）。
pub fn clear_suspended() {
    PARKED_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// 「现在该用哪个档位」：`TIER` 已由某次 `apply_with` 种下就用它，
/// 否则现场探测一次（驻留可能发生在任何一次 apply 之前）。
fn tier_now() -> Tier {
    match TIER.get() {
        Some(t) => *t,
        None => classify(detect_build()),
    }
}

/// 应用窗口材质。
///
/// `reason` 只用于日志：材质失效这类问题**只能靠「谁在什么时候动了它」定位** ——
/// 之前排查「最小化后不恢复」时，日志只有「材质应用完成」，根本看不出是谁触发的，
/// 只能靠猜。现在每条都带来源（startup / theme-refresh / toggle-advanced / restore / …）。
pub fn apply_with(hwnd: isize, dark: bool, reason: &str) -> Tier {
    // 驻留中的窗口不要去碰材质：见 `PARKED_HWND` / `suspend_material`。
    if is_suspended(hwnd) {
        crate::log_print!("[effects] 跳过材质下发（窗口驻留中，来源={reason}）");
        return current_tier();
    }
    // 取证：窗口的扩展样式决定 backdrop 能不能画。
    //   WS_EX_LAYERED(0x00080000)          —— 分层窗口由应用自己合成，DWM 不画 backdrop
    //   WS_EX_NOREDIRECTIONBITMAP(0x00200000) —— 透明窗口常用，部分情况下同样影响合成
    // 「属性对、画面没有」如果配上异常的 EX_STYLE，根因就在窗口创建侧而不在材质设置侧。
    unsafe {
        extern "system" {
            fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
        }
        let style = GetWindowLongW(hwnd, -16) as u32;
        let ex = GetWindowLongW(hwnd, -20) as u32;
        crate::log_print!(
            "[effects] 窗口样式 style=0x{style:08X} ex=0x{ex:08X}（来源={reason}）"
        );
    }
    let build = detect_build();
    let tier = classify(build);
    let _ = BUILD.set(build);
    let _ = TIER.set(tier);

    crate::log_print!(
        "[effects] 应用材质（来源={reason}）build={} tier={} 高级材质={} (radius≤{}px)",
        build,
        tier.as_str(),
        if is_round() { "开" } else { "关" },
        match tier.max_radius() {
            u32::MAX => 0,
            r => r,
        }
    );

    // 先清掉 WS_CAPTION | WS_THICKFRAME 等样式位——DWM 正是靠它们才
    // 沿窗口矩形画 1px 边框
    //
    // **所有档位都要剥，包括 SystemBackdrop**。早先这里写的是「SystemBackdrop 档
    // 需要 DWM 提供 backdrop 与圆角，不能动」，于是该档位保留了这两个样式位，靠
    // tao「用 WM_NCCALCSIZE 吞掉非客户区」装作无边框。这是个隐含假设：只要 tao 的
    // NC 处理被打乱（主题切换、后台释放后窗口重建、tao 的 set_window_flags 把
    // WS_CAPTION 又加回来），原生标题栏与 DWM 那圈 frame 就整条露出来 ——
    // 用户看到的就是「UI 四周像 Win7 那样的边框」。
    //
    // 材质与样式位无关：`DWMWA_SYSTEMBACKDROP_TYPE` 是按**窗口矩形**画的，
    // 不依赖非客户区。剥掉之后材质照常，只是 DWM 不再替我们画边框 ——
    // 边框与圆角本来就由我们自己控制。
    strip_window_frame(hwnd, false);
    // Tauri 的 4px 缩放边框子窗口会挡在 WebView 之上、抢走边缘鼠标事件，
    // 每次 apply 都确认一遍（窗口重建后子窗口句柄会变）
    disable_resize_border(hwnd);

    // 标题栏/系统区域跟随深色
    set_immersive_dark(hwnd, dark);

    // 系统材质档（Win11 backdrop）**不能**关 NC 渲染：backdrop 就是作为
    // 非客户区画的，关了它材质也没了。那一档只把 1px 边框颜色设成透明。
    // 其它档（Win10 acrylic / 无材质）继续用关 NC 渲染来消边框。
    if tier_now() == Tier::SystemBackdrop {
        clear_window_border(hwnd);
    } else {
        disable_nc_rendering(hwnd);
    }

    // ⚠ **语义已反转**（2026-11-05，用户实测要求「高级材质打开才应该是毛玻璃」）：
    //   开（round_mode = true）→ **施加**系统材质，这才是毛玻璃；
    //   关                    → 清掉所有系统材质，窗口退化为透明 layered，
    //                           由 CSS 用更高浓度的实底兜住。
    // 早先是反的（开＝关掉模糊换干净四角），名字还叫「圆角模式」，两头都不对：
    // 用户以为开了是高级效果，实际得到的是"没有模糊"。
    let result = if is_round() {
        match tier {
            Tier::SystemBackdrop => apply_system_backdrop(hwnd),
            Tier::Mica => apply_mica(hwnd),
            Tier::Acrylic => apply_acrylic(hwnd, dark),
            Tier::Solid => Ok(()),
        }
    } else {
        clear_system_material(hwnd, tier);
        clear_accent(hwnd);
        Ok(())
    };

    match result {
        Ok(()) => {
            crate::log_print!("[effects] 材质应用完成：{}（来源={reason}）", tier.as_str());
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

/// 最小化 → 恢复时补一次材质（**这是「最小化再打开变成灰扑扑纯色」的修复**）。
///
/// Windows 在窗口最小化时会把 `DWMWA_SYSTEMBACKDROP_TYPE` 丢掉（DWM 不再为不可见
/// 窗口维护 backdrop）。恢复后窗口是激活的、内容正常，但材质永远回不来 ——
/// 用户看到的就是「还是那种灰扑扑的材质，不是毛玻璃」。
///
/// 为什么不能只补一次：恢复动画期间 DWM 会**忽略**该属性（返回 S_OK 但没生效），
/// 而不同的机器/壁纸下动画时长不一样。所以补两次（0.35s / 0.9s）。
/// `apply` 是幂等的（重复设同一个值无副作用），多补一次的成本可以忽略。
///
/// 判断方式是 `IsIconic` 的状态翻转：Resized 在最小化与恢复时都会触发，
/// 靠「上一次是不是最小化」区分到底是哪一个。
pub fn reapply_after_restore(window: &tauri::WebviewWindow) {
    let Ok(h) = window.hwnd() else {
        return;
    };
    let hwnd = h.0 as isize;
    use std::sync::atomic::{AtomicBool, Ordering};
    static WAS_ICONIC: AtomicBool = AtomicBool::new(false);
    unsafe {
        extern "system" {
            fn IsIconic(hwnd: isize) -> i32;
        }
        let iconic = IsIconic(hwnd) != 0;
        let was = WAS_ICONIC.swap(iconic, Ordering::Relaxed);
        // 正在最小化 → 只记状态；本来就没最小化 → 只是普通缩放，不补
        if iconic || !was {
            return;
        }
    }
    crate::log_print!("[effects] 从最小化恢复 → 补材质（DWM 会丢 backdrop）");
    let w = window.clone();
    for delay in [350u64, 900] {
        let w2 = w.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            // 窗口此时已可见：走**静默**补涂。响亮复位（清 backdrop 再设回）
            // 会当着用户的面拆掉重建 —— 那正是「从灰重建为材质」的闪烁
            let _ = reapply_visuals_for(&w2, "restore");
        });
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


/// 逐帧 SetWindowRgn 对窗口本体做区域裁剪非常贵（大窗口尤甚），
/// 会让缩放明显卡顿。这里改成「缩放中不裁剪、缩放后补一次」。
/// 窗口尺寸变化后收尾。
///
/// ⚠ **这里是「材质莫名其妙丢失、点一下深浅色就恢复」的真正原因**。
///
/// 原来无条件在 120ms 后用 `SetWindowRgn` 把窗口裁成圆角。而在有系统
/// 材质的档位上（Acrylic / Mica / SystemBackdrop），材质是 DWM **按窗口矩形**画的：
/// 一旦用 `SetWindowRgn` 把窗口区域改成圆角矩形，DWM 对这块表面的 backdrop 就废了 ——
/// 窗口还在、内容还在，但毛玻璃没了，退成实色。
///
/// 而切深浅色会重新下发材质 → 看起来「点一下就好了」。
/// 这就是用户看到的现象。
///
/// 所以两条路分开：
///   · 有系统材质（高级材质开）：**不裁区域**，改为重新下发一次材质 ——
///     拖动/缩放本身也会让 DWM 重建表面，顺手补一次最稳；
///   · 无系统材质（高级材质关）：照旧用区域裁剪画圆角。
/// 缩放世代号：`begin_resize` 每次自增，延迟回调里比对是否还是最后一次。
static RESIZE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn begin_resize(hwnd: isize, dark: bool) {
    let gen = RESIZE_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        if RESIZE_GEN.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return; // 还在拖，等最后一次
        }
        // 圆角由 CSS（`--radius` + overflow）或系统给，不裁区域 ——
        // 裁区域会破坏 DWM 的 backdrop（见本函数文档）。
        //
        // 这里**刻意**只发材质、不走 `restore_visuals`：缩放全程窗口都可见，
        // NC 渲染策略不会被重置，不需要那两步；而 `strip_window_frame(force=true)`
        // 在可见窗口上会强制整窗重绘，缩放停止瞬间会闪一下。
        // 这不是抄漏 —— 场景不需要，就不付那个代价。
        let _ = apply_with(hwnd, dark, "resize-end");
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
/// `force = true` 时：即使样式位已经干净，也照样下发 `SWP_FRAMECHANGED`。
///
/// 主题切换后 DWM 会重建非客户区 —— 样式位看着没被改过，DWM 那边却已经"忘掉"
/// 我们剥离过边框，于是原生标题栏与那圈投影重新出现。这种情况下只删样式位不发
/// `FRAMECHANGED` 等于什么都没做，必须强制它重算一次。
pub fn strip_window_frame(hwnd: isize, force: bool) {
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
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOMOVE: u32 = 0x0002;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_FRAMECHANGED: u32 = 0x0020;

        // **只剥 `WS_CAPTION | WS_THICKFRAME`**。
        //
        // DWM 沿窗口矩形画那圈边框，靠的就是这两个位；最小化/最大化/系统菜单
        // 这三个位与边框无关，但**系统要靠它们判断「这个窗口能不能最小化」** ——
        // 一起剥掉的话，点击任务栏图标不会最小化（表现为「点了没反应」，
        // 而 Windows 上所有正常窗口都是「再点一下就收起」）。
        let mask = WS_CAPTION | WS_THICKFRAME;
        let cur = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if cur == 0 {
            crate::log_warn!("[effects] 读取窗口样式失败");
            return;
        }
        let want = cur & !mask;
        crate::log_print!(
            "[effects] strip frame：GWL_STYLE=0x{cur:X} → 0x{want:X}（force={force}）"
        );
        if cur == want && !force {
            return;
        }
        if cur != want {
            SetWindowLongPtrW(hwnd, GWL_STYLE, want);
        }
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
    }
}

/// 「本窗口现在该用的深浅」。
///
/// 手动切主题时**不再**调 `set_theme`（见 `set_theme_mode` 的注释），所以
/// `window.theme()` 返回的始终是**系统**主题。拿它推深浅，用户手动选了深色、
/// 系统是浅色时就会算反 —— tint 变浅、卡片发白，看起来正是「灰扑扑」。
/// 所有需要 dark 的地方统一从这里取，别各自拼一份。
pub fn effective_dark(window: &tauri::WebviewWindow) -> bool {
    // 窗口参数保留是为了不动所有调用点，但 system 档**不能用 `window.theme()`**：
    // `set_theme_mode` 早就不调 `window.set_theme()` 了（tao 会重加被剥掉的
    // WS_CAPTION，Win10 上原生标题栏重现），所以窗口的 theme 永远是建窗时的旧值
    // —— 这就是「选跟随系统却不变」的根因。
    let _ = window;
    crate::effects::is_dark_now()
}

/// 把 DWM 侧与窗口视觉有关的状态**整份**重设一遍，返回实际档位。
///
/// 这是「整份重设」的配方，**只在真的需要重建表面时用**（对外出口是
/// `refresh_theme_full`）。其余时机各有各的语义，不要混用：
///   · 日常静默补涂 → `reapply_visuals_for`（设相同值 = no-op，不闪）
///   · 托盘驻留唤回 → `resume_material`（驻留期间材质已被清空）
///   · 系统主题变化 → `schedule_system_theme_refresh`（只同步颜色）
///
/// ⚠ 这里有血泪：早先有个"只重下材质"的旁路实现手抄了其中几行
/// （剥边框 + 下发材质 + nudge），漏掉 `disable_nc_rendering` 与
/// `set_immersive_dark`。结果托盘唤回后
/// `DWMWA_SYSTEMBACKDROP_TYPE` 回读是 3（Acrylic）、`hr=0`，**属性明明在、
/// 画面却是灰底** —— 因为 hide→show 后 DWM 把 `DWMWA_NCRENDERING_POLICY`
/// 重置回 `USEWINDOWSTYLE`，backdrop 就不再被绘制。属性在、画面没有，
/// 是最难查的一类：日志一切正常，只有用户的眼睛知道不对。
///
/// 「照抄一段能用的代码」本身就是隐患，抄漏一行就是一次线上 bug。
pub fn restore_visuals(hwnd: isize, dark: bool, reason: &str) -> Tier {
    // 驻留中的窗口一律不碰（同 `apply_with`）
    if is_suspended(hwnd) {
        crate::log_print!("[effects] 跳过整份复位（窗口驻留中，来源={reason}）");
        return current_tier();
    }
    set_immersive_dark(hwnd, dark);
    // force：DWM 可能已经「忘掉」剥过边框，样式位没变也必须重发 FRAMECHANGED，
    // 否则原生标题栏会带着投影重新出现
    strip_window_frame(hwnd, true);
    if current_tier() == Tier::SystemBackdrop {
        clear_window_border(hwnd);
    } else {
        disable_nc_rendering(hwnd);
    }
    // ⚠ **不再 clear 成 NONE**：清成 NONE 再设回会把窗口的合成面拆了重建，
    // 实测反而造成「属性对、画面没有」（回读 type=3 hr=0）。
    //
    // 但 DWM 对「设置相同值」直接跳过（3→3 无效），所以这里在**两个都有效的值**
    // 之间切一次（瞬态窗口 ←→ 主窗口）—— 值真的变了会触发重建，
    // 中间又不会留下「无材质」的空白态。随后 apply_with 设回目标值。
    unsafe {
        extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *const c_void,
                size: u32,
            ) -> i32;
        }
        let alt: u32 = DWMSBT_MAINWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &alt as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
    // ⚠ 这里**故意不 flush**：中间值（Mica）如果被真正合成一帧，用户就能看到
    // 一下"变成灰板再变回毛玻璃"——就是「手动切换时闪一下」。
    // 两次写入紧挨着（中间只隔一次属性写），DWM 会把它们合到同一次合成里：
    // 目标值照样是一次"真实变化"（会被接受），而中间态不会上屏。
    // 曾担心"不 flush 就会被当成没变化、留在 Mica 上"——实测那取决于两次写入之间
    // 是否跨了合成帧，连续写是同一帧，不会停在中间值。
    //
    // 重放宿主窗口的「全透明」状态。tao 只在**创建窗口时**调过一次
    // `DwmEnableBlurBehindWindow`，hide→show 之后 DWM 会把它丢掉：
    // 窗口不再透明，backdrop 画在下面也透不出来，整窗变成灰底 ——
    // 属性全对、画面全灰。这是 tauri#16341 给的手法：空区域 + 立即失效重绘。
    reassert_transparency(hwnd);
    let tier = apply_with(hwnd, dark, reason);
    nudge(hwnd);
    // **材质就位之后**才强制重绘。
    //
    // 这一步原先写在 `reassert_transparency` 里，顺序上是「backdrop 刚被清成
    // NONE、目标值还没设回」——`InvalidateRect` 的立即失效会逼 DWM 当场合成
    // 这一帧中间态：窗口本体全透明 + 没有 backdrop = 桌面直接透出来。
    // 用户看到的就是「切深浅色时闪一下，直接透明出背景」。
    // 同步序列写完再失效，DWM 按 vsync 只看到最终状态。
    force_redraw(hwnd);
    log_backdrop_state(hwnd, reason);
    tier
}

/// 把宿主窗口重新断言为「全透明」。
///
/// `DwmEnableBlurBehindWindow` + 空区域（`CreateRectRgn(0,0,-1,-1)`）是 tao 创建
/// 透明窗口时用的原始配方；重放它等于恢复「窗口本体不画任何东西」的状态。
/// 单独清 backdrop 不够 —— 那是**材质**层，而透明是**窗口**层，两层得分别管。
///
/// ⚠ 这里**只重放属性，不催重绘**。催重绘（`InvalidateRect`）会立即合成一帧，
/// 而调用点上 backdrop 往往还处在「已清成 NONE、尚未设回」的中间态 —— 那一帧
/// 就是「整窗透明、桌面透出来」。需要重绘时由调用方在材质就位之后显式调
/// [`force_redraw`]。
fn reassert_transparency(hwnd: isize) {
    // 旧实现重放的是 `DwmEnableBlurBehindWindow` + 空区域（Vista 时代的透明手法）。
    // 它和 `DWMWA_SYSTEMBACKDROP_TYPE` 是两套机制：混用时 DWM 可能按"没有 backdrop"
    // 去合成 —— 表现就是「属性 3、画面平的」。现在统一成文档配方的那一步：
    // 把 DWM 边框重新铺满客户区（窗口重新可见/唤回后必须重做一次）。
    unsafe {
        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        let hr = DwmExtendFrameIntoClientArea(hwnd, &margins);
        if hr < 0 {
            crate::log_warn!("[effects] 重新扩展 DWM 边框失败 hr=0x{:08X}", hr as u32);
        }
    }
}

/// 立即失效整窗，逼 DWM 用当前状态重画一次。
///
/// 只在**材质已经下发到位**之后调用。提前调会把中间态（无 backdrop + 全透明）
/// 合成到屏幕上 —— 见 [`reassert_transparency`] 的注释。
pub fn force_redraw(hwnd: isize) {
    unsafe {
        extern "system" {
            fn InvalidateRect(hwnd: isize, rect: *const c_void, erase: i32) -> i32;
        }
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

/// 静默补涂（窗口**可见期间**用）。
///
/// 与 [`restore_visuals_for`] 一样两层齐全（blur-behind 重放 + WebView2 背景），
/// 但**不做任何会引发可见重建的动作**：
///   · 不 `clear_system_backdrop` —— 值变化会强制 DWM 把 backdrop 拆了重建，
///     当着用户的面「先灰、再变毛玻璃」。这正是托盘唤回闪一下的来源
///     （用户实测：「打开的瞬间会闪一下，从灰色重建为高级材质」）；
///   · 无 force 的 strip —— 样式位没变就不发 FRAMECHANGED，无整窗重绘。
///
/// 值真的丢了时，Set 一个不同的值照样生效（DWM 自己重建）；值没丢就是纯 no-op。
/// **不闪靠的就是这个语义**（设相同值 DWM 直接跳过）。完整的响亮复位只在
/// 「用户自己触发了切换」或「窗口刚回到前台」时刻用（见 `refresh_theme_full`），
/// 其余可见期间一律走这里。
pub fn reapply_visuals_for(window: &tauri::WebviewWindow, reason: &str) -> Option<Tier> {
    let Ok(h) = window.hwnd() else {
        return None;
    };
    if is_suspended(h.0 as isize) {
        return Some(current_tier());
    }
    // blur-behind 重放仍然要做：它不拆 backdrop，而 hide→show 后被 DWM 丢掉的
    // 恰恰是它。静默 ≠ 省略，只是不拆房子。
    reassert_transparency(h.0 as isize);
    let tier = apply_with(h.0 as isize, effective_dark(window), reason);
    if let Err(e) = window.set_background_color(Some(tauri::window::Color(0, 0, 0, 0))) {
        crate::log_warn!("[effects] 重设 WebView 透明背景失败：{e}");
    }
    Some(tier)
}

// ---------------------------------------------------------------------------
// 驻留（收进托盘）与唤回：一对成对的材质操作
// ---------------------------------------------------------------------------

/// 收进托盘后调用（此时窗口**已经在屏幕外**）：把系统材质清掉，并记为驻留中。
///
/// 为什么必须清：`SetWindowPos` 把窗口移出屏幕后，DWM 会把它从合成里摘出去。
/// 唤回时如果属性**仍是目标值**，`DwmSetWindowAttribute` 就是一次同值 no-op ——
/// DWM 没有任何理由重建 backdrop，用户看到的就是「关掉再打开没材质」。
/// 而此刻清掉是**零成本**的：窗口在屏幕外，这一帧没人看得见。
/// 「真实的那个值变化」留给 [`resume_material`]。
///
/// 顺带把「驻留中」这个状态记下来：期间所有材质补涂（看门狗、聚焦、缩放、
/// 主题刷新）都会自动跳过，否则它们会把属性又设回目标值，前提就没了。
pub fn suspend_material(hwnd: isize) {
    use std::sync::atomic::Ordering;
    if hwnd == 0 {
        return;
    }
    PARKED_HWND.store(hwnd, Ordering::SeqCst);
    let tier = tier_now();
    // 圆角模式下本来就没有系统材质；清一遍是幂等的。
    clear_system_material(hwnd, tier);
    clear_accent(hwnd);
    crate::log_print!(
        "[effects] 驻留：已清空系统材质（tier={}），唤回时将重新下发",
        tier.as_str()
    );
}

/// 从屏幕外驻留唤回后调用（此时窗口**已移回屏幕内**）：按当前深浅重新下发材质。
///
/// 与 [`suspend_material`] 成对：驻留期间材质已被清空，所以这里这次下发是一次
/// **真实的值变化**，DWM 必然重建 backdrop —— 不再依赖「移出屏幕不会丢材质」
/// 这个假设（它在用户机器上并不成立，正是「关掉再打开材质不应用」的来源）。
pub fn resume_material(window: &tauri::WebviewWindow, reason: &str) -> Option<Tier> {
    use std::sync::atomic::Ordering;
    let Ok(h) = window.hwnd() else {
        return None;
    };
    let hwnd = h.0 as isize;
    // 先解除驻留标记，否则 apply_with 会把自己跳过
    PARKED_HWND.store(0, Ordering::SeqCst);
    let dark = effective_dark(window);
    // 做一次**值切换**（主窗口 ←→ 瞬态）：窗口在屏幕外时 DWM 不一定为它合成，
    // 「清空后重新下发」这一步有概率被视为 no-op —— 表现就是「从托盘打开有几率
    // 没材质」（用户实测）。值切换能让属性真的变化，DWM 必须重建。
    // 代价是可能合成一帧中间材质（Mica）；相比「没材质」这个取舍是划算的。
    unsafe {
        let alt: u32 = DWMSBT_MAINWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &alt as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
    let tier = apply_with(hwnd, dark, reason);
    dwm_flush();
    if let Err(e) = window.set_background_color(Some(tauri::window::Color(0, 0, 0, 0))) {
        crate::log_warn!("[effects] 重设 WebView 透明背景失败：{e}");
    }
    crate::log_print!("[effects] 唤回：材质已重新下发（dark={dark}，tier={}）", tier.as_str());
    Some(tier)
}

/// 完整体检：DWM 层（[`restore_visuals`]）+ WebView2 内容层。
///
/// 「窗口重新出现」的路径都应该用这个，因为透明是**两层**的：
///   · 宿主窗口 —— DWM 的 backdrop + blur-behind；
///   · WebView2 内容 —— 自己的 `DefaultBackgroundColor`。
/// 只补 DWM 那层，webview 一旦在 hide→show / GPU 进程重建后回到不透明，
/// 就会把 backdrop 整块盖住，用户看到的就是「灰扑扑的实心窗口」。
pub fn restore_visuals_for(window: &tauri::WebviewWindow, reason: &str) -> Option<Tier> {
    let Ok(h) = window.hwnd() else {
        return None;
    };
    let dark = effective_dark(window);
    let tier = restore_visuals(h.0 as isize, dark, reason);
    // alpha=0 才是透明（Windows 上非 0 的 alpha 会被替换成 255，所以只能全透）
    if let Err(e) = window.set_background_color(Some(tauri::window::Color(0, 0, 0, 0))) {
        crate::log_warn!("[effects] 重设 WebView 透明背景失败：{e}");
    }
    Some(tier)
}

/// 把 backdrop 相关属性读回来打日志。
///
/// 「材质没生效」有两类完全不同的原因，日志里必须能分清，否则只能靠猜：
///   · 属性丢了 / 没设上（hr 失败或 type=0）→ 我们的下发没成功，或有人清掉了；
///   · 属性在、画面没有（type≠0 但看不到模糊）→ DWM 没把它挂到当前表面上，
///     需要一次 frame recalc（即 `refresh_theme_colors` → `restore_visuals` 做的事）。
fn log_backdrop_state(hwnd: isize, reason: &str) {
    unsafe {
        let mut kind: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &mut kind as *mut _ as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        crate::log_print!(
            "[effects] backdrop 回读（来源={reason}）type={kind} hr=0x{:08X}",
            hr as u32
        );
    }
}

/// 把 DWM 画的 1px 窗口边框颜色设为「无」。
///
/// Win11 上窗口带一圈极细的边框（失焦时更明显）。过去我们用
/// `DWMNCRP_DISABLED` 关掉整个非客户区渲染来消掉它 —— 但**系统材质
/// （backdrop / Mica）本身就是以"边框"形式绘制的**，关掉 NC 渲染等于把材质
/// 一起关了（属性还在、画面没了）。所以系统材质档改用「只把边框颜色设成透明」。
const DWMWA_BORDER_COLOR: u32 = 34;
/// `DWMWA_COLOR_NONE`：表示不画边框。
const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

fn clear_window_border(hwnd: isize) {
    unsafe {
        let v: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &v as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// 关闭 DWM 对非客户区的渲染（1px 边框 + 投影）。
///
/// ⚠ **SystemBackdrop 档绝对不能用它**：backdrop / Mica 本身就是以非客户区的
/// 形式绘制的，关掉 NC 渲染等于把材质一起关掉 —— 回读 type=3、hr=0，画面却
/// 一片平色。这一档改用 [`clear_window_border`]（只把边框颜色设成透明）。
///
/// 其它档（Win10 acrylic / 无材质实底）继续用它：那些档没有系统材质可关，
/// 关掉 NC 才能真正消掉那一圈边框和投影。
pub fn disable_nc_rendering(hwnd: isize) {
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
        // **文档配方（顺序固定）**：
        //   ① `DwmExtendFrameIntoClientArea(hwnd, {-1,-1,-1,-1})` —— 把 DWM 边框
        //      铺满整个客户区，整块窗口才成为"一片玻璃"。`DWMWA_SYSTEMBACKDROP_TYPE`
        //      **只在这片玻璃上绘制**；不扩展边框，设了类型也不会画出模糊。
        //   ② `DWMWA_SYSTEMBACKDROP_TYPE = TRANSIENTWINDOW`（acrylic）。
        //   ③ 圆角由系统给。
        //
        // 早先这里设的是 `DWMWA_USE_HOSTBACKDROPBRUSH` —— 那是给
        // Win32 + Windows.UI.Composition（XAML 岛屿）用的，普通窗口设它只会让
        // DWM 拿"宿主画刷"去涂，表现正是「一块平色、没有模糊」，而且切主题后
        // 状态更乱。已删除。
        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        let hr_frame = DwmExtendFrameIntoClientArea(hwnd, &margins);

        let v: u32 = DWMSBT_TRANSIENTWINDOW;
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &v as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            crate::log_warn!(
                "[effects] 设置 backdrop 失败 hr=0x{:08X}（扩展边框 hr=0x{:08X}），退回 Mica",
                hr as u32,
                hr_frame as u32
            );
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
            crate::log_warn!("[effects] 设置 Mica 失败 hr=0x{:08X}", hr as u32);
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
    // 是否施加由 `apply` 统一判断（关闭高级材质时根本走不到这里）。
    // tint 恒为 8%：明暗由 CSS 层负责，系统只提供「模糊」这一件事。
    let alpha = crate::settings::get().alpha();
    set_accent_state(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, tint_for(dark, alpha))
}

/// 切换「高级材质」后立即生效（无需重启）。
///
/// 直接重跑 `apply` 即可 —— 施加还是清除，由 `apply` 里那一个分支统一决定。
/// 早先这里手写了「开就清、关就恢复」两段逻辑，与 `apply` 各判断一次，
/// 语义一改就要改两处（本次正是踩到：改完 `apply` 忘了这里，切开关毫无反应）。
/// **一处判断胜过两处同步**。
pub fn apply_round_mode(hwnd: isize, dark: bool) {
    let _ = apply_with(hwnd, dark, "toggle-advanced");
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
    strip_window_frame(hwnd, false);
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

/// 等 DWM 完成一次合成。
///
/// 材质类操作里只有「同帧隐藏 / 显示重建」这一处需要等。
fn dwm_flush() {
    unsafe {
        extern "system" {
            fn DwmFlush() -> i32;
        }
        let _ = DwmFlush();
    }
}

/// 主题变化后的材质刷新：与 `focus-refresh` 同一套静默重下。
///
/// **不做** backdrop 值切换、immersive dark 翻转、额外 FRAMECHANGED ——
/// 这些都会在可见窗口上闪（用户实测）。Windows 侧只需：正确 immersive dark +
/// 剥回 tao 可能加回的 WS_CAPTION + 重下材质 + WebView 全透。
pub fn refresh_theme_visuals(window: &tauri::WebviewWindow, reason: &str) -> Option<Tier> {
    let Ok(h) = window.hwnd() else {
        return None;
    };
    let hwnd = h.0 as isize;
    if is_suspended(hwnd) {
        return Some(current_tier());
    }
    // 只在这里设一次 WebView 透明背景（`reapply_visuals_for` 内部还会设一次，
    // 之前这里前后各设一次等于让 webview 多重绘一轮，是闪的次要来源）。
    let t = reapply_visuals_for(window, reason);
    dwm_flush();
    crate::log_print!(
        "[effects] 主题变化 → 完整重下（来源={reason}，dark={}，tier={}）",
        is_dark_now(),
        t.map(|x| x.as_str()).unwrap_or("unknown")
    );
    log_backdrop_state(hwnd, reason);
    t
}

/// 上一次「自动」主题刷新的时刻（毫秒）。
///
/// 一次系统深浅切换会同时惊动三条路径：`WM_SETTINGCHANGE` 钩子、400ms 看门狗、
/// 前端事件。它们都会调本函数 —— 实测一次切换刷了 **3 次**材质（日志里三组
/// 「应用完成 + 完整重下」），而每次重建都是一次可见的闪。这里用时间窗去重：
/// 1 秒内只认第一次，其余直接跳过。手动切换不走这条，不受影响。
static LAST_AUTO_REFRESH_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 系统主题变化（无焦点）：立即静默重下 + 150ms 再补一次（对齐 focus-refresh）。
/// 「系统主题变过、但材质还没重建」的标记。
///
/// 系统切深浅时主窗口往往**正在被看**（用户刚点了 Windows 设置），此时重建
/// backdrop 一定会闪。所以先只切 CSS，把材质重建挂起；等窗口失焦（用户已经
/// 去操作别的窗口，看不见它）再补 —— 这样「材质一定跟上」和「绝不闪」两者都能拿到。
static THEME_REBUILD_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 窗口失焦 / 隐藏时调用：如果系统主题变过，趁用户看不见把材质重建掉。
pub fn flush_pending_theme_rebuild(window: &tauri::WebviewWindow) {
    use std::sync::atomic::Ordering;
    if !THEME_REBUILD_PENDING.swap(false, Ordering::SeqCst) {
        return;
    }
    if any_suspended() {
        // 驻留/隐藏中：`resume_material` 唤回时会按当前深浅重新下发，这里不用管
        return;
    }
    crate::log_print!("[effects] 窗口回到前台 → 补做挂起的主题材质重建（完整复位）");
    // 必须是完整复位：静默重下只写属性、不重建表面，恢复不了"属性 3 / 画面没有"。
    refresh_theme_full(window, "pending-theme");
}

/// 窗口即将重新显示（驻留唤回 / show）时清掉挂起标记 —— 那个路径会自己下发材质。
pub fn clear_pending_theme_rebuild() {
    THEME_REBUILD_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
}

pub fn schedule_system_theme_refresh(window: tauri::WebviewWindow, reason: &'static str) {
    // **不重建主窗口材质**（用户实测结论：「接上不如不接上」）。
    //
    // 系统主题变化这条路，tao 已经重走过 `set_window_flags`；要让材质"对上"就得
    // 重建一次 backdrop —— 而重建在可见窗口上一定是可见变化（闪 / 短暂透明）。
    // 实测下来这样比"不重建"更难看，所以这条路径只做两件不会闪的事：
    //   ① 把当前深浅推给前端 —— CSS 是同步的，切换不出帧；
    //   ② 面板 / 托盘菜单按新深浅重刷 —— 体量小，重建不可感知。
    // 主窗口材质留到**用户手动切主题**时再重建（`refresh_theme_manual`，
    // 那条路已验证：不闪，且材质一定能恢复）。
    use tauri::Emitter;
    let now = now_ms();
    let last = LAST_AUTO_REFRESH_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 800 {
        return; // 同一事件被多条路径看到，只处理第一次
    }
    LAST_AUTO_REFRESH_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    clear_theme_event_pending();
    // ① **立刻**把「窗口是不是深色」推下去（只改这一个属性，不重建材质）：
    //    DWM 会按新属性重算 backdrop 的颜色，材质当场就和新深浅一致 ——
    //    这解决「切完是半深半浅」。不碰 backdrop 类型、不碰 WebView 背景，所以不闪。
    if let Ok(h) = window.hwnd() {
        set_immersive_dark(h.0 as isize, is_dark_now());
        dwm_flush();
    }
    // ② 材料重建仍挂起：万一某些系统版本上属性变化不足以触发重算，
    //    等窗口失焦（用户看不见）时再完整重建一次兜底。
    THEME_REBUILD_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);

    let app = window.app_handle();
    let dark = is_dark_now();
    let _ = window.emit("system-theme-changed", dark);
    #[cfg(windows)]
    if let Some(pw) = app.get_webview_window(crate::popup::PANEL_LABEL) {
        if let Ok(ph) = pw.hwnd() {
            // **只同步深浅属性，不重建材质**。面板是可见的独立窗口，
            // 重建（清/重设 backdrop）会让它在那一瞬间直接失去材质
            // （用户实测：「面板打开时切深浅，材质会消失」）。
            // `set_immersive_dark` 足以让 DWM 按新深浅重算 backdrop 颜色。
            set_immersive_dark(ph.0 as isize, dark);
        }
        let _ = app.emit_to(crate::popup::PANEL_LABEL, "panel-theme", dark);
    }
    crate::tray::sync_menu_appearance(app);
    crate::log_print!("[effects] 系统主题变化 → 同步颜色（不动主窗口材质，来源={reason}）");
}

static THEME_EVENT_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn note_theme_event_handled() {
    THEME_EVENT_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn theme_event_handled(_dark: bool) -> bool {
    THEME_EVENT_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst)
}

pub fn clear_theme_event_pending() {
    THEME_EVENT_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 系统主题变过 → 挂起一次材质重建，等窗口**回到前台**时再补。
///
/// 这是早期那版的行为：系统换深浅时窗口通常在后台（用户在设置里操作），
/// 此刻不碰材质（会闪、而且看不见）；等它回到前台再做完整复位 ——
/// 用户看到的就是"材质跟着窗口一起回来了"，既不闪也一定生效。
pub fn mark_pending_theme_rebuild() {
    THEME_REBUILD_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 系统主题变化（tao `ThemeChanged`）的入口：转给「只同步颜色」那条路。
/// **不能**在这里做 `restore_visuals_for`（会闪），也不能重建主窗口材质。
pub fn refresh_after_theme_switch(window: &tauri::WebviewWindow) {
    // 走系统主题那条统一入口：只同步颜色，不重建主窗口材质。
    schedule_system_theme_refresh(window.clone(), "theme-switch");
}

/// 统一别名：只做静默重下（看门狗兜底与 IPC 命令走这条，不闪）。
/// ⚠ 系统主题变化的自动跟跟随**不再走这里**（那条只同步颜色，见
/// `schedule_system_theme_refresh`）。
pub fn refresh_theme_colors(window: &tauri::WebviewWindow, reason: &str) {
    let _ = refresh_theme_visuals(window, reason);
}

/// **完整复位**：能恢复「属性还在、画面没有」的材质。
///
/// 只给"用户正看着窗口"的时机用 —— 那一刻重建一次是用户自己触发的，可接受：
///   · 系统换过深浅、窗口回到前台时（`flush_pending_theme_rebuild`）
///   · 检测到系统深浅变化、且窗口就在前台时（看门狗）
/// 手动点深浅那条路**不用**它（走静默重下，避免闪）。
pub fn refresh_theme_full(window: &tauri::WebviewWindow, reason: &str) {
    let t = restore_visuals_for(window, reason);
    dwm_flush();
    crate::log_print!(
        "[effects] 完整复位材质（来源={reason}，dark={}，tier={}）",
        is_dark_now(),
        t.map(|x| x.as_str()).unwrap_or("unknown")
    );
}

/// **手动**切深浅时用：一次静默重下，不重建表面。
///
/// 不做值切换、不做 `force_redraw` —— 两者都会让「中间值已设、目标值未设」的
/// 那一帧被合成出来，就是用户说的「切深浅闪一下」。
/// 深浅切换时 `apply_with` 里的 `set_immersive_dark` 本身就会变，
/// DWM 会按新属性重算 backdrop 颜色，一次 apply 足够。
pub fn refresh_theme_manual(window: &tauri::WebviewWindow, reason: &str) {
    let Ok(h) = window.hwnd() else {
        return;
    };
    let _ = h;
    let tier = tier_now();
    // **不做值切换**（用户要求「保留一个」，且值切换会合成中间帧 = 闪）。
    // 深浅切换时 `apply_with` 里的 `set_immersive_dark(dark)` 本身就会变，
    // DWM 会按新属性重算 backdrop 颜色 —— 一次 apply 就够。
    // 记一次时间戳：紧跟着的看门狗不应该再刷一遍。
    LAST_AUTO_REFRESH_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    let _ = refresh_theme_visuals(window, reason);
    dwm_flush();
    crate::log_print!(
        "[effects] 手动切换 → 静默重下一次（来源={reason}，dark={}，tier={}）",
        is_dark_now(),
        tier.as_str()
    );
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

/// 按设置把窗口主题钉成浅色/深色，或交还给系统（跟随 Windows）。
///
/// 传 `None` 就是「跟随系统」—— Tauri 会继续监听系统主题变化并派发事件，
/// 前端的 `onThemeChanged` 因此不用改，三档切换共用同一条链路。
/// 当前界面是不是深色。
///
/// light / dark 直接取设置；system 读系统的「应用主题」（注册表
/// `HKCU\...\Themes\Personalize\AppsUseLightTheme`）。
///
/// 注册表是这里唯一可靠的来源 —— 截图提示这类没有 Tauri 窗口的场合拿不到
/// 系统主题消息，而系统主题变化也不会有窗口消息可听。
pub fn is_dark_now() -> bool {
    match crate::settings::get().theme_mode.as_str() {
        "dark" => true,
        "light" => false,
        _ => !apps_use_light_theme(),
    }
}

/// 读系统的应用主题：true = 浅色，false = 深色。读不到按浅色处理。
fn apps_use_light_theme() -> bool {
    unsafe {
        extern "system" {
            fn RegOpenKeyExW(
                hkey: isize,
                subkey: *const u16,
                opts: u32,
                sam: u32,
                out: *mut isize,
            ) -> i32;
            // 签名必须与 about.rs 里的声明逐字一致：同一个 C 符号在同一个 crate
            // 里声明成两种样子，Rust 会报 clashing_extern_declarations；
            // 而参数个数写错更是直接错位调用。
            fn RegQueryValueExW(
                hkey: isize,
                value: *const u16,
                reserved: *mut u32,
                out_type: *mut u32,
                data: *mut u8,
                data_size: *mut u32,
            ) -> i32;
            fn RegCloseKey(hkey: isize) -> i32;
        }
        const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
        const KEY_READ: u32 = 0x20019;
        /// REG_DWORD 的类型码。`RegGetValueW` 通过 `out_type` **输出**实际类型，
        /// `flags` 传 0 表示不限类型。
        const REG_DWORD: u32 = 4;

        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let name: Vec<u16> = "AppsUseLightTheme".encode_utf16().chain(std::iter::once(0)).collect();

        let mut hkey: isize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return true;
        }
        let mut val: u32 = 1;
        let mut out_type: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        // 用 RegQueryValueExW（键已打开，只需要值名）而不是 RegGetValueW ——
        // 后者实测在这台机器上返回非 0（rc 失败），于是「读不到按浅色处理」，
        // 深色系统被判成浅色。日志里 `rust=false window=dark mq=true` 就是它。
        let rc = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut out_type,
            &mut val as *mut u32 as *mut u8,
            &mut len,
        );
        RegCloseKey(hkey);
        if rc != 0 || out_type != REG_DWORD {
            crate::log_warn!(
                "[theme] 读 AppsUseLightTheme 失败 rc={rc} type={out_type}，按浅色处理"
            );
            return true;
        }
        val != 0
    }
}

pub fn apply_theme_mode(window: &tauri::WebviewWindow) {
    let theme = match crate::settings::get().theme_mode.as_str() {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None,
    };
    let _ = window.set_theme(theme);
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
/// 边缘判定容差（物理 px）。
///
/// 取系统标准值 `SM_CXSIZEFRAME + SM_CXPADDEDBORDER`：100% DPI 下是 8，150% 下 12，
/// 200% 下 16。这正是资源管理器、记事本等原生窗口的可缩放边框宽度，也是用户
/// 形成「碰到边框就能拉」直觉的来源。
///
/// 原先写死 6（物理像素）没做 DPI 换算 —— 在 150% 屏上只有 4 个逻辑像素，
/// 只有系统标准的一半，表现就是「必须往里靠很多才拉得动」。系统这个值本身
/// 已经按当前 DPI 给出，不需要再自己乘缩放比。
fn resize_edge_tolerance(hwnd: isize) -> i32 {
    const SM_CXSIZEFRAME: i32 = 32;
    const SM_CXPADDEDBORDER: i32 = 92;
    unsafe {
        extern "system" {
            fn GetDpiForWindow(hwnd: isize) -> u32;
            fn GetSystemMetricsForDpi(index: i32, dpi: u32) -> i32;
            fn GetSystemMetrics(index: i32) -> i32;
        }
        let dpi = GetDpiForWindow(hwnd);
        if dpi >= 96 {
            let w = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            if w > 0 {
                return w;
            }
        }
        // 兜底：理论上 Win10 1607+ 都有 GetSystemMetricsForDpi，这里只是防它返回 0
        let w = GetSystemMetrics(SM_CXSIZEFRAME) + GetSystemMetrics(SM_CXPADDEDBORDER);
        if w > 0 {
            w
        } else {
            8
        }
    }
}

/// 按缩放方向位掩码换成系统的缩放光标。位掩码见 `edge_from_cursor`。
///
/// 需要自己设光标的原因：`TAURI_DRAG_RESIZE_BORDERS` 子窗口盖在 WebView 之上，
/// 而我们让它 `WM_NCHITTEST` 一律返回 `HTCLIENT`，它就不再是「非客户区」，
/// 系统不会给缩放光标。那 4px 里光标一直是箭头，看起来像「这里不能拉」。
fn set_resize_cursor(edge: u32) {
    // LoadCursorW 接受 MAKEINTRESOURCE，即把序号当指针传
    const IDC_SIZEWE: usize = 32644;
    const IDC_SIZENS: usize = 32645;
    const IDC_SIZENWSE: usize = 32642;
    const IDC_SIZENESW: usize = 32643;
    // 注意：match 里的 `|` 是「或模式」，不是位或，所以组合值要写成字面量
    let id = match edge {
        1 | 2 => IDC_SIZENS,   // 上 / 下
        4 | 8 => IDC_SIZEWE,   // 左 / 右
        5 | 10 => IDC_SIZENWSE, // 左上(1|4) / 右下(2|8)：↖↘
        6 | 9 => IDC_SIZENESW,  // 左下(2|4) / 右上(1|8)：↙↗
        _ => return,
    };
    unsafe {
        extern "system" {
            fn LoadCursorW(inst: isize, name: *const u16) -> isize;
            fn SetCursor(cursor: isize) -> isize;
        }
        let h = LoadCursorW(0, id as *const u16);
        if h != 0 {
            SetCursor(h);
        }
    }
}

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
        let t = resize_edge_tolerance(main);
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
        // `as *const ()` 中转一下再变整数：Rust 2021 起不允许函数项直接 cast 成整数
        let prev = SetWindowLongPtrW(found, GWLP_WNDPROC_BORDER, border_proc as *const () as isize);
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
    // 光标：`WM_NCHITTEST` 一律返回 HTCLIENT 之后，这一圈在系统眼里就是客户区，
    // 不会自动变成缩放箭头。最外这 4px 是用户「贴到边框」时最先碰到的位置，
    // 那里光标不变，看起来就像「还没进入可缩放区」，只能再往里挪 —— 这正是
    // 「必须靠边框里面很多才触发」的直接来源。
    const WM_SETCURSOR: u32 = 0x0020;
    if msg == WM_SETCURSOR {
        let main = unsafe { GetAncestor(hwnd, 2) }; // GA_ROOT
        let target = if main != 0 { main } else { hwnd };
        let edge = edge_from_cursor(target);
        if edge != 0 {
            set_resize_cursor(edge);
            return 1; // TRUE：光标已设，别让系统再改回箭头
        }
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

/// 菜单窗口的 NC 边框拦截 —— 只拦 WM_NCACTIVATE，其余全部透传。
///
/// 为什么要单独一份：`install_focus_hook` 的原 proc 存在**单个全局**里，
/// 再装第二个窗口会把它覆盖掉，主窗口的消息链就断了。菜单窗口只有一条
/// 需求（失焦瞬间 Windows 会重绘非客户区，Win7 式边框闪一下，用户截图实锤），
/// 所以这里用**独立的一对静态**，只拦那一条消息，互不影响。
///
/// 用 hwnd 做防重装护栏：菜单窗口是复用的，每次 open_menu 都调用这里，
/// 同一个 hwnd 只装一次；窗口被重建（hwnd 变了）则自动换到新窗口上。
static MENU_HOOKED_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static MENU_ORIG_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn install_menu_nc_hook(hwnd: isize) {
    use std::sync::atomic::Ordering;
    if MENU_HOOKED_HWND.load(Ordering::Relaxed) == hwnd {
        return;
    }
    unsafe {
        extern "system" {
            fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
            fn CallWindowProcW(prev: isize, hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
            fn DefWindowProcW(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
        }
        const GWLP_WNDPROC: i32 = -4;
        unsafe extern "system" fn menu_proc(
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> isize {
            if msg == WM_NCACTIVATE {
                // lparam = -1 → 禁止重绘非客户区（与主窗口同一手法）
                return unsafe {
                    DefWindowProcW(hwnd, msg, wparam, -1)
                };
            }
            unsafe {
                CallWindowProcW(
                    MENU_ORIG_WNDPROC.load(std::sync::atomic::Ordering::Relaxed),
                    hwnd,
                    msg,
                    wparam,
                    lparam,
                )
            }
        }
        let orig =
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, menu_proc as *const () as isize);
        if orig == 0 {
            crate::log_warn!("[effects] 菜单 NC hook 挂载失败");
            return;
        }
        MENU_ORIG_WNDPROC.store(orig, Ordering::Relaxed);
        MENU_HOOKED_HWND.store(hwnd, Ordering::Relaxed);
        crate::log_print!("[effects] 菜单 NC hook 已挂载（WM_NCACTIVATE → 不重绘）");
    }
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
        /// 客户区重算。`wparam != 0` 时返回值就是客户区相对窗口的位置与大小。
        const WM_NCCALCSIZE: u32 = 0x0083;

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
        // 无边框的**最终保险**：无论样式位怎么变（主题切换、后台释放后窗口重建、
        // tao 的 `set_window_flags` 把 WS_CAPTION 加回来、DWM 重算非客户区），
        // 只要这里把非客户区算成 0，原生标题栏与那圈系统边框就永远画不出来。
        //
        // 这是 Windows 无边框窗口的标准做法。tao 内部本来也这么做，但它的判断
        // 会被上面那些路径打乱；接管这一层之后就不再依赖它的状态了。
        // `wparam == 0` 的那种调用要给系统算默认值（客户区不变），不能一律返回 0。
        if msg == WM_NCCALCSIZE && wparam != 0 {
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
        let ret = unsafe { CallWindowProcW(orig, hwnd, msg, wparam, lparam) };
        // tao 的 ThemeChanged 监听在本机日志里**从未出现**（无「系统主题切换 →」）。
        // WM_SETTINGCHANGE 是系统主题变化的权威消息：先交给 tao，再在本钩子里
        // 主线程补一次材质（与焦点路径同等的完整下发 + FRAMECHANGED）。
        // 仅 system 档；手动钉住时不动，避免把材质改成系统那套。
        const WM_SETTINGCHANGE: u32 = 0x001A;
        if msg == WM_SETTINGCHANGE && crate::settings::get().theme_mode == "system" {
            // 只认配色广播：lParam 常为 "ImmersiveColorSet"（UTF-16）。
            let mut color_scheme = false;
            if lparam != 0 {
                unsafe {
                    let mut p = lparam as *const u16;
                    let mut buf = String::new();
                    for _ in 0..64 {
                        let c = *p;
                        if c == 0 {
                            break;
                        }
                        buf.push(char::from_u32(c as u32).unwrap_or('?'));
                        p = p.add(1);
                    }
                    color_scheme =
                        buf.contains("ImmersiveColorSet") || buf.contains("WindowsThemeElement");
                }
            }
            if color_scheme {
                if let Some(app) = HOOK_APP.get() {
                                    if let Some(w) = app.get_webview_window(crate::idle_release::MAIN_LABEL) {
                        crate::log_print!("[effects] WM_SETTINGCHANGE → 系统主题静默重下");
                        // 与焦点路径一致：立即 + 150ms 再补；不做翻转/FRAMECHANGED，不闪。
                        schedule_system_theme_refresh(w, "setting-change");
                        note_theme_event_handled();
                        let _ = crate::tray::sync_menu_appearance(app);
                    }
                }
            }
        }
        ret
    }

    let orig = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook_proc as *const () as isize) };
    if orig == 0 {
        crate::log_warn!("[effects] WndProc 子类挂载失败（SetWindowLongPtrW 返回 0）");
        return;
    }
    ORIG_WNDPROC.store(orig, std::sync::atomic::Ordering::Relaxed);
    crate::log_print!("[effects] 焦点 hook 已挂载（WM_NCACTIVATE → 不重绘 NC 区）");
}

// ---------------------------------------------------------------------------
// 弹出窗口（快捷面板 / 托盘菜单）的拖动
//
// 它们不能复用上面那套：`ORIG_WNDPROC` 是**单槽**的，给第二个窗口挂上去会把主窗口
// 的转发链覆盖掉（主窗口的消息会被转到面板的 WndProc，直接崩）。所以单独一套。
//
// 为什么必须自定义拖动而不能用 `startDragging()`：面板是 `transparent` + 带材质的
// 分层窗口，系统移动循环会让 DWM 逐帧重算模糊，实测拖动时身后拖出一串重影
// （用户反馈「拖拽重影很严重」）。主窗口早年为同一个问题改用自定义拖动，
// 这里复用同一套：SetCapture + 按绝对光标位移 SetWindowPos + 16ms 节流。
// ---------------------------------------------------------------------------

/// 已挂 hook 的弹出窗口句柄（窗口重建后句柄会变，靠它判断要不要重挂）
static POPUP_HOOKED: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
/// 每个弹窗自己的「原窗口过程」，按句柄存。
///
/// 原先只有一个槽：面板和托盘菜单**都**会挂钩子，后挂的把前一个的原过程覆盖掉，
/// 于是先挂的那个窗口收到的消息会被送到另一个窗口的 WndProc 里处理 ——
/// 表现可能是「某个弹窗的边框/命中测试突然不对」这类说不清的问题。
static POPUP_ORIG: std::sync::Mutex<Vec<(isize, isize)>> = std::sync::Mutex::new(Vec::new());

fn popup_orig_of(hwnd: isize) -> isize {
    POPUP_ORIG
        .lock()
        .ok()
        .and_then(|g| g.iter().rev().find(|(h, _)| *h == hwnd).map(|(_, o)| *o))
        .unwrap_or(0)
}

fn set_popup_orig(hwnd: isize, orig: isize) {
    if let Ok(mut g) = POPUP_ORIG.lock() {
        g.retain(|(h, _)| *h != hwnd);
        g.push((hwnd, orig));
    }
}
/// `GWL_WNDPROC`：窗口过程的存放槽位（-4）。主窗口那份是写在函数内部的局部常量，
/// 这里要用在模块作用域，单独定义一份。
const GWLP_WNDPROC: i32 = -4;

extern "system" fn popup_hook_proc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    unsafe extern "system" {
        fn CallWindowProcW(
            prev: isize,
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> isize;
    }
    unsafe extern "system" {
        fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }
    const WM_NCCALCSIZE: u32 = 0x0083;
    const WM_NCACTIVATE: u32 = 0x0086;
    const WM_NCHITTEST: u32 = 0x0084;
    const HTCLIENT: isize = 1;

    if msg == WM_NCCALCSIZE && wparam != 0 {
        return 0;
    }
    // **聚焦时系统会发 WM_NCACTIVATE 重绘非客户区** —— 面板本身是
    // decorations(false)，但一旦聚焦，系统照样照着样式位画出最小化/最大化/关闭，
    // 失焦又擦掉（用户截图：「聚焦时出现，失焦就消失」）。
    // `lparam = -1` 表示「不要重绘非客户区」，和主窗口那条钩子完全一样。
    if msg == WM_NCACTIVATE {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, -1) };
    }
    if msg == WM_NCHITTEST {
        return HTCLIENT;
    }
    if handle_drag_msg(hwnd, msg) {
        return 0;
    }
    let orig = popup_orig_of(hwnd);
    if orig == 0 {
        return 0;
    }
    unsafe { CallWindowProcW(orig, hwnd, msg, wparam, lparam) }
}

/// 给弹出窗口挂上拖动用的 WndProc（幂等：同一个句柄只挂一次）。
pub fn install_popup_hook(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    // 幂等必须**按句柄查表**：原来用单槽 `POPUP_HOOKED == hwnd` 判断，
    // 面板和托盘菜单互相覆盖时会导致同一个窗口被重复挂——
    // 第二次挂会把 orig 设成我们自己的 proc，消息转发直接成死循环。
    let already = POPUP_ORIG
        .lock()
        .map(|g| g.iter().any(|(h, _)| *h == hwnd))
        .unwrap_or(false);
    if already {
        return;
    }
    unsafe {
        extern "system" {
            fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        }
        let orig =
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, popup_hook_proc as *const () as isize);
        if orig == 0 {
            crate::log_warn!("[effects] 弹出窗口 hook 挂载失败");
            return;
        }
        set_popup_orig(hwnd, orig);
        POPUP_HOOKED.store(hwnd, std::sync::atomic::Ordering::Relaxed);
    }
    crate::log_print!("[effects] 弹出窗口拖动 hook 已挂载");
}

// ---------------------------------------------------------------------------
// 浮层（快捷面板）材质：保留系统材质，同时把窗口剪成圆角
// ---------------------------------------------------------------------------

/// 施加浮层材质，并把窗口剪成圆角。
///
/// 为什么需要 region：系统材质恒为**矩形**铺满整个窗口（见 `clear_system_material`
/// 的注释），CSS 的 `border-radius` 只裁得到自己那层底色，裁不掉它 ——
/// 四角会露出矩形的材质，看起来就是「直角」（用户实测）。
/// `SetWindowRgn` 剪的是**窗口形状**本身，DWM 画 backdrop 时会跟着被裁掉，
/// 材质与圆角才可能同时成立。
pub fn apply_popup_material(hwnd: isize, dark: bool) -> bool {
    // 关了高级材质就不施加，与主窗口的语义保持一致
    if !is_round() {
        return false;
    }
    // **必须先把窗口标成「深色」**：Win11 的系统 backdrop 颜色由 DWM 按窗口的
    // 沉浸式深色属性决定，不设的话深色主题下拿到的仍是浅色 backdrop ——
    // 表现为「深色模式下面板还是白的」，CSS 层怎么调都救不回来
    // （主窗口一直有这一步，面板从没设过，这就是深色面板发白的根因）。
    set_immersive_dark(hwnd, dark);
    // 面板窗口同样要剥样式位：系统主题变化时 tao 会重走 `set_window_flags`，
    // 把 WS_CAPTION 加回来 —— 表现就是「面板右上角冒出最小化/最大化/关闭」
    // （用户截图）。主窗口一直有这一步，面板缺了。
    strip_window_frame(hwnd, false);
    let tier = current_tier();
    // 与主窗口同一条规则：backdrop 是画在**非客户区**上的，这一档不能关 NC 渲染
    // （关了材质就没了）；只把 1px 边框设成透明。其它档继续用关 NC 消边框。
    if tier == Tier::SystemBackdrop {
        clear_window_border(hwnd);
    } else {
        disable_nc_rendering(hwnd);
    }
    let ok = match tier {
        Tier::SystemBackdrop => apply_system_backdrop(hwnd).is_ok(),
        Tier::Mica => apply_mica(hwnd).is_ok(),
        Tier::Acrylic => apply_acrylic(hwnd, dark).is_ok(),
        Tier::Solid => false,
    };
    ok
}

/// 显示前重施一次材质（兜底：首次显示、切主题）。
///
/// 面板改成「移屏幕外驻留」而不是 `hide`，材质在驻留期间不会丢，
/// 所以这里不需要「清成 NONE 再设回」那套 —— 直接施加 + 逼 DWM 重画即可。
pub fn rebuild_popup_material(hwnd: isize, dark: bool) -> bool {
    if !is_round() {
        crate::log_print!("[effects] 浮层材质跳过：高级材质未开启（is_round=false）");
        return false;
    }
    // 主题切换后 DWM 会把浮层这层表面也丢掉，此时**再设同一个 backdrop 值会被跳过**，
    // 面板就成了实底。这里先设一次 NONE（真实变化）再设回目标值 —— 两次写紧挨着、
    // 中间不 flush，DWM 会合并到同一次合成里，既强制重建又不上屏中间态。
    if current_tier() == Tier::SystemBackdrop {
        unsafe {
            extern "system" {
                fn DwmSetWindowAttribute(
                    hwnd: isize,
                    attr: u32,
                    pv: *const c_void,
                    size: u32,
                ) -> i32;
            }
            let none: u32 = DWMSBT_NONE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &none as *const _ as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
    let ok = apply_popup_material(hwnd, dark);
    // ⚠ tint 必须在 backdrop 重建**之后**再钉一次：
    // `DWMWA_SYSTEMBACKDROP_TYPE` 的 NONE→目标 变化会让 DWM 重新解析这层表面，
    // 过程中 `DWMWA_USE_IMMERSIVE_DARK_MODE` 会被按窗口当前状态重置回旧值，
    // 于是出现「文字变了、面板还是深色底」。
    set_immersive_dark(hwnd, dark);
    nudge(hwnd);
    force_redraw(hwnd);
    crate::log_print!(
        "[effects] 浮层材质施加 成功={ok} tier={}",
        current_tier().as_str()
    );
    ok
}

// ---------------------------------------------------------------------------
// 材质看门狗
// ---------------------------------------------------------------------------

/// 检查窗口的「材质骨架」是否还完整：
///   ① 样式位里不该有 `WS_CAPTION`（有 = tao 把剥掉的边框加回来了）；
///   ② Win11 的 system backdrop 类型应该还是瞬态窗口（3）。
///
/// 只读、不写。读不到（hr < 0）时按「正常」处理 —— 保守，避免误判造成周期性闪。
fn material_is_intact(hwnd: isize) -> bool {
    unsafe {
        extern "system" {
            fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
            fn DwmGetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *mut c_void,
                size: u32,
            ) -> i32;
        }
        const GWL_STYLE: i32 = -16;
        const WS_CAPTION: i32 = 0x00C0_0000;
        if GetWindowLongW(hwnd, GWL_STYLE) & WS_CAPTION != 0 {
            return false;
        }
        // 高级材质关 → 本来就不该有系统材质（backdrop 被主动清成 NONE）。
        // 这时再按"必须有 backdrop"去判，会每 800ms 误判一次、无脑复位，
        // 日志里就是一串「看门狗：样式位/backdrop 异常」（用户实测刷屏）。
        if !is_round() {
            return true;
        }
        match current_tier() {
            Tier::SystemBackdrop => {
                let mut v: u32 = 0;
                let hr = DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &mut v as *mut u32 as *mut c_void,
                    std::mem::size_of::<u32>() as u32,
                );
                hr < 0 || v == DWMSBT_TRANSIENTWINDOW
            }
            Tier::Mica => {
                let mut v: u32 = 0;
                let hr = DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_MICA_EFFECT,
                    &mut v as *mut u32 as *mut c_void,
                    std::mem::size_of::<u32>() as u32,
                );
                hr < 0 || v == 1
            }
            // acrylic 档的 accent state 读取要另走 GetWindowCompositionAttribute，
            // 判据不稳定，这里只查样式位（上面那条）。
            _ => true,
        }
    }
}

/// 材质看门狗：400ms 只读巡检，发现异常才复位一次。
///
/// 为什么需要定时检查，而不是靠事件：
/// 我们已经在这些时机挂了补涂 —— 启动、最小化恢复、主题切换、焦点变化。
/// 但实测仍有漏网：**启动早期**（DWM 还没铺开）、**系统主题切换**（tao 自己重走
/// set_window_flags 把 WS_CAPTION 加回来、backdrop 被丢）都可能没有事件可等，
/// 表现就是「没材质，点一下窗口 / 切一次深浅色才回来」。
/// Win11 24H2+ 在最小化恢复时甚至不派发任何窗口消息（社区已确认），
/// 事件驱动天生补不全。
///
/// 同类工具（ExplorerBlurMica）的解法就是**定时幂等重刷**：我们做成只读巡检，
/// 正常时零写入（不闪、不耗 GPU），只有连续两拍（≈800ms）都判异常才复位。
pub fn spawn_material_watch(app: tauri::AppHandle) {
    // 低占用释放会销毁并重建主窗口，而重建又会走一遍 `setup_effects` ——
    // 以前每重建一次就多一个看门狗线程，几个线程各刷各的材质、各发一次
    // 「系统深浅变化」，日志里同一次切换出现 2–3 条。只允许一个。
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let mut bad = 0u8;
        // 上一次看到的系统深浅。只用于「发现变化」：一变就按手动切的那条路
        // 重刷一遍材质（见下面的调用点），不再做任何"是否已经跟上"的判断。
        let mut last_dark = is_dark_now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let Some(w) = app.get_webview_window(crate::idle_release::MAIN_LABEL) else {
                continue;
            };

            // ① 系统深浅变化：CSS/托盘/面板照常；材质仅在 ThemeChanged 未排队时兜底。
            let now_dark = is_dark_now();
            if now_dark != last_dark {
                last_dark = now_dark;
                crate::log_print!("[theme] 系统深浅变化 → dark={now_dark}，全部窗口重刷");
                use tauri::Emitter;
                let _ = w.emit("system-theme-changed", now_dark);
                #[cfg(windows)]
                if let Some(pw) = app.get_webview_window(crate::popup::PANEL_LABEL) {
                    if let Ok(ph) = pw.hwnd() {
                        let _ = rebuild_popup_material(ph.0 as isize, now_dark);
                    }
                    let _ = app.emit_to(crate::popup::PANEL_LABEL, "panel-theme", now_dark);
                }
                crate::tray::sync_menu_appearance(&app);
                // 窗口在前台 → 用户正看着它，直接完整复位（此刻刷新是他期待的）；
                // 在后台 → 挂起，等他回到前台时由 `flush_pending_theme_rebuild` 补。
                // ⚠ 不能用 `w.is_focused()`：WebView 窗口在"被盖住但仍是焦点窗口"
                // 等状态下返回值不可靠，实测失焦时报 true，于是走了前台分支却没效果。
                // 用真实前台窗口判断。
                let focused = {
                    extern "system" {
                        fn GetForegroundWindow() -> isize;
                    }
                    let hwnd = w.hwnd().map(|h| h.0 as isize).unwrap_or(0);
                    hwnd != 0 && unsafe { GetForegroundWindow() } == hwnd
                };
                if focused {
                    refresh_theme_full(&w, "theme-focused");
                    clear_pending_theme_rebuild();
                } else {
                    mark_pending_theme_rebuild();
                }
                // ThemeChanged 已排队 → 跳过材质，避免与 setting-change 双刷。
                if !theme_event_handled(now_dark) {
                    schedule_system_theme_refresh(w.clone(), "watchdog-theme");
                }
                bad = 0;
                continue;
            }

            // ② 窗口不可见 / 最小化 / 正驻留屏幕外：这一拍什么都不写。
            //    驻留期间材质是「已清空」状态，写回去会毁掉唤回时的真实值变化。
            if any_suspended() || !w.is_visible().unwrap_or(false) {
                bad = 0;
                continue;
            }
            let Ok(h) = w.hwnd() else { continue };
            unsafe {
                extern "system" {
                    fn IsIconic(hwnd: isize) -> i32;
                }
                if IsIconic(h.0 as isize) != 0 {
                    bad = 0;
                    continue;
                }
            }

            // ④ 属性骨架被破坏（WS_CAPTION 回来了 / backdrop 被清成 NONE）→
            //    连续两拍（≈800ms）都异常才复位，避免误判造成周期性闪。
            //
            if material_is_intact(h.0 as isize) {
                bad = 0;
                continue;
            }
            bad = bad.saturating_add(1);
            if bad < 2 {
                continue;
            }
            bad = 0;
            crate::log_print!("[effects] 看门狗：样式位/backdrop 异常，执行一次复位");
            // 看门狗异常复位：静默重下（禁止 restore，会闪）。
            refresh_theme_colors(&w, "watchdog");
        }
    });
}
