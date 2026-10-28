//! 全局快捷键。
//!
//! 用 Win32 `RegisterHotKey` + **独立线程的消息循环**，而不是往主窗口的 WndProc 里塞：
//! 主窗口已经有一层子类化（材质、阴影重涂），再挂 WM_HOTKEY 会把「热键」和「窗口外观」
//! 两件事耦在一起，出问题时很难判断是谁的锅。
//!
//! 注册是**线程绑定**的：`RegisterHotKey(0, …)` 与调用它的线程绑定，消息也只投给那个线程，
//! 所以改键必须让同一个线程去重注册 —— 这就是 `PENDING` + `WM_RELOAD` 存在的原因。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

/// 唤出面板的热键 ID。同一线程内唯一即可，取 "QT" 的 ASCII 值。
pub const HOTKEY_MAIN: i32 = 0x5154;
/// 截图的热键 ID（"QS"）。
pub const HOTKEY_SHOT: i32 = 0x5153;

/// 系统投递给注册线程的热键消息。
const WM_HOTKEY: u32 = 0x0312;

/// 自定义消息：让热键线程按最新配置重新注册（`WM_APP + 1`）。
const WM_RELOAD: u32 = 0x8000 + 1;

const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;
/// 长按不连发。否则按住不放会不停唤出窗口。
const MOD_NOREPEAT: u32 = 0x4000;

/// 热键线程的线程 ID（`GetCurrentThreadId`）。0 = 线程还没起来。
static THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// 待处理的注册请求：**所有槽位**的新组合 + 结果回传通道。
///
/// 一次提交全部槽位而不是逐个改：注册是「先全注销、再逐个注册」，
/// 分几次做会让另一个热键在中间短暂失效，甚至被别的程序抢走。
///
/// 为什么需要回传：注册可能失败（组合被别的程序占用），而失败必须能告诉用户 ——
/// 否则他改完键位、按了没反应，只能猜是软件坏了。
static PENDING: Mutex<Option<(Vec<(i32, String)>, Sender<Result<(), String>>)>> = Mutex::new(None);

#[repr(C)]
pub(crate) struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    private: u32,
}

extern "system" {
    fn RegisterHotKey(hwnd: isize, id: i32, modifiers: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: isize, id: i32) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn PostThreadMessageW(thread_id: u32, msg: u32, w_param: usize, l_param: isize) -> i32;
    fn GetCurrentThreadId() -> u32;
}

/// 启动热键线程。**只调一次**（`lib.rs` 的 setup 里）。
pub fn init(app: tauri::AppHandle) {
    std::thread::spawn(move || unsafe {
        THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);

        // 启动时按配置注册一次。失败只影响「这台机器上热键不生效」，
        // 不能让热键线程直接退出 —— 否则改键也救不回来，只能重启程序。
        let cfg = crate::settings::get();
        let _ = register_slots(&slots_of(&cfg));

        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            if msg.message == WM_HOTKEY {
                // `wParam` 就是注册时的 ID —— 两个热键共用一条消息循环，
                // 靠它区分是哪一个。
                match msg.w_param as i32 {
                    HOTKEY_MAIN => {
                        crate::log_print!("[hotkey] 触发：快捷面板");
                        crate::panel::toggle(&app);
                    }
                    HOTKEY_SHOT => {
                        // 走哪套实现由设置决定：内置覆盖层默认，系统截图留作备选
                        // （系统那套自带标注，需要画箭头时可以切过去）。
                        let mode = crate::settings::get().screenshot_mode;
                        crate::log_print!("[hotkey] 触发：截图（{mode}）");
                        if mode == "system" {
                            crate::screenclip::launch();
                        } else {
                            // 原生覆盖层：不经过渲染进程，输入与重绘都在自己的消息循环里
                            crate::shot_native::start(&app);
                        }
                    }
                    other => crate::log_warn!("[hotkey] 未知热键 ID {other}"),
                }
            } else if msg.message == WM_RELOAD {
                let job = PENDING.lock().ok().and_then(|mut g| g.take());
                if let Some((slots, tx)) = job {
                    let r = register_slots(&slots);
                    if let Err(e) = &r {
                        crate::log_warn!("[hotkey] 改键后注册失败：{e}");
                    }
                    // 接收端可能已经超时放弃，发送失败不算问题
                    let _ = tx.send(r);
                }
            } else if msg.message == crate::screenclip::WM_INSTALL_HOOK {
                // 低级鼠标钩子必须装在有消息循环的线程上 —— 就装在这里，
                // 不再单独起一个线程和一套 MSG 结构。
                crate::screenclip::install_hook();
            } else if msg.message == crate::screenclip::WM_REMOVE_HOOK {
                crate::screenclip::remove_hook();
            }
        }
        crate::log_warn!("[hotkey] 消息循环退出，热键不再生效");
    });
}

/// 按配置算出两个槽位要注册的组合（禁用 = 空串）。
fn slots_of(cfg: &crate::settings::Config) -> Vec<(i32, String)> {
    vec![
        (
            HOTKEY_MAIN,
            if cfg.hotkey_enabled {
                cfg.hotkey.trim().to_string()
            } else {
                String::new()
            },
        ),
        (
            HOTKEY_SHOT,
            if cfg.screenshot_hotkey_enabled {
                cfg.screenshot_hotkey.trim().to_string()
            } else {
                String::new()
            },
        ),
    ]
}

/// 改键 / 启停。两个槽位**一起提交**。
///
/// 返回的是**实际注册结果**：被占用时前端要即时提示，不能让用户以为设成功了。
pub fn apply(main: (&str, bool), shot: (&str, bool)) -> Result<(), String> {
    let slots = vec![
        (
            HOTKEY_MAIN,
            if main.1 {
                main.0.trim().to_string()
            } else {
                String::new()
            },
        ),
        (
            HOTKEY_SHOT,
            if shot.1 {
                shot.0.trim().to_string()
            } else {
                String::new()
            },
        ),
    ];
    post(slots)
}

fn post(slots: Vec<(i32, String)>) -> Result<(), String> {
    let tid = THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        // 线程还没起来（极少见：启动瞬间改了设置）—— 配置已存，启动时会注册
        return Ok(());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    *PENDING.lock().map_err(|_| "热键状态锁失效")? = Some((slots, tx));
    unsafe {
        PostThreadMessageW(tid, WM_RELOAD, 0, 0);
    }
    rx.recv_timeout(std::time::Duration::from_millis(800))
        .map_err(|_| "快捷键注册超时（热键线程无响应）".to_string())?
}

/// 全部注销（录制快捷键时用：否则按键被系统截走，界面录不到）。
pub fn suspend() -> Result<(), String> {
    post(vec![(HOTKEY_MAIN, String::new()), (HOTKEY_SHOT, String::new())])
}

/// 按当前配置恢复注册。
pub fn resume() -> Result<(), String> {
    post(slots_of(&crate::settings::get()))
}

/// 在当前线程注册整批槽位：先全部注销，再逐个注册。
///
/// 一个槽位失败不影响另一个（继续往下注册），错误只报第一个 —— 常见情况是
/// 「截图键被占用、面板键没问题」，那种时候不该把面板键一起废掉。
/// 最近一次注册失败的原因（成功则为 None）。
///
/// 前端在设置页据此提示「这个组合被别的程序占了」。
/// 为什么不让后端直接发事件：启动那一刻前端可能还没就绪，事件大概率丢失；
/// 改成「前端主动来问」时序才稳定。
static LAST_ERR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 当前热键是否可用；`Some(原因)` 表示被别的程序占用。
#[tauri::command]
pub fn hotkey_status() -> Option<String> {
    LAST_ERR.lock().ok().and_then(|g| g.clone())
}

unsafe fn register_slots(slots: &[(i32, String)]) -> Result<(), String> {
    for (id, _) in slots {
        UnregisterHotKey(0, *id);
    }
    let mut first_err: Option<String> = None;
    for (id, combo) in slots {
        if combo.trim().is_empty() {
            continue;
        }
        match parse_combo(combo).and_then(|(mods, vk)| {
            if RegisterHotKey(0, *id, mods | MOD_NOREPEAT, vk) == 0 {
                Err(format!("{combo} 已被其它程序占用，请换一个组合"))
            } else {
                Ok(())
            }
        }) {
            Ok(()) => crate::log_print!("[hotkey] 已注册 {combo}"),
            Err(e) => {
                crate::log_warn!("[hotkey] 注册失败：{e}");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Ok(mut g) = LAST_ERR.lock() {
        *g = first_err.clone();
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 给热键线程投递一条自定义消息。
///
/// 所有需要「有消息泵的线程」的活儿（装/卸低级输入钩子）都走这里，不再各起一个线程：
/// 一个 Win32 消息线程服务热键、鼠标钩子、键盘钩子三件事，排查时只看一处。
pub fn post_msg(msg: u32) {
    post_msg_w(msg, 0);
}

/// 带一个整型参数投递（`wParam`）。目前只有截图钩子的开关用它带会话序号。
pub fn post_msg_w(msg: u32, param: usize) {
    let tid = THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        crate::log_warn!("[hotkey] 热键线程未就绪，消息 {msg:#x} 被丢弃");
        return;
    }
    unsafe {
        PostThreadMessageW(tid, msg, param, 0);
    }
}


/// `"ctrl+alt+v"` → (修饰键, 虚拟键码)。
fn parse_combo(combo: &str) -> Result<(u32, u32), String> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in combo.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            other => key = Some(vk_of(other)?),
        }
    }
    let vk = key.ok_or("缺少主键（例如 V）")?;
    if mods == 0 {
        // 纯单键会抢掉正常输入，必须带修饰键
        return Err("至少需要一个修饰键（Ctrl / Alt / Shift）".into());
    }
    Ok((mods, vk))
}

fn vk_of(name: &str) -> Result<u32, String> {
    let b = name.as_bytes();
    if b.len() == 1 {
        let c = b[0];
        if c.is_ascii_lowercase() {
            return Ok((c - b'a' + 0x41) as u32);
        }
        if c.is_ascii_digit() {
            return Ok((c - b'0' + 0x30) as u32);
        }
        if c == b'`' {
            return Ok(0xC0);
        }
        if c == b'-' {
            return Ok(0xBD);
        }
    }
    if name == "space" {
        return Ok(0x20);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|x| x.parse::<u32>().ok()) {
        if (1..=12).contains(&n) {
            return Ok(0x70 + n - 1);
        }
    }
    Err(format!("不认识的按键：{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 组合串解析：修饰键顺序无关、大小写无关。
    #[test]
    fn parse_combo_accepts_any_order_and_case() {
        assert_eq!(parse_combo("ctrl+alt+v").unwrap(), (MOD_CONTROL | MOD_ALT, 0x56));
        assert_eq!(parse_combo("ALT+Ctrl+V").unwrap(), (MOD_CONTROL | MOD_ALT, 0x56));
        assert_eq!(parse_combo("ctrl+shift+space").unwrap(), (MOD_CONTROL | MOD_SHIFT, 0x20));
        assert_eq!(parse_combo("alt+`").unwrap(), (MOD_ALT, 0xC0));
        assert_eq!(parse_combo("ctrl+f5").unwrap(), (MOD_CONTROL, 0x74));
    }

    /// 不允许「没有修饰键」或「没有主键」—— 前者会抢掉正常输入，后者没法注册。
    #[test]
    fn parse_combo_rejects_incomplete() {
        assert!(parse_combo("v").is_err(), "纯单键必须拒绝");
        assert!(parse_combo("ctrl").is_err(), "只有修饰键必须拒绝");
        assert!(parse_combo("ctrl+alt+?").is_err(), "未知按键必须拒绝");
    }
}
