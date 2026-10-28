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
use std::sync::{Mutex, OnceLock};

/// 日志文件上限：到 64MB 时轮转 —— 当前文件 rename 成 `Clipboard.prev.log`，
/// 新文件继续写。不整份清空：异常前后的上下文分别落在两代文件里，都还在。
const MAX_BYTES: u64 = 64 * 1024 * 1024;

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

/// 配置文件路径（`config.json`）。启动最早期由 `lib.rs` 告诉我们，
/// 因为那时 `clipboard` 还没初始化，取不到缓存目录。
///
/// 为什么必须提前知道：日志原本要等 `clipboard::init` 之后才知道缓存目录，
/// 于是**启动最初那几条日志会落到 `%LOCALAPPDATA%\轻贴\log`** ——
/// 恰恰是「启动即崩溃」最需要的那段现场，却和主日志分在两个文件里，
/// 用户按「缓存目录/log」去找只能看到半截。
static CONFIG_HINT: OnceLock<PathBuf> = OnceLock::new();

pub fn hint_config_file(p: PathBuf) {
    let _ = CONFIG_HINT.set(p);
}

/// 从 config.json 里直接抠 `cache_dir`（不经过 settings 模块，避免初始化顺序耦合）。
fn cache_dir_from_config() -> Option<PathBuf> {
    let p = CONFIG_HINT.get()?;
    let s = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let d = v.get("cache_dir")?.as_str()?;
    if d.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(d))
    }
}

fn log_dir() -> PathBuf {
    let dir = crate::clipboard::cache_info().dir;
    let base = if dir.trim().is_empty() {
        // 缓存目录还没初始化（启动早期 / panic 就发生在这里）：
        // 先从配置文件里找用户自定义的目录，找不到才退回 LOCALAPPDATA。
        if let Some(custom) = cache_dir_from_config() {
            custom
        } else {
            std::env::var("LOCALAPPDATA")
                .map(|p| PathBuf::from(p).join("轻贴"))
                .unwrap_or_else(|_| PathBuf::from("."))
        }
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

    // 超限则截断保尾：ring buffer 语义，异常前后的日志都还在。
    //
    // 老实现是「读整个 64MB → 截掉前半 → 整体重写」：单次轮转就要读 64MB 进内存
    // 再写 32MB，恰好发生在出问题、日志最密集的时候，会把界面卡住。改成 rename
    // 留一代旧文件：O(1)、不动内存，异常前后的内容分别在 Clipboard.prev.log 与
    // Clipboard.log 里，反而更容易翻。
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > MAX_BYTES {
            let prev = path.with_file_name("Clipboard.prev.log");
            let _ = std::fs::remove_file(&prev);
            if let Err(e) = std::fs::rename(&path, &prev) {
                // 轮转失败不能让日志断掉：继续往原文件追加即可，但必须留痕
                eprintln!("[logger] 日志轮转失败：{e}");
            }
        }
    }

    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match opened {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                // 磁盘满 / 文件被占用时会走到这里。以前是彻底静默 ——
                // 日志系统自己坏掉，用户却以为「没什么异常」，是最后的盲区。
                eprintln!("[logger] 日志写入失败（{}）：{e}", path.display());
                eprintln!("{}", line.trim_end());
            }
        }
        Err(e) => {
            // 连文件都打不开（目录不可写、被安全软件拦截）：至少让 stderr 有内容，
            // 用户从终端启动或在开发机上仍能看到。
            eprintln!("[logger] 日志文件打开失败（{}）：{e}", path.display());
            eprintln!("{}", line.trim_end());
        }
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
///
/// 对 crate 内开放：图片按日期时间命名（`clipboard` 里把条目的时间戳转成本地
/// 日历值）也要用同一个结构 —— FFI 契约结构各写一份迟早对不上字段顺序。
#[repr(C)]
#[derive(Default)]
pub(crate) struct SystemTimeLocal {
    pub(crate) w_year: u16,
    pub(crate) w_month: u16,
    pub(crate) w_day_of_week: u16,
    pub(crate) w_day: u16,
    pub(crate) w_hour: u16,
    pub(crate) w_minute: u16,
    pub(crate) w_second: u16,
    pub(crate) w_milliseconds: u16,
}

#[cfg(windows)]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTimeLocal);
}

/// days since 1970-01-01 → (年, 月, 日)，Howard Hinnant 的算法。
/// 仅供非 Windows 回退路径使用。
#[cfg(not(windows))]
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
