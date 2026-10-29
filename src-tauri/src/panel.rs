//! 快捷面板：快捷键直接弹出「贴光标的小列表」，回车把内容粘回刚才那个窗口。
//!
//! 这是剪贴板工具的核心体验 —— 不用切窗口、不用点两下。两个关键点：
//!
//! 1. **唤出前的前台窗口必须记下来**。面板一显示，前台就变成面板自己了；
//!    之后要把焦点还回去，只能靠 `GetForegroundWindow()` 在显示**之前**取一次。
//! 2. **粘贴要确认焦点真的回去了**。`SetForegroundWindow` 不保证成功（系统会拦
//!    后台进程抢焦点），如果没抢到就发 Ctrl+V，那是把内容粘进面板自己 —— 所以
//!    发按键前先比对一次前台窗口，没回去就放弃并记日志。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

/// 窗口 label 由 `popup` 统一定义（它要按 label 决定失焦策略），这里不另立一份。
use crate::popup::PANEL_LABEL;
use crate::effects::Rect;

const PANEL_W: f64 = 620.0;
const PANEL_H: f64 = 460.0;
/// 面板与鼠标之间留一点距离，别让光标正好压在第一行上
const GAP: f64 = 12.0;

/// 唤出面板之前的前台窗口。0 = 未知（例如从托盘打开面板）。
static PREV_FG: AtomicIsize = AtomicIsize::new(0);

extern "system" {
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn AttachThreadInput(attach: u32, to: u32, on: i32) -> i32;
    fn BringWindowToTop(hwnd: isize) -> i32;
    fn SetFocus(hwnd: isize) -> isize;
    fn GetCurrentThreadId() -> u32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    // `GetCursorPos` 要一个 POINT，这里用 `effects::Rect` 接：两个结构的前两个字段
    // 都是 x/y（i32），系统只写前 8 字节。自己再立一个 `Point` 会和 effects 里那份
    // 声明打架（同名 extern 签名不同 → clashing 警告），不如共用同一个物理坐标类型。
    fn GetCursorPos(pt: *mut Rect) -> i32;
}

const VK_CONTROL: u16 = 0x11;
const VK_V: u16 = 0x56;
/// Alt。注入它只是为了换取前台切换权限，不是要真的按 Alt（见 `force_foreground`）
const VK_MENU: u16 = 0x12;

/// 唤出面板。已有窗口就复用（重建 WebView 要多几百毫秒，弹出会很迟钝）。
pub fn show(app: &AppHandle) {
    // 必须在显示之前取：面板一显示，前台就变成它自己了
    unsafe {
        let fg = GetForegroundWindow();
        let self_hwnd = app
            .get_webview_window(PANEL_LABEL)
            .and_then(|w| w.hwnd().ok())
            .map(|h| h.0 as isize)
            .unwrap_or(0);
        if fg != 0 && fg != self_hwnd {
            PREV_FG.store(fg, Ordering::SeqCst);
        }
    }

    // 后续动作必须在**主线程**做：窗口是主线程创建的，抢焦点要用
    // `AttachThreadInput` 挂到当前前台线程上，线程不对就白挂。
    let app2 = app.clone();
    if let Err(e) = app.run_on_main_thread(move || show_now(&app2)) {
        crate::log_warn!("[panel] 切到主线程失败：{e}");
    }
}

/// 快捷键被按下时的动作：已在前台就收起，否则（重新）唤出。
///
/// 为什么不是无脑 `show`：把「面板失焦保持打开」打开之后，用户去别的窗口干完事
/// 回来按快捷键，如果只会 `show`，面板就一直赖着不走了 —— 那时唯一的下车方式只有 Esc。
///
/// 判据是「可见 **且** 在前台」才收起。只可见不算：面板可能被用户丢在一边、前台换了
/// 别的窗口，这时按快捷键用户想的是「把面板叫回来」，收起来最反直觉。
pub fn toggle(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(PANEL_LABEL) {
        let visible = win.is_visible().unwrap_or(false);
        let hwnd = win.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        if visible && hwnd != 0 && unsafe { GetForegroundWindow() } == hwnd {
            crate::log_print!("[panel] 快捷键再次按下，收起面板");
            let _ = win.hide();
            return;
        }
    }
    show(app);
}

fn show_now(app: &AppHandle) {
    let Some(win) = crate::popup::ensure(app, PANEL_LABEL, "panel.html", "轻贴", PANEL_W, PANEL_H)
    else {
        return;
    };

    // 跟随鼠标：这是「光标附近弹出」最实用的近似。真正的插入符位置要靠
    // `GetCaretPos`，但它只在同进程窗口里可靠，跨进程拿到的经常是 0。
    let mut pt = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let anchor = unsafe {
        if GetCursorPos(&mut pt) == 0 {
            PhysicalPosition::new(200.0, 200.0)
        } else {
            PhysicalPosition::new(pt.left as f64, pt.top as f64)
        }
    };

    let (x, y) = crate::popup::place(app, PANEL_W, PANEL_H, anchor, false, GAP);
    let _ = win.set_position(PhysicalPosition::new(x, y));

    let _ = win.show();
    crate::popup::mark_shown(PANEL_LABEL);
    let _ = win.set_focus();
    // 再显式抢一次前台。后台进程直接 `SetForegroundWindow` 常被系统拒绝
    // （实测：焦点会落到同进程的主窗口上，面板刚弹出就因为失焦被收起）。
    if let Ok(h) = win.hwnd() {
        if !unsafe { force_foreground(h.0 as isize) } {
            crate::log_warn!("[panel] 焦点没抢到，面板可能刚弹出就被失焦收起");
        }
    }

    // 每次弹出都让页面重新拉一次数据：面板是常驻的，不刷新就会看到旧列表，
    // 而「刚复制的那条在最上面」正是用户最先要的。
    // 顺带把面板需要的设置推下去：面板是常驻窗口，改完设置要靠这次推送才生效；
    // 单独为此发一次 IPC 会让弹出路径多一个来回，不值得。
    let prefs = serde_json::json!({
        "closeKey": crate::settings::get().panel_close_key,
    });
    let _ = app.emit_to(PANEL_LABEL, "panel-open", prefs);
}

/// 把窗口强行拉到前台，**返回是否真的抢到了**。
///
/// 非前台进程调用 `SetForegroundWindow` 会被系统拦（防止弹窗骚扰），绕法是先
/// `AttachThreadInput` 把自己的输入队列挂到**当前前台线程**上，此时系统认为两者
/// 是同一个输入上下文，设置才能生效；做完立刻解挂，否则两边键盘状态会互相影响。
///
/// **调用线程必须是主线程**：窗口是主线程创建的，Attach 挂的也是线程输入队列，
/// 从别的线程调用等于挂到一个跟窗口无关的队列上 —— 白挂。面板与截图两处都按这条
/// 走（截图那边用 `run_on_main_thread` 转一次）。
///
/// `pub(crate)`：截图覆盖层也要抢一次前台（否则第一次点击会被系统拿去激活窗口，
/// 表现为「必须点一下才能开始框选」），逻辑完全一样，
/// 不重复实现 —— 这个函数里每一步都是踩坑换来的，复制一份必然漂移。
pub(crate) unsafe fn force_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    // 必须重试，而且每轮重新取前台窗口，原因有两个：
    // 1. `show()` 与前台切换都是**异步**的 —— 刚 show 完的窗口还没成为可激活的
    //    顶层窗口，第一次调用多半无效；
    // 2. Attach 的对象（当前前台窗口）每一轮都可能变，拿上一轮的结果去挂会挂空。
    //
    // 40ms × 4 轮 ≈ 160ms，用户察觉不到，但足够覆盖「窗口刚显示」这段时间。
    for round in 0..4 {
        let fg = GetForegroundWindow();
        if fg == hwnd {
            return true;
        }
        // 前两轮都失败，说明进程不满足 Windows 放行前台切换的那几条
        // （「调用进程收到最后一个输入事件」「前台锁超时已过」等）—— 只靠
        // AttachThreadInput 也救不回来。这时模拟一次 **Alt 按下抬起**：本进程因此
        // 成为最后输入接收者，系统才会放行。tao 自己在窗口创建时用的就是这招
        // （`force_window_active`，注释里引的是「SetForegroundWindow 只在 VS
        // 打开时才生效」那个经典问题），我们只是把它搬到运行时。
        if round == 2 && crate::input::send_keys(&[VK_MENU]) {
            crate::log_print!("[focus] 已注入 Alt 换取前台切换权限");
        }
        let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        let cur = GetCurrentThreadId();
        let attached =
            fg_thread != 0 && fg_thread != cur && AttachThreadInput(fg_thread, cur, 1) != 0;
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        if attached {
            AttachThreadInput(fg_thread, cur, 0);
        }
        if round < 3 {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    GetForegroundWindow() == hwnd
}

/// 提示的落点：面板矩形下沿中点（物理像素）。
///
/// 必须在**收起之前**取。收起之后面板就没了，退化成按光标定位会让提示跑到另一处 ——
/// 实测两种观感都不对：
/// - 键盘选中（`Enter`）时鼠标还停在唤起面板时的位置，提示跟用户盯着的地方无关；
/// - 鼠标点选时提示压在刚粘出去的内容上。
///
/// 贴着面板原来的下沿就稳定得多：面板本身就在光标下方展开，提示接在它下方，
/// 「面板收起/不收起」两种模式的落点一致。
fn panel_anchor(app: &AppHandle) -> Option<(i32, i32)> {
    let w = app.get_webview_window(PANEL_LABEL)?;
    let pos = w.outer_position().ok()?;
    let size = w.inner_size().ok()?;
    Some((
        pos.x + size.width as i32 / 2,
        pos.y + size.height as i32,
    ))
}

/// 光标位置。面板矩形取不到时才退回用它。
fn cursor_point() -> (i32, i32) {
    let mut pt = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetCursorPos(&mut pt) } != 0 {
        return (pt.left, pt.top);
    }
    (200, 200)
}

/// 出一张复制结果提示。文案与落点都由调用方决定 —— 落点取不到（面板还没建过）
/// 才退回光标。
fn toast(anchor: Option<(i32, i32)>, text: &str) {
    let (x, y) = anchor.unwrap_or_else(cursor_point);
    crate::shot_native::toast_near_point(text, x, y);
}

/// 把某条内容复制进剪贴板，再粘回唤出前的窗口。
///
/// 默认粘完就收起面板；「点选后保持打开」开着时留着不动 —— 那时焦点已经还给原窗口，
/// 面板处于「可见但失焦」的状态，可以接着点下一条（点击本身会把焦点拿回来）。
/// 该开关只有在「面板失焦不收起」也开着时才允许打开（见 `settings`），否则留不住。
pub fn paste(app: &AppHandle, id: &str) -> Result<(), String> {
    // 必须在收起之前取：收起后窗口就没了，落点会退化成光标位置
    let anchor = panel_anchor(app);

    // 走与右键「复制」完全相同的路径：格式处理（原样 / 纯文本 / 内联图还原）
    // 只有一份实现，两边不会漂移。
    crate::clipboard::copy_back(id.to_string(), Some(false))?;

    // 先收起再接下面「焦点还给原窗口」那一段：面板还在时抢焦点会被它自己顶回来。
    // 开了「点选后保持打开」就不收，但焦点照样要还回去，否则 Ctrl+V 会发给面板。
    if !crate::settings::get().panel_keep_after_copy {
        if let Some(w) = app.get_webview_window(PANEL_LABEL) {
            let _ = w.hide();
        }
    }

    let target = PREV_FG.load(Ordering::SeqCst);
    if target == 0 {
        // 没有可回的目标：内容已经在剪贴板里，用户自己按 Ctrl+V 即可
        crate::log_print!("[panel] 无目标窗口，已只复制不粘贴");
        toast(anchor, "已复制到剪贴板");
        return Ok(());
    }

    unsafe {
        SetForegroundWindow(target);
        // 焦点切换不是同步的，给系统一点时间；80ms 是实测「不会漏按键」的下限
        std::thread::sleep(Duration::from_millis(80));
        if GetForegroundWindow() != target {
            crate::log_warn!("[panel] 焦点没能切回原窗口，跳过模拟粘贴（内容已在剪贴板）");
            toast(anchor, "已复制到剪贴板");
            return Ok(());
        }
        // 顺序：Ctrl 按下 → V 按下 → V 抬起 → Ctrl 抬起。
        // 注入实现只有 `input` 一份，截图取消发 Esc 走的是同一个函数。
        if !crate::input::send_keys(&[VK_CONTROL, VK_V]) {
            crate::log_warn!("[panel] SendInput 未能送出全部按键（可能被权限拦截）");
        }
    }
    crate::log_print!("[panel] 已粘贴回原窗口");
    // 文案统一成「已复制到剪贴板」：粘贴是否真的落到目标窗口取决于对方，
    // 而复制这一步是确定成功的，提示只说确定的事
    toast(anchor, "已复制到剪贴板");
    Ok(())
}

/// 前端按 Esc 或点空白处时收起面板。
#[tauri::command]
pub fn panel_hide(app: AppHandle) {
    if let Some(w) = app.get_webview_window(PANEL_LABEL) {
        let _ = w.hide();
    }
}

/// 面板里回车选中某条：复制并粘回原窗口。
#[tauri::command]
pub fn panel_paste(app: AppHandle, id: String) -> Result<(), String> {
    paste(&app, &id)
}
