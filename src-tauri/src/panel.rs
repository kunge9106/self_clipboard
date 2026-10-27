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
        unsafe { force_foreground(h.0 as isize) };
    }

    // 每次弹出都让页面重新拉一次数据：面板是常驻的，不刷新就会看到旧列表，
    // 而「刚复制的那条在最上面」正是用户最先要的。
    let _ = app.emit_to(PANEL_LABEL, "panel-open", ());
}

/// 把窗口强行拉到前台。
///
/// 非前台进程调用 `SetForegroundWindow` 会被系统拦（防止弹窗骚扰），绕法是先
/// `AttachThreadInput` 把自己的输入队列挂到**当前前台线程**上，此时系统认为两者
/// 是同一个输入上下文，设置才能生效；做完立刻解挂，否则两边键盘状态会互相影响。
unsafe fn force_foreground(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let fg = GetForegroundWindow();
    if fg == hwnd {
        return;
    }
    let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
    let cur = GetCurrentThreadId();
    let attached = fg_thread != 0 && fg_thread != cur && AttachThreadInput(fg_thread, cur, 1) != 0;
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    if attached {
        AttachThreadInput(fg_thread, cur, 0);
    }
}

/// 把某条内容复制进剪贴板，收起面板，再粘回唤出前的窗口。
pub fn paste(app: &AppHandle, id: &str) -> Result<(), String> {
    // 走与右键「复制」完全相同的路径：格式处理（原样 / 纯文本 / 内联图还原）
    // 只有一份实现，两边不会漂移。
    crate::clipboard::copy_back(id.to_string(), Some(false))?;

    if let Some(w) = app.get_webview_window(PANEL_LABEL) {
        let _ = w.hide();
    }

    let target = PREV_FG.load(Ordering::SeqCst);
    if target == 0 {
        // 没有可回的目标：内容已经在剪贴板里，用户自己按 Ctrl+V 即可
        crate::log_print!("[panel] 无目标窗口，已只复制不粘贴");
        return Ok(());
    }

    unsafe {
        SetForegroundWindow(target);
        // 焦点切换不是同步的，给系统一点时间；80ms 是实测「不会漏按键」的下限
        std::thread::sleep(Duration::from_millis(80));
        if GetForegroundWindow() != target {
            crate::log_warn!("[panel] 焦点没能切回原窗口，跳过模拟粘贴（内容已在剪贴板）");
            return Ok(());
        }
        // 顺序：Ctrl 按下 → V 按下 → V 抬起 → Ctrl 抬起。
        // 注入实现只有 `input` 一份，截图取消发 Esc 走的是同一个函数。
        if !crate::input::send_keys(&[VK_CONTROL, VK_V]) {
            crate::log_warn!("[panel] SendInput 未能送出全部按键（可能被权限拦截）");
        }
    }
    crate::log_print!("[panel] 已粘贴回原窗口");
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
