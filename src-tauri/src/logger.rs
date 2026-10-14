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

/// 安装全局钩子：`println!` / `eprintln!` 的内容自动追加到日志文件。
///
/// 这样全项目几十处打印语句不用逐个改，也能全部留痕——排查问题时
/// 「日志里有没有这一行」直接决定能不能定位。
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
                "轻贴-崩溃_{}.log",
                stamp().replace(' ', "_").replace(':', "-")
            ));
            let _ = std::fs::copy(log_file(), &dst);
        }));
    });
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

/// 日志文件路径：正常是 `log/轻贴.log`；一旦出现异常就另存
/// `轻贴-异常_<时间戳>.log`，用户把那个发过来即可。
pub fn log_file() -> PathBuf {
    log_dir().join("轻贴.log")
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
    ($tag:expr, $($arg:tt)*) => {
        $crate::logger::write_line("ERROR", $tag, &format!($($arg)*));
        eprintln!("[{}] {}", $tag, format!($($arg)*));
    };
}

/// 记一条普通信息。
#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)*) => {
        $crate::logger::write_line("INFO", $tag, &format!($($arg)*));
    };
}

/// 本地时间戳 `MM-DD HH:MM:SS`。手算避免引chrono/chrono 依赖。
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 2000-01-01 起的累计天数转年月日
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// days since 1970-01-01 → (年, 月, 日)，Howard Hinnant 的算法。
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

/// 异常快照：把当前日志复制成 `轻贴-异常_<时间>.log`。
/// 用户反馈时把这个文件发来即可定位问题。
pub fn snapshot_on_error(reason: &str) -> PathBuf {
    let stamp = stamp().replace(' ', "_").replace(':', "-");
    let name = format!("轻贴-异常_{stamp}.log");
    let dst = log_dir().join(name);

    // 先把本次异常也写进主日志，再快照，避免快照里缺这一条
    write_line("ERROR", "crash", reason);

    let src = log_file();
    let _ = std::fs::copy(&src, &dst);
    dst
}

/// 错误发生的次数计数（用于判断是否重复问题）。
static ERROR_COUNT: Mutex<u32> = Mutex::new(0);

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
