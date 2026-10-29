//! 关于页：版本信息、运行环境自检、检查更新。
//!
//! 检查更新走**系统自带的 `curl.exe`**（Win10+ 均自带），不引入 HTTP 客户端依赖，
//! 与项目「依赖极少」的取向一致。

use serde_json::json;

const REPO_URL: &str = "https://github.com/kunge9106/self_clipboard";
const RELEASES_API: &str =
    "https://api.github.com/repos/kunge9106/self_clipboard/releases/latest";
/// 测试版用：列表接口会把 pre-release 一并返回（`latest` 接口按定义会跳过它们）。
const RELEASES_LIST_API: &str =
    "https://api.github.com/repos/kunge9106/self_clipboard/releases?per_page=30";

/// 显示用版本号。正式版是干净的版本号，本地测试版额外标出来。
///
/// 版本号自身已经带预发布后缀（如 `2.1.9-beta`）时不再重复追加，改加 `-dev`：
/// 这样「装了一个又跑了一个 debug」仍然一眼可辨，同时不会出现
/// `2.1.9-beta-beta` 这种没法看的字符串。
pub fn version_display() -> String {
    let v = env!("CARGO_PKG_VERSION");
    if cfg!(debug_assertions) {
        if v.contains('-') {
            format!("{v}-dev")
        } else {
            format!("{v}-beta")
        }
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

pub(crate) fn mem_mb() -> u64 {
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
    // GetCurrentProcess() 本身是 safe，只有读取内存计数需要 unsafe
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
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
    // 只取**核心段**（`2.4.1-beta.2` → [2,4,1]）。
    // 原来把 `-` 也当分隔符，于是 `2.4.1-beta.2` 变成 [2,4,1,0,2] ——
    // 多出来的 `2` 让它比 `2.4.1` 还大，预发布版被误判成「更新的版本」。
    v.trim_start_matches('v')
        .split('-')
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}

/// 预发布标识（`2.4.1-beta.2` → `["beta","2"]`），没有则 `None`。
fn prerelease(v: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = v.trim_start_matches('v').splitn(2, '-').collect();
    match parts.as_slice() {
        [_, pre] if !pre.is_empty() => Some(pre.split('.').collect()),
        _ => None,
    }
}

/// 按 semver 比较预发布标识：数字段按数值、字符串段按字典序、数字段小于字符串段，
/// 前缀相同时更长的那一方更大（`-alpha < -alpha.1`）。
fn cmp_prerelease(a: &[&str], b: &[&str]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for i in 0..a.len().max(b.len()) {
        match (a.get(i), b.get(i)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>().ok(), y.parse::<u64>().ok()) {
                    (Some(p), Some(q)) => p.cmp(&q),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    Ordering::Equal
}

/// 数字段逐位比较；数字段完全相同、只有一方带预发布后缀（`-beta`）时，
/// **不带后缀的那个更新**（`2.1.9` > `2.1.9-beta`）。
///
/// 没有这一步，「beta 测完转正式」用户永远收不到提示——
/// 两个版本号的数字段一样，逐位比较会判为「无更新」。
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
    // 数字段相同才轮到预发布比较。原来只判「有没有 `-`」，`-beta.2` 这种带序号的
    // 后缀会被当成比正式版更新（`parse_version` 把 `2` 也吃进去了），已由
    // `parse_version` + `cmp_prerelease` 一并修正。
    match (prerelease(latest), prerelease(current)) {
        (None, None) => false,
        (None, Some(_)) => true,  // 正式版高于自己的预发布
        (Some(_), None) => false,
        (Some(a), Some(b)) => cmp_prerelease(&a, &b) == std::cmp::Ordering::Greater,
    }
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

/// 候选代理 + 末尾的「直连」空串。
///
/// 直连必须放在最后：有人压根不开代理、境外网络直连更快，所以不能省；
/// 但放在前面会让每次检查都先白等一个 12 秒超时。
fn proxy_candidates_with_direct() -> Vec<String> {
    let mut c = proxy_candidates();
    c.push(String::new());
    c
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
            // 只有拿到 release JSON 才算成功；错误 JSON（限流/404）不算。
            // 列表接口在一个 release 都没有时会返回 `[]`——那也算「请求成功」。
            let ok = o.status.success()
                && (body.contains("\"tag_name\"") || body.trim() == "[]");
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

// ---------------------------------------------------------------------------
// 更新检查
// ---------------------------------------------------------------------------

/// 一次更新查询的结果。
///
/// 两种模式共用这个结构，差别只在「查哪个接口」：
///   - **正式版**（版本号不带后缀）只查 `/releases/latest`。该接口按定义
///     **跳过 pre-release 与 draft**，所以正式版天然看不到 beta ——
///     这正是「beta 不打扰正式用户」的官方机制（Release 没有 private 开关，
///     可见性只到仓库级；而 draft 要 token 才读得到，程序里不可能存 token）。
///   - **测试版**（版本号带 `-beta` 这类后缀）查 `/releases` 列表，在全部
///     release 里挑版本号最高的（含 pre-release），所以 beta 既能看到新 beta，
///     也能看到新正式版。
#[derive(Default)]
struct Release {
    latest: String,
    url: String,
    notes: String,
    asset_url: String,
    asset_name: String,
    asset_size: u64,
}

/// 当前构建是不是预发布版（`2.1.9-beta`）——决定查 `latest` 还是列表接口。
fn is_prerelease_build() -> bool {
    env!("CARGO_PKG_VERSION").contains('-')
}

/// 用 curl 逐候选（代理 + 末尾直连）请求一个 URL，返回解析好的 JSON。
fn fetch_json(url: &str, candidates: &[String]) -> Result<serde_json::Value, String> {
    let mut text = String::new();
    let mut rate_limited = false;
    for proxy in candidates {
        let (ok, body) = curl_fetch(url, proxy);
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
                "连不上 GitHub（试了 {} 个代理和直连），请检查网络",
                candidates.len().saturating_sub(1)
            )
        });
    }

    serde_json::from_str(text.trim()).map_err(|e| format!("解析更新信息失败：{e}"))
}

/// 从一条 release JSON 里抽出要用的字段。安装包优先取名字带 `setup` 的 exe。
fn release_from_json(v: &serde_json::Value) -> Release {
    let tag = v.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    let assets = v.get("assets").and_then(|a| a.as_array());
    let is_exe = |a: &&serde_json::Value| {
        a.get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .ends_with(".exe")
    };
    let pick = assets.and_then(|list| {
        list.iter()
            .find(|a| {
                let n = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
                n.ends_with(".exe") && n.contains("setup")
            })
            .or_else(|| list.iter().find(is_exe))
    });

    Release {
        latest: tag.trim_start_matches('v').to_string(),
        url: v
            .get("html_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        notes: v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        asset_url: pick
            .and_then(|a| a.get("browser_download_url").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string(),
        asset_name: pick
            .and_then(|a| a.get("name").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string(),
        asset_size: pick
            .and_then(|a| a.get("size").and_then(|x| x.as_u64()))
            .unwrap_or(0),
    }
}

/// 查询「当前该推荐哪个版本」。正式版查 latest，测试版查列表。
fn fetch_release() -> Result<Release, String> {
    let candidates = proxy_candidates_with_direct();

    if !is_prerelease_build() {
        log_append(&format!(
            "────── 检查更新 v{}（正式版：只查 latest，自动跳过 pre-release）| 候选 {} 条 ──────",
            version_display(),
            candidates.len()
        ));
        let v = fetch_json(RELEASES_API, &candidates)?;
        return Ok(release_from_json(&v));
    }

    log_append(&format!(
        "────── 检查更新 v{}（测试版：查列表，含 pre-release）| 候选 {} 条 ──────",
        version_display(),
        candidates.len()
    ));
    let v = fetch_json(RELEASES_LIST_API, &candidates)?;
    let list = v.as_array().ok_or("更新信息格式异常")?;
    let mut best: Option<Release> = None;
    for r in list
        .iter()
        .filter(|r| !r.get("draft").and_then(|d| d.as_bool()).unwrap_or(false))
        .map(release_from_json)
        .filter(|r| !r.latest.is_empty())
    {
        // 字符串比较不可靠（要处理 -beta 后缀），统一用 is_newer 定序
        best = Some(match best {
            Some(b) if !is_newer(&r.latest, &b.latest) => b,
            _ => r,
        });
    }
    best.ok_or_else(|| "没有找到任何发布版本".into())
}

/// 随包内置的更新日志（仓库根 `CHANGELOG.md`，**编译期内嵌**进 exe）。
///
/// 为什么需要：GitHub 上未必存在「当前这一版」的 Release —— beta 版尤其如此。
/// 那时远端返回的是上一个正式版的说明，用户看到的就不是自己这版改了什么。
/// 内置一份就没这个问题：本机跑的是哪一版，就显示哪一版的说明。
const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

/// 关于页最多一次显示几节更新日志。
///
/// 用户从很老的版本升上来时（比如 2.2 → 2.4.1），把中间每一节全列出来会把关于页
/// 变成一篇长文。取最近 8 节已经能覆盖几个月的跨度；更早的说明在项目主页看。
const MAX_NOTE_SECTIONS: usize = 8;

/// 没有「升级起点」时显示几节。
///
/// 起点字段是本版才加的，**现有用户升级上来时配置里没有它** —— 无法知道确切跨度。
/// 这种情况下退化为「最近 N 节」，而不是只显示当前一节：否则正是最需要看更新的
/// 这批老用户，反而只看到一屏说明。N=4 能覆盖绝大多数升级跨度（一般人不会连跳
/// 四五个版本）。
const FALLBACK_NOTE_SECTIONS: usize = 4;

/// 把 CHANGELOG 拆成 `(版本号, 正文)`，保持文件里的顺序（新 → 旧）。
fn changelog_sections() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in CHANGELOG.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((v, body)) = cur.take() {
                out.push((v, body.join("\n").trim().to_string()));
            }
            cur = Some((
                rest.trim().trim_start_matches('v').to_string(),
                Vec::new(),
            ));
            continue;
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push(line);
        }
    }
    if let Some((v, body)) = cur.take() {
        out.push((v, body.join("\n").trim().to_string()));
    }
    out
}

/// 取「`from` 之后、到 `upto` 为止」的所有版本节，拼成一段 Markdown。
///
/// 跨版本升级时只看当前那一节是不够的 —— 用户从 2.3.0 升到 2.4.1，中间 2.4.0 改了什么
/// 同样是「这次更新带来的变化」，漏掉就不知道新增了哪些功能。
///
/// `from` 为空（首次安装 / 配置里没有记录）时只返回 `upto` 这一节。
/// 返回 `(拼接后的 Markdown, 实际包含的版本号列表)`，后者供前端显示区间标题。
fn changelog_since(from: &str, upto: &str) -> (String, Vec<String>) {
    let to = upto.trim().trim_start_matches('v');
    let from = from.trim().trim_start_matches('v');
    let has_origin = !from.is_empty();
    let picked: Vec<(String, String)> = changelog_sections()
        .into_iter()
        // 比当前版本还新的节不显示：CHANGELOG 里可能已经写好了尚未发布的版本
        .filter(|(v, _)| !is_newer(v, to))
        .filter(|(v, _)| {
            if has_origin {
                // **含起点**：用户未必真的打开过关于页，漏掉「他当前那版」的说明
                // 比多显示一节糟得多。所以取「起点 ≤ 版本 ≤ 当前」。
                !is_newer(from, v)
            } else {
                true // 无起点：由下面的 take 决定显示几节
            }
        })
        .take(if has_origin {
            MAX_NOTE_SECTIONS
        } else {
            FALLBACK_NOTE_SECTIONS
        })
        .collect();

    let versions: Vec<String> = picked.iter().map(|(v, _)| v.clone()).collect();
    // 节标题保留 CHANGELOG 原来的写法（`## 2.4.0`），前端 md() 会渲染成小标题，
    // 多节并列时靠它分段。
    let body = picked
        .iter()
        .map(|(v, text)| format!("## {v}\n\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    (body, versions)
}


/// 统一的结果 payload（`check_update` 与 `update_state` 返回同一个形状）。
fn payload(rel: &Release) -> serde_json::Value {
    let cur = env!("CARGO_PKG_VERSION");
    // 已下载但未安装的安装包（用户上次在安装器上取消了）：有的话前端把
    // 按钮切成「重新安装」。只认当前版本的资产名，旧版本包不算数。
    let downloaded = crate::settings::get().update_cache.as_ref().and_then(|c| {
        if c.asset_name.is_empty() {
            return None;
        }
        let p = crate::clipboard::cache_dir().join("update").join(&c.asset_name);
        p.is_file().then(|| p.to_string_lossy().to_string())
    });
    // 跨版本升级：`upgraded_from` 是本次启动前运行的那一版，空表示首次安装
    let local_from = crate::settings::get().upgraded_from;
    let (local_notes, versions) = changelog_since(&local_from, cur);

    json!({
        "hasUpdate": is_newer(&rel.latest, cur),
        "latest": rel.latest,
        "current": cur,
        // 纯版本号（不带后缀）供比较，带后缀的展示号单独给 displayCurrent
        "displayCurrent": version_display(),
        "url": rel.url,
        "notes": rel.notes,
        "assetUrl": rel.asset_url,
        "assetName": rel.asset_name,
        "assetSize": rel.asset_size,
        "downloadedPath": downloaded,
        // 本机这版（**含跨过的中间版本**）的说明，随包内置。
        // 没有新版本时前端显示它，这样 beta / 未发布版本也能看到自己的更新日志；
        // 跨版本升级时一次列出中间所有节（见 `changelog_since`）。
        "localTag": cur,
        "localNotesFrom": local_from,
        // 节数给前端用：没有起点时标题写「及之前 N 个版本」，避免只写一个版本号
        // 却列了好几节
        "localNotesCount": versions.len(),
        "localNotes": local_notes,
    })
}

/// 把一次成功的结果记进 config.json：跨重启保留，设置红点与「最新版更新日志」都读它。
/// 内容没变就不写盘，免得每次启动都白写一遍配置文件。
fn save_cache(rel: &Release) {
    let next = crate::settings::UpdateCache {
        latest: rel.latest.clone(),
        notes: rel.notes.clone(),
        url: rel.url.clone(),
        asset_url: rel.asset_url.clone(),
        asset_name: rel.asset_name.clone(),
        asset_size: rel.asset_size,
        checked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let same = crate::settings::get()
        .update_cache
        .as_ref()
        .map_or(false, |c| {
            c.latest == next.latest
                && c.notes == next.notes
                && c.asset_url == next.asset_url
                && c.asset_size == next.asset_size
        });
    if same {
        return;
    }
    crate::settings::update(|cfg| cfg.update_cache = Some(next));
}

/// 启动时自动检查一次（背景线程，静默）。
///
/// 只有「确实有新版本」才通知前端；**检查失败或没有新版一律什么都不做**
/// （用户明确要求：这两种情况不要打扰）。
pub fn auto_check(app: tauri::AppHandle) {
    use tauri::Emitter;
    std::thread::spawn(move || {
        let rel = match fetch_release() {
            Ok(r) => r,
            Err(e) => {
                crate::log_print!("[update] 启动自检未完成（静默）：{e}");
                return;
            }
        };
        save_cache(&rel);
        if !is_newer(&rel.latest, env!("CARGO_PKG_VERSION")) {
            crate::log_print!("[update] 启动自检：已是最新（远端 {}）", rel.latest);
            return;
        }
        crate::log_print!("[update] 启动自检：发现新版本 {}", rel.latest);
        let _ = app.emit("update-available", payload(&rel));
    });
}

/// 手动检查更新。
#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    // 同步命令会跑在主线程上，而这里最坏要等「若干个代理 × 12 秒」——
    // 放阻塞线程池，界面全程不卡。
    let rel = tauri::async_runtime::spawn_blocking(fetch_release)
        .await
        .map_err(|e| e.to_string())??;
    save_cache(&rel);
    Ok(payload(&rel))
}

/// 读缓存的检查结果：前端启动时用它恢复红点与更新日志，不必再联网。
#[tauri::command]
pub fn update_state() -> serde_json::Value {
    match crate::settings::get().update_cache {
        Some(c) => payload(&Release {
            latest: c.latest,
            url: c.url,
            notes: c.notes,
            asset_url: c.asset_url,
            asset_name: c.asset_name,
            asset_size: c.asset_size,
        }),
        None => payload(&Release::default()),
    }
}

/// 清掉「已经装上了」的安装包。
///
/// 更新流程：下载到 `<缓存>/update/` → 拉起安装器 → 本进程退出。装完后版本号
/// 就追平了，那份包的唯一用途（供安装）已经结束，留在磁盘上只是垃圾。
///
/// 为什么不能只认 `asset_name`：它记的是**最近一次检查**的版本。若后来远端
/// 又发了新版，缓存里的 `asset_name` 就指向新包，旧包便再也没人管，越积越多。
/// 所以这里扫描整个目录，逐个比对，只删「不比当前版本新」的那几份。
pub fn prune_stale_installers() {
    let dir = crate::clipboard::cache_dir().join("update");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let cur = match parse_semver(env!("CARGO_PKG_VERSION")) {
        Some(v) => v,
        None => return,
    };

    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let Some(v) = parse_semver(&name) else {
            continue;
        };
        // 还比当前版本新 → 用户上次取消了安装，留着让他能继续装
        if v > cur {
            continue;
        }
        let _ = std::fs::remove_file(&p);
        crate::log_print!("[update] 清理安装包 v{}（已安装或过期）", name);
    }
}

/// 从文件名里取出第一组 `主.次.修` 版本号，取不到返回 None。
///
/// 安装包名形如 `轻贴_2.1.8_x64-setup.exe` / `light_clip_v2.1.8_setup.exe`，
/// 统一按「数字.数字.数字」扫描即可，不必区分命名风格。
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0usize;

    while i < n {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut parts = [0u32; 3];
        let mut k = 0usize;
        let mut j = i;

        loop {
            let mut v = 0u32;
            while j < n && b[j].is_ascii_digit() {
                v = v * 10 + (b[j] - b'0') as u32;
                j += 1;
            }
            parts[k] = v;
            k += 1;

            if k == 3 {
                return Some((parts[0], parts[1], parts[2]));
            }
            if j < n && b[j] == b'.' {
                j += 1;
            } else {
                break;
            }
        }
        i = if j > i { j } else { i + 1 };
    }
    None
}

/// 打开外部链接（项目主页 / 新建反馈 / 下载页）。
///
/// 走系统 shell 的 `open` 动词，只允许 http(s)，避免被塞进 `file://` 之类的本地路径。
///
/// **不能用 `explorer <url>` 来打开**：explorer.exe 解析命令行时会把 URL 里的 `?`
/// 当成路径，带查询串的链接根本送不到浏览器 —— 实测会退化成打开「文档」文件夹。
/// 轻贴的「问题反馈」按钮拼的是 `issues/new?title=…&body=…`，正好中这一枪。
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("仅允许打开 http/https 链接".into());
    }
    // ShellExecuteW 会等 shell 解析关联程序，放阻塞线程池里跑，别占住 IPC 线程
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let op = w("open");
        let file = w(&url);
        unsafe {
            ShellExecuteW(
                0,
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            )
        }
    })
    .await;
    // ShellExecuteW 的约定：返回值小于等于 32 表示失败
    let rc = joined.map_err(|e| e.to_string())?;
    if rc <= 32 {
        return Err(format!("系统里没有能打开链接的程序（错误码 {rc}）"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 下载安装包并运行
// ---------------------------------------------------------------------------

/// 同一时间只允许一个下载任务（重复点按钮不该开出两条 curl）。
static DL_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 下载最新安装包到缓存目录的 `update/`，全程用 `update-progress` 事件报进度。
///
/// 进度怎么来的：**轮询 `.part` 文件大小**，分母取 release JSON 里的
/// `assets[].size`。不去解析 curl 的进度条——那是本地化文本、格式随版本变，
/// 而文件大小是稳定可信的事实，跨语言跨版本都不受影响。
#[tauri::command]
pub async fn download_update(app: tauri::AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::sync::atomic::Ordering;
        if DL_RUNNING.swap(true, Ordering::SeqCst) {
            return Err("已有下载任务在进行中".into());
        }
        let r = download_inner(&app);
        DL_RUNNING.store(false, Ordering::SeqCst);
        r
    })
    .await
    .map_err(|e| e.to_string())?
}

fn download_inner(app: &tauri::AppHandle) -> Result<String, String> {
    use tauri::Emitter;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let cached = crate::settings::get()
        .update_cache
        .ok_or("请先点「检查更新」")?;

    // 校验用的「期望大小」必须**现取**，不能直接用缓存：发布方重新上传同名资产
    // （覆盖发布）之后，缓存里的 `asset_size` 就过期了 —— 于是文件其实下载成功、
    // 校验却必然报「文件大小不符」（实测遇到过一次）。
    // 取不到时（离线等）再退回缓存，反正那种情况下下载本身也大概率失败。
    let cache = match fetch_release() {
        Ok(rel) => {
            save_cache(&rel);
            rel
        }
        Err(e) => {
            crate::log_print!("[update] 下载前重新获取发布信息失败，改用缓存：{e}");
            Release {
                latest: cached.latest,
                notes: cached.notes,
                url: cached.url,
                asset_url: cached.asset_url,
                asset_name: cached.asset_name,
                asset_size: cached.asset_size,
            }
        }
    };
    if cache.asset_url.is_empty() {
        return Err("该版本没有安装包，请到 Release 页手动下载".into());
    }

    let dir = crate::clipboard::cache_dir().join("update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败：{e}"))?;

    let name = if cache.asset_name.is_empty() {
        "light_clip_setup.exe".to_string()
    } else {
        cache.asset_name.clone()
    };
    let final_path = dir.join(&name);
    let part = dir.join(format!("{name}.part"));

    // 清掉上次留下的半截文件与旧版本安装包（只保留本次的目标文件）
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p != final_path && p.is_file() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    let total = cache.asset_size;
    let mut last_err = String::new();

    for proxy in proxy_candidates_with_direct() {
        let _ = std::fs::remove_file(&part);

        let mut cmd = std::process::Command::new("curl");
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.args(["-sSL", "--fail", "--max-time", "1800", "--retry", "2", "-o"]);
        cmd.arg(&part);
        if !proxy.is_empty() {
            cmd.args(["-x", &proxy]);
        }
        cmd.arg(&cache.asset_url);

        log_append(&format!(
            "[{}] 开始下载 proxy={} -> {}",
            now_stamp(),
            if proxy.is_empty() { "直连" } else { &proxy },
            final_path.display()
        ));

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("启动 curl 失败：{e}");
                continue;
            }
        };

        let mut exit_ok = false;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let percent = if total > 0 {
                (got.min(total) as f64 * 100.0 / total as f64).round() as u32
            } else {
                0
            };
            let _ = app.emit(
                "update-progress",
                json!({ "got": got, "total": total, "percent": percent }),
            );
            match child.try_wait() {
                Ok(Some(st)) => {
                    exit_ok = st.success();
                    if !exit_ok {
                        last_err = format!("下载中断（curl 退出码 {:?}）", st.code());
                    }
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    last_err = format!("等待下载进程失败：{e}");
                    break;
                }
            }
        }

        let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        // 知道总大小时必须一分不差；不知道时只要求非空
        let size_ok = if total > 0 { got == total } else { got > 0 };
        if exit_ok && size_ok {
            if std::fs::rename(&part, &final_path).is_err() {
                last_err = "下载完成但重命名失败".into();
                continue;
            }
            remember_proxy(&proxy);
            let _ = app.emit(
                "update-progress",
                json!({ "got": got, "total": total, "percent": 100 }),
            );
            crate::log_print!("[update] 安装包已就绪：{}", final_path.display());
            return Ok(final_path.to_string_lossy().to_string());
        }
        if last_err.is_empty() {
            last_err = format!("下载不完整：期望 {total} 字节，实际 {got} 字节");
        }
    }

    Err(if last_err.is_empty() {
        "下载失败，请检查网络".into()
    } else {
        last_err
    })
}

#[link(name = "shell32")]
extern "system" {
    /// `runas` 动词会触发 UAC 提权。`std::process::Command` 直接 spawn 一个
    /// 需要管理员权限的程序只会拿到 ERROR_ELEVATION_REQUIRED，所以这里必须走
    /// shell 的 runas。
    fn ShellExecuteW(
        hwnd: isize,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        cwd: *const u16,
        show: i32,
    ) -> isize;
}

/// 运行已下载的安装包，然后退出自身。
///
/// 顺序要紧：**先拉起安装器，再退出自己**。安装器是 perMachine（需要管理员），
/// 而且要覆盖 `self-clipboard.exe` —— 自己正锁着它，nsis 模板里的
/// `CheckIfAppIsRunning` 也会因此拦下来。安装器是独立进程，
/// 父进程退出不影响它继续跑。
#[tauri::command]
pub fn install_downloaded(app: tauri::AppHandle, path: String) -> Result<(), String> {
    // 只允许运行我们自己下载目录里的文件：这个路径来自前端，
    // 不校验就等于开了一条「执行任意程序」的通道。
    let dir = crate::clipboard::cache_dir().join("update");
    let p = std::path::PathBuf::from(&path);
    if !p.starts_with(&dir) {
        return Err("安装包路径不合法".into());
    }
    if !p.is_file() {
        return Err("安装包不存在，可能已被删除".into());
    }

    let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let op = w("runas");
    let file = w(&path);
    let rc = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    // 返回值 > 32 才算成功（<= 32 全是错误码，含用户在 UAC 上点了「否」）
    if rc <= 32 {
        return Err(format!("无法启动安装程序（错误码 {rc}）"));
    }

    std::thread::spawn(move || {
        // 给安装器一点时间完成初始化再退出，免得它刚起来就判定「轻贴正在运行」
        std::thread::sleep(std::time::Duration::from_millis(1500));
        crate::idle_release::note_exiting();
        app.exit(0);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨版本升级要一次显示中间所有版本的说明。
    ///
    /// 只看当前那一节是不够的：用户从 2.3.0 升到 2.4.1，2.4.0 改了什么同样是
    /// 「这次更新带来的变化」。这条测试对着真实的 CHANGELOG 跑 —— 所以它同时
    /// 保证了「节解析没错行、没把下一个标题吃进来」。
    #[test]
    fn changelog_since_includes_intermediate_versions() {
        let (notes, versions) = changelog_since("2.3.0", "2.4.1");
        assert!(versions.contains(&"2.4.1".to_string()), "当前版本必须有: {versions:?}");
        assert!(versions.contains(&"2.4.0".to_string()), "中间跨过的版本必须有: {versions:?}");
        assert!(
            versions.contains(&"2.3.0".to_string()),
            "起点也要显示 —— 用户未必看过自己那版的说明: {versions:?}"
        );
        // 两节的正文都要在，且各自带标题，前端靠标题分段
        assert!(notes.contains("## 2.4.0"));
        assert!(notes.contains("## 2.4.1"));
    }

    /// 用户马上会遇到的场景：从 2.4.0 升到 2.4.1，两节都要给。
    #[test]
    fn changelog_since_one_version_jump_shows_both() {
        let (_, versions) = changelog_since("2.4.0", "2.4.1");
        assert_eq!(
            versions,
            vec!["2.4.1".to_string(), "2.4.0".to_string()],
            "跨一版也要把起点那版带上"
        );
    }

    /// 没有起点时退化为「最近 N 节」而不是只显示一节。
    ///
    /// 这是为**现有老用户**准备的：升级起点字段是本版才加的，他们配置里没有，
    /// 若不兜底，恰是最需要看更新的这批人只看到一屏说明。
    #[test]
    fn changelog_since_without_origin_falls_back_to_recent() {
        let (notes, versions) = changelog_since("", "2.4.1");
        assert_eq!(versions.first().map(|s| s.as_str()), Some("2.4.1"), "第一节必须是当前版本");
        assert!(versions.len() > 1, "老用户升级也要能看到前几版: {versions:?}");
        assert!(versions.len() <= FALLBACK_NOTE_SECTIONS);
        assert!(notes.contains("## 2.4.0"), "兜底时也要带上中间版本");
        // 不能把比当前版本更新的节混进来
        assert!(!versions.iter().any(|v| is_newer(v, "2.4.1")));
    }

    /// 节数有上限：从很老的版本升上来时全列会把关于页变成一篇长文。
    #[test]
    fn changelog_sections_are_capped() {
        let (_, versions) = changelog_since("0.0.1", "9.9.9");
        assert!(versions.len() <= MAX_NOTE_SECTIONS, "节数超上限: {}", versions.len());
        assert!(versions.len() > 1, "这条用例的前提是 CHANGELOG 里有多节");
    }

    /// 版本比较的两个坑：两位补丁号、预发布后缀。
    ///
    /// 按字符串比会把 `2.4.10` 判成小于 `2.4.9`，跨版本日志就会漏节。
    #[test]
    fn version_compare_handles_multi_digit_and_prerelease() {
        assert!(is_newer("2.4.10", "2.4.9"), "两位补丁号不能按字符串比");
        assert!(is_newer("2.4.1", "2.4.1-beta.2"), "正式版高于自己的预发布");
        assert!(!is_newer("2.4.1-beta.2", "2.4.1"), "预发布低于正式版");
        assert!(is_newer("2.4.0", "2.3.9"));
        assert!(!is_newer("2.4.0", "2.4.0"), "同版本不算更新");
    }
}
