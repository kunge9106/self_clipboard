//! 关于页：版本信息、运行环境自检、检查更新。
//!
//! 检查更新走**系统自带的 `curl.exe`**（Win10+ 均自带），不引入 HTTP 客户端依赖，
//! 与项目「依赖极少」的取向一致。

use serde_json::json;

const REPO_URL: &str = "https://github.com/kunge9106/self_clipboard";
const RELEASES_API: &str =
    "https://api.github.com/repos/kunge9106/self_clipboard/releases/latest";

/// 显示用版本号：**测试版(debug)带 `-beta` 后缀**，正式版是干净的版本号。
/// 这样一眼就能区分手上跑的是哪一版，避免"装了一个又跑了一个 debug"的混淆。
pub fn version_display() -> String {
    let v = env!("CARGO_PKG_VERSION");
    if cfg!(debug_assertions) {
        format!("{v}-beta")
    } else {
        v.to_string()
    }
}

// ---------------------------------------------------------------------------
// 进程内存（psapi::GetProcessMemoryInfo，手写 FFI）
// ---------------------------------------------------------------------------

#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

extern "system" {
    fn GetCurrentProcess() -> isize;
}

#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(
        process: isize,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

fn mem_mb() -> u64 {
    let mut c = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let ok = unsafe { GetProcessMemoryInfo(unsafe { GetCurrentProcess() }, &mut c, c.cb) };
    if ok != 0 {
        (c.working_set_size / 1024 / 1024) as u64
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// 版本比较（只比数字段，忽略 -beta 之类后缀）
// ---------------------------------------------------------------------------

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(['.', '-'])
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let l = parse_version(latest);
    let c = parse_version(current);
    for i in 0..l.len().max(c.len()) {
        let a = l.get(i).copied().unwrap_or(0);
        let b = c.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 读一个 HKLM\SOFTWARE 下的字符串值。手写 RegGetValueW，不引 windows crate
/// （与项目既有风格一致；`std::os::windows::registry` 仍是 unstable，编不过）。
fn reg_str(path: &str, name: &str) -> String {
    use std::ffi::c_void;

    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002isize;
    const KEY_READ: u32 = 0x20_019;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;

    extern "system" {
        fn RegOpenKeyExW(
            hkey: isize,
            subkey: *const u16,
            opts: u32,
            sam: u32,
            out: *mut isize,
        ) -> i32;
        fn RegGetValueW(
            hkey: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            out_type: *mut u32,
            data: *mut c_void,
            data_size: *mut u32,
        ) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe {
        let sub = wide(path);
        let mut hkey: isize = 0;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return String::new();
        }
        let val = wide(name);
        let mut typ: u32 = 0;
        let mut size: u32 = 0;
        // hkey 已经指向目标子键，所以 lpSubKey 传 null ——
        // 若这里再传 sub，等于去找「子键\子键」这个不存在的路径，读出来全是空。
        // 第一次调用只为拿数据长度。
        RegGetValueW(
            hkey,
            std::ptr::null(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            &mut typ,
            std::ptr::null_mut(),
            &mut size,
        );
        if size == 0 {
            RegCloseKey(hkey);
            return String::new();
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        let mut n = size;
        let rc = RegGetValueW(
            hkey,
            std::ptr::null(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            &mut typ,
            buf.as_mut_ptr() as *mut c_void,
            &mut n,
        );
        RegCloseKey(hkey);
        if rc != 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf)
            .trim_end_matches('\0')
            .to_string()
    }
}

/// 读取 Windows 版本名、marketing version 与 build 号。
///
/// `ProductName` 给「Windows 11」，`DisplayVersion` 给 marketing version
/// （26H2 这类），`CurrentBuildNumber` 给 build（26300）。三者都要——
/// 只报 build 用户看不出是哪个版本。读不到返回空串，前端显示「未知」不报错。
fn windows_version() -> (String, String, String) {
    const BASE: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let display = reg_str(BASE, "DisplayVersion"); // 26H2
    let build = reg_str(BASE, "CurrentBuildNumber"); // 26300
    let product = reg_str(BASE, "ProductName"); // Windows 11
    // 注册表里的 ProductName 在部分版本上仍是 "Windows 10"，用 build 纠正：
    // 22000 及以上都是 Windows 11。这样比在前端判版本号可靠。
    let build_num: u32 = build.parse().unwrap_or(0);
    let name = if build_num >= 22000 && product.contains("Windows 10") {
        "Windows 11".to_string()
    } else {
        product
    };
    (name, display, build)
}

/// 「关于」页需要的全部信息，一次性返回（避免前端发一堆零碎 invoke）。
#[tauri::command]
pub fn about_info() -> serde_json::Value {
    let s = crate::settings::get();
    let (os_name, disp, build) = windows_version();
    json!({
        "version": version_display(),
        "debug": cfg!(debug_assertions),
        // build 仍单独给出，材质档位判断用的是它
        "osBuild": crate::effects::detect_build(),
        "osName": os_name,
        "osDisplay": disp,
        "osBuildRaw": build,
        "tier": crate::effects::current_tier().as_str(),
        "roundMode": s.round_mode,
        "autoStart": crate::autostart::is_enabled(),
        "memMb": mem_mb(),
        "dataDir": crate::clipboard::cache_info().dir,
        "repoUrl": REPO_URL,
    })
}

/// 截取字符串前 n 个字符（按字符边界，避免切坏 UTF-8）。
fn text_head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 记住上次成功的代理，下次直接先试它，省掉一串失败探测的等待。
///
/// 用 Mutex<Option<String>> 而非 static mut：OnceLock 存不了「可更新」的值，
/// 而这里恰恰需要在每次成功时改写。
static WORKING_PROXY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 探测可用代理，返回**候选列表**（按可靠度排序）。
///
/// 为什么需要：GUI 程序启动的子进程拿不到 shell 里设的 HTTP_PROXY，
/// 而 api.github.com 在部分网络下不通，表现就是「日志空白 / 提示无版本说明」。
///
/// 为什么是列表而不是单个值：
///   - 注册表里的端口可能是代理软件没在用的那个（多实例/多端口很常见）；
///   - 用户可能根本没开代理（此时应直连）；
///   - 各家代理软件端口号都不一样，所以绝不硬编码，靠实测决定。
fn proxy_candidates() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim().to_string();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };

    // 1) 上次成功的排最前——大多数时候第一下就中
    if let Ok(g) = WORKING_PROXY.lock() {
        if let Some(p) = g.as_ref() {
            push(p.clone());
        }
    }

    // 2) 环境变量（命令行/脚本启动时最准）
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(v) = std::env::var(k) {
            push(v);
        }
    }

    // 3) 系统代理（GUI 启动时靠这个）
    let p = reg_str(
        r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        "ProxyServer",
    );
    if !p.is_empty() {
        // 形如 "127.0.0.1:7897" 或 "http=1.2.3.4:8080;https=..."
        // 分号后的每一项都收进来——多端口时不确定哪个在用，实测最可靠。
        for seg in p.split(';') {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            let hostport = seg.rsplit_once('=').map_or(seg, |(_, v)| v).trim();
            if hostport.is_empty() {
                continue;
            }
            if hostport.starts_with("http://") || hostport.starts_with("https://") {
                push(hostport.to_string());
            } else {
                push(format!("http://{hostport}"));
            }
        }
    }

    out
}

/// 记住本次成功的代理（空串表示「直连可用」，同样要记住，
/// 否则下次又会把代理挨个试一遍）。
fn remember_proxy(p: &str) {
    if let Ok(mut g) = WORKING_PROXY.lock() {
        *g = Some(p.to_string());
    }
}

/// 写诊断日志（追加）。一次检查更新会依次试多个候选，全记下来才有排查价值。
/// 写入统一走 logger（超 64MB 按环形截断保尾，不会整份清空）。
fn log_append(line: &str) {
    crate::logger::write_line("INFO", "update", line);
}

/// `CreateProcess` 标志：在新进程里不创建控制台窗口。
///
/// 轻贴是 GUI 程序（`windows_subsystem = "windows"`，自身没有控制台），而 `curl.exe`
/// 是**控制台程序**——不加这个标志，Windows 会为它新建一个黑窗口：用户每进一次关于页、
/// 每点一次检查更新都会看到 cmd 闪现；探测多个代理时更是连着闪好几次。
/// 加上之后子进程无窗口运行，功能与输出（stdout/stderr 管道）完全不受影响。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 用某个代理（或直连，proxy 为空）请求一次，返回 (是否拿到数据, 响应体)。
fn curl_fetch(url: &str, proxy: &str) -> (bool, String) {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let mut cmd = std::process::Command::new("curl");
    // 抑制黑窗口（见 CREATE_NO_WINDOW 注释）
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.args([
        "-sSL",
        "--max-time",
        "12",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: qingtie-self-clipboard",
    ]);
    if !proxy.is_empty() {
        cmd.args(["-x", proxy]);
    }
    cmd.arg(url);

    match cmd.output() {
        Ok(o) => {
            let body = String::from_utf8_lossy(&o.stdout).to_string();
            // 只有拿到 release JSON 才算成功；错误 JSON（限流/404）不算
            let ok = o.status.success() && body.contains("\"tag_name\"");
            log_append(&format!(
                "[{}] proxy={} code={:?} ok={ok} len={}\n    body={}",
                now_stamp(),
                if proxy.is_empty() { "直连" } else { proxy },
                o.status.code(),
                body.len(),
                text_head(&body.replace('\n', " "), 200)
            ));
            (ok, body)
        }
        Err(e) => {
            log_append(&format!(
                "[{}] proxy={} 启动 curl 失败: {e}",
                now_stamp(),
                if proxy.is_empty() { "直连" } else { proxy }
            ));
            (false, String::new())
        }
    }
}

/// 本地时间戳（到秒），只用 std 不引chrono。
fn now_stamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 直接显示 epoch 秒即可，排查时对照文件修改时间就够
    d.to_string()
}

/// 检查 GitHub 上是否有新版本。
#[tauri::command]
pub fn check_update() -> Result<serde_json::Value, String> {
    // 候选代理逐个实测；最后再试一次直连（有人压根不开代理、境外网络直连更快）。
    let mut candidates = proxy_candidates();
    candidates.push(String::new()); // 空串 = 直连
    log_append(&format!(
        "────── 检查更新 v{} | 候选 {} 条 ──────",
        version_display(),
        candidates.len()
    ));

    let mut text = String::new();
    let mut rate_limited = false;
    for proxy in &candidates {
        let (ok, body) = curl_fetch(RELEASES_API, proxy);
        if ok {
            text = body;
            remember_proxy(proxy);
            break;
        }
        if body.contains("rate limit") {
            rate_limited = true;
        }
    }

    if text.is_empty() {
        // 全部候选都失败才算异常：计数 + 留痕，用户反馈时带上日志即可定位
        crate::logger::note_error(
            "update",
            &format!(
                "检查更新失败：试了{} 个候选（{}）",
                candidates.len(),
                if rate_limited {
                    "含限流"
                } else {
                    "均无法连接"
                }
            ),
        );
        return Err(if rate_limited {
            "GitHub 请求过于频繁（匿名接口限流），请稍后再试".into()
        } else {
            format!(
                "无法连接 GitHub（已尝试 {} 个代理和直连），请检查网络",
                candidates.len() - 1
            )
        });
    }

    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("解析更新信息失败：{e}"))?;

    let latest = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();

    Ok(json!({
        "hasUpdate": is_newer(&latest, env!("CARGO_PKG_VERSION")),
        "latest": latest,
        // 纯版本号（不带 -beta），供前端显示与比较；带后缀的展示号单独给 displayCurrent
        "current": env!("CARGO_PKG_VERSION"),
        "displayCurrent": version_display(),
        "url": v.get("html_url").and_then(|x| x.as_str()).unwrap_or(""),
        "notes": v.get("body").and_then(|x| x.as_str()).unwrap_or(""),
        "publishedAt": v.get("published_at").and_then(|x| x.as_str()).unwrap_or(""),
    }))
}

/// 打开外部链接（项目主页 / 新建反馈 / 下载页）。
/// 只允许 http(s)，避免被塞进 `file://` 之类的本地路径。
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("仅允许打开 http/https 链接".into());
    }
    std::process::Command::new("explorer")
        .arg(url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
