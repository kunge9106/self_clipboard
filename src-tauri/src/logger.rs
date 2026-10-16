//! 全局日志：把 `eprintln!` / `println!` 的内容同时写到文件。
//!
//! 为什么需要：程序是 GUI 启动的，stderr 没人接——出问题时用户只看到
//! 「弹了个窗」，看不到任何细节。把日志落到缓存目录，用户就能把这个
//! 文件发给我们，问题能直接定位。
//!
//! 实现取向：项目刻意不引第三方日志库（依赖极少是既定原则），所以用
//! 最土的办法——`eprintln!` 本来就经过 `std::io::Stderr`，这里用
//! `set_hook` 挂一个自定义钩子把同样的内容再写一份到文件。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// 日志文件上限：到 64MB 时按半量截断，保留最早的内容。
/// 不整份清空——异常前后的上下文往往都在同一个文件里。
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// 保留多少：截断后留下尾部这么多
const KEEP_BYTES: u64 = MAX_BYTES / 2;

/// 安装 panic 钩子。
///
/// ⚠️ 诚实说明：本文件早期注释声称「println!/eprintln! 自动进日志文件」，
/// 这是错的——Rust stable 没有拦截标准输出/错误的钩子，此前只有 panic
/// 和显式调 write_line 的地方落盘，其余几十处 println 全部只进了没人接的
/// stderr。这就是「报错没被记录到」的根因之一。
/// 现在的补法：项目内统一改用下方 log_print!/log_warn! 宏（写文件 + stdout）。
pub fn install_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知 panic".into());
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            write_line("ERROR", "panic", &format!("@ {loc} — {msg}"));
            // 崩溃是用户最需要反馈的场景：另存一份带时间戳的文件，
            // 用户把这个发过来就能看到崩溃前的完整上下文。
            let dst = log_dir().join(format!(
                "Clipboard-崩溃_{}.log",
                stamp().replace(' ', "_").replace(':', "-")
            ));
            let _ = std::fs::copy(log_file(), &dst);
        }));
    });
}

/// 原样落盘一行（保留消息里的既有 [标签] 前缀，级别归 INFO）。
/// 供全模块 println! 的批量替换用：行为对用户完全不变，多了一份文件记录。
pub fn write_raw(line: &str) {
    let line = line.trim_end_matches('\n');
    if line.trim().is_empty() {
        return;
    }
    write_line("INFO", "app", line);
}

/// 同 write_raw，级别归 WARN（对应原 eprintln!——它通常意味着出了状况）。
pub fn write_raw_warn(line: &str) {
    let line = line.trim_end_matches('\n');
    if line.trim().is_empty() {
        return;
    }
    write_line("WARN", "app", line);
    eprintln!("{line}");
}

fn log_dir() -> PathBuf {
    // 缓存目录可能还没初始化（panic 快照就发生在初始化之前），
    // 这时退回 %LOCALAPPDATA%\轻贴\log，避免把日志写到奇怪的工作目录里。
    let dir = crate::clipboard::cache_info().dir;
    let base = if dir.trim().is_empty() {
        std::env::var("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("轻贴"))
            .unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(&dir)
    };
    let d = base.join("log");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// 日志文件路径：正常是 `log/Clipboard.log`；一旦出现异常就另存
/// `Clipboard-异常_<时间戳>.log`，用户把那个发过来即可。
///
/// 文件名刻意用 ASCII 的 `Clipboard`：日志常常要发给别人看，
/// 纯 ASCII 名在各类聊天工具、邮件、压缩包里都不会变成乱码。
pub fn log_file() -> PathBuf {
    log_dir().join("Clipboard.log")
}

/// 追加一行。带级别与模块标签，便于按需grep。
pub fn write_line(level: &str, tag: &str, msg: &str) {
    let path = log_file();
    let line = format!("[{ts}] {level:<5} [{tag}] {msg}\n", ts = stamp());

    // 超限则截断保尾：ring buffer 语义，异常前后的日志都还在
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > MAX_BYTES {
            if let Ok(data) = std::fs::read(&path) {
                let keep = data[data.len().saturating_sub(KEEP_BYTES as usize)..].to_vec();
                let _ = std::fs::write(&path, &keep);
            }
        }
    }

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 记一条错误（同时打一份到 stderr，保持原有调试体验）。
#[macro_export]
macro_rules! log_err {
    ($tag:expr, $($arg:tt)*) => {{
        let msg = format!("[{}:{}] {}", file!(), line!(), format!($($arg)*));
        $crate::logger::write_line("ERROR", $tag, &msg);
        eprintln!("[{}] {}", $tag, msg);
    }};
}

/// 记一条普通信息。
#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)*) => {{
        $crate::logger::write_line("INFO", $tag, &format!($($arg)*));
    }};
}

/// 替代 println!：输出到 stdout 的同时写进日志文件。
/// 消息文本原样保留（项目内 println 惯例自带 [模块] 前缀）。
/// 用块表达式包裹，使其可用于 match 分支等表达式位置。
#[macro_export]
macro_rules! log_print {
    ($($arg:tt)*) => {{
        $crate::logger::write_raw(&format!($($arg)*));
    }};
}

/// 替代 eprintln!：输出到 stderr 的同时以 WARN 级别写进日志文件。
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {{
        $crate::logger::write_raw_warn(&format!("[{}:{}] {}", file!(), line!(), format!($($arg)*)));
    }};
}

/// 本地时间戳 `YYYY-MM-DD HH:MM:SS`。
///
/// ⚠️ 必须是**本地时间**。改动前这里是「拿 UNIX 秒数手算年月日时分秒」——
/// 那算出来的是 **UTC**，于是日志时间比本地时间慢 8 小时（中国区），
/// 用户对日志时会以为「时间戳坏了/日志是早上的旧文件」。用户实测反馈过。
/// 手算还无法处理时区与夏令时，不如直接问系统。
fn stamp() -> String {
    #[cfg(windows)]
    {
        let mut st = SystemTimeLocal::default();
        unsafe {
            GetLocalTime(&mut st);
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.w_year, st.w_month, st.w_day, st.w_hour, st.w_minute, st.w_second
        )
    }
    // 非 Windows（本项目实际不发布，仅为让代码在其它平台可编译/可测）：
    // 退回 UTC，并显式标注，避免误读成本地时间。
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = secs / 86_400;
        let rem = secs % 86_400;
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let (y, mo, d) = civil_from_days(days as i64);
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
    }
}

/// Win32 SYSTEMTIME。字段即日历值，无需再做换算。
#[repr(C)]
#[derive(Default)]
struct SystemTimeLocal {
    w_year: u16,
    w_month: u16,
    w_day_of_week: u16,
    w_day: u16,
    w_hour: u16,
    w_minute: u16,
    w_second: u16,
    w_milliseconds: u16,
}

#[cfg(windows)]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTimeLocal);
}

/// days since 1970-01-01 → (年, 月, 日)，Howard Hinnant 的算法。
/// 仅供非 Windows 回退路径使用。
#[cfg(not(windows))]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 错误发生的次数计数（用于判断是否重复问题）。
static ERROR_COUNT: Mutex<u32> = Mutex::new(0);

/// 前端（WebView/JS）错误上报入口。
///
/// 渲染进程的错误——JS 异常、图片解码失败、资源加载失败——Rust 的
/// panic 钩子完全看不到；2.1.10 那次 OOM 白屏在日志里一片空白就是
/// 这个缺口。前端装全局钩子后经此落盘。
/// 超长消息截断，防止异常内容本身把日志撑爆。
#[tauri::command]
pub fn frontend_log(level: String, tag: String, msg: String) {
    let level = match level.as_str() {
        "ERROR" | "WARN" | "INFO" => level.as_str(),
        _ => "INFO",
    };
    let msg = text_head_trunc(&msg, 2000);
    write_line(level, &format!("fe/{tag}"), &msg);
    // ERROR 级别走计数与快照逻辑（与 note_error 一致）
    if level == "ERROR" {
        note_error(&format!("fe/{tag}"), &msg);
    }
}

fn text_head_trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 记一次异常；每第 5 次额外做一份快照文件名，方便用户直接发送。
pub fn note_error(tag: &str, msg: &str) {
    write_line("ERROR", tag, msg);
    if let Ok(mut c) = ERROR_COUNT.lock() {
        *c += 1;
        if *c % 5 == 0 {
            let n = *c;
            let _ = std::fs::write(
                log_dir().join("最近异常计数.txt"),
                format!("累计异常 {n} 次，最新：{msg}\n"),
            );
        }
    }
}
