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

use std::sync::atomic::{AtomicIsize, Ordering};
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
}

const SRCCOPY: u32 = 0x00CC_0020;
const DIB_RGB_COLORS: u32 = 0;

const WH_KEYBOARD_LL: i32 = 13;
const HC_ACTION: i32 = 0;
const WM_KEYDOWN: usize = 0x0100;
const WM_SYSKEYDOWN: usize = 0x0104;
const VK_ESCAPE: u32 = 0x1B;

/// 让热键线程装上 Esc 兜底钩子（`WM_APP + 4`）
pub const WM_SHOT_GUARD_ON: u32 = 0x8000 + 4;
/// 卸掉 Esc 兜底钩子（`WM_APP + 5`）
pub const WM_SHOT_GUARD_OFF: u32 = 0x8000 + 5;

type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;

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
    /// 编码后的 PNG data URL。**同一张图只编一次**：1080p 的 PNG 压缩要几十毫秒，
    /// 而前端为了防「事件早于页面加载」会主动拉一次图，可能连拉两次。
    png: Option<String>,
}

static LAST: Mutex<Option<Shot>> = Mutex::new(None);

/// `Alt+Q` 的入口：抓屏 → 铺满鼠标所在显示器 → 通知前端画图。
pub fn start(app: &AppHandle) {
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

    if let Ok(mut g) = LAST.lock() {
        *g = Some(Shot {
            w: w as u32,
            h: h as u32,
            left,
            top,
            pixels,
            png: None,
        });
    }

    let scale = mon.scale_factor().max(0.1);
    let app2 = app.clone();
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
    let app3 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let pending = LAST.lock().map(|g| g.is_some()).unwrap_or(false);
        if !pending {
            return; // 已经 commit / cancel 过了
        }
        let visible = app3
            .get_webview_window(SHOT_LABEL)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if !visible {
            crate::log_warn!("[shot] 前端 1.2 秒内未就绪，取消本次截图（窗口保持隐藏）");
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
    let win = match app.get_webview_window(SHOT_LABEL) {
        Some(w) => w,
        None => {
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
                .focused(true);
            if let Some(args) = crate::popup::browser_args(app) {
                b = b.additional_browser_args(&args);
            }
            match b.build() {
                Ok(w) => w,
                Err(e) => {
                    crate::log_err!("screenshot", "创建截图覆盖层失败：{e}");
                    return;
                }
            }
        }
    };

    fit_client_to_screen(&win, left, top, w, h);
    // 1 CSS px = 1 物理 px，前端算出的选区坐标就等同于像素坐标
    let _ = win.set_zoom(1.0 / scale);
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

/// 真正把覆盖层亮出来并抢焦点。由前端在截图解码完成后调用（见 `shot_show`）。
fn reveal(app: &AppHandle) {
    let Some(win) = app.get_webview_window(SHOT_LABEL) else {
        return;
    };
    let _ = win.show();
    let _ = win.set_focus();
    if let Ok(h) = win.hwnd() {
        // 后台进程直接 SetForegroundWindow 会被系统拒绝，必须走 AttachThreadInput
        unsafe { crate::panel::force_foreground(h.0 as isize) };
    }
}

fn guard_on() {
    crate::hotkey::post_msg(WM_SHOT_GUARD_ON);
}

fn guard_off() {
    crate::hotkey::post_msg(WM_SHOT_GUARD_OFF);
}

/// 装上 **Esc 兜底**钩子：截图期间无论前端是否正常，按 Esc 都能退出。
///
/// 为什么必须有这一条：取消逻辑本来完全在前端（`keydown` → `shot_cancel`）。
/// 于是「页面没加载成功」时，用户面对的是一个全屏、置顶、点了没反应的窗口 ——
/// 这不是假设，是实测踩到的事故。逃生通道不能依赖那条最可能出问题的链路。
///
/// 与 `screenclip` 的鼠标钩子同源（都装在热键线程的消息泵上），但判据不同：
/// 那个拦截鼠标右键、只在系统截图覆盖层出现时生效；这个只认 Esc、只在本次截图期间存在。
pub fn guard_install() {
    if GUARD.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, key_proc, GetModuleHandleW(std::ptr::null()), 0);
        if h == 0 {
            crate::log_warn!("[shot] Esc 兜底钩子安装失败（错误码 {}）", GetLastError());
            return;
        }
        GUARD.store(h, Ordering::SeqCst);
    }
    crate::log_print!("[shot] Esc 兜底已启用");
}

pub fn guard_uninstall() {
    let h = GUARD.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            UnhookWindowsHookEx(h);
        }
        crate::log_print!("[shot] Esc 兜底已卸载");
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
                    close(&app);
                });
            }
            // 吞掉这次 Esc：别让它同时落到下层窗口（比如编辑器里退出全屏）
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
fn png_data_url(shot: &Shot) -> Option<String> {
    let mut rgba = Vec::with_capacity(shot.pixels.len());
    for px in shot.pixels.chunks_exact(4) {
        // BGRA → RGBA
        rgba.push(px[2]);
        rgba.push(px[1]);
        rgba.push(px[0]);
        rgba.push(255);
    }
    let img = image::RgbaImage::from_raw(shot.w, shot.h, rgba)?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        crate::clipboard::base64_encode(out.get_ref())
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
    dib.extend_from_slice(&size_image.to_le_bytes()); // biSizeImage    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
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
#[tauri::command]
pub fn shot_image() -> Result<String, String> {
    let mut g = LAST.lock().map_err(|_| "截图状态锁失效")?;
    let shot = g.as_mut().ok_or("没有待处理的截图")?;
    if let Some(p) = shot.png.clone() {
        crate::log_print!("[shot] 前端取图（命中缓存，{} KB）", p.len() / 1024);
        return Ok(p);
    }
    let t0 = std::time::Instant::now();
    let Some(encoded) = png_data_url(shot) else {
        crate::log_warn!(
            "[shot] PNG 编码失败（{}×{}，原始 {} 字节）",
            shot.w,
            shot.h,
            shot.pixels.len()
        );
        return Err("截图编码失败".into());
    };
    crate::log_print!(
        "[shot] 前端取图（编码 {} KB，耗时 {}ms）",
        encoded.len() / 1024,
        t0.elapsed().as_millis()
    );
    shot.png = Some(encoded.clone());
    Ok(encoded)
}

/// 前端把截图**解码完成**后调用，这时才显示窗口。
///
/// 顺序反了会闪：先 show 再加载图，用户按下快捷键的第一眼是白底或黑底，
/// 第二眼才变成桌面 —— 看起来像卡了一下。
#[tauri::command]
pub fn shot_show(app: AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || reveal(&app2));
}

/// 双击 / `Enter`：裁图 → 写剪贴板（会被采集线程记成一条图片）→ 收起覆盖层。
#[tauri::command]
pub fn shot_commit(app: AppHandle, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    // 顺带把「抓的是哪块屏」记进日志：多屏下选区坐标看着正常但内容错位时，
    // 第一个要排除的就是「抓屏原点不对」
    let (dib, left, top) = {
        let g = LAST.lock().map_err(|_| "截图状态锁失效")?;
        let shot = g.as_ref().ok_or("没有待处理的截图")?;
        (crop_to_dib(shot, x, y, w, h)?, shot.left, shot.top)
    };
    crate::clipboard::write_shot_to_clipboard(&dib)?;
    crate::log_print!("[shot] 已复制选区 {w}×{h}（屏幕原点 {left},{top}）");
    close(&app);
    Ok(())
}

/// 右键 / `Esc` / 点空白：直接收起，**不碰剪贴板**。
///
/// 不碰剪贴板是硬要求：用户可能正拿剪贴板里的东西在干别的，截个图再取消不该把它冲掉。
#[tauri::command]
pub fn shot_cancel(app: AppHandle) {
    crate::log_print!("[shot] 已取消（剪贴板未改动）");
    close(&app);
}

fn close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SHOT_LABEL) {
        let _ = w.hide();
    }
    guard_off();
    // 抓屏缓冲有几 MB（1080p 约 8MB），不用了立刻放掉
    if let Ok(mut g) = LAST.lock() {
        *g = None;
    }
}
