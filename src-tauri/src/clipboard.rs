//! 剪贴板监听 / 读取 / 存储 / 写回。
//!
//! 设计红线（DESIGN.md 6.1 / 6.3.1）：
//!   - 事件驱动：仅响应 `WM_CLIPBOARDUPDATE`，绝不轮询
//!   - 廉价去重：`GetClipboardSequenceNumber` 判重 + 上一条指纹
//!   - 内容永不常驻内存：index 只存元数据，长文本/图片按 id 落盘分片

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

/// 主窗口 hwnd：写回剪贴板时 OpenClipboard 需要 owner 窗口
static MAIN_HWND: OnceLock<isize> = OnceLock::new();
/// AppHandle：emit 事件用
static APP: OnceLock<AppHandle> = OnceLock::new();
/// 存储目录（app_data_dir/clipboard）
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// 内存中的轻量索引（只存元数据，不存内容正文）
static HISTORY: OnceLock<Mutex<Vec<HistoryItem>>> = OnceLock::new();

/// 上一次处理过的剪贴板序列号（去重）
static LAST_SEQ: AtomicU32 = AtomicU32::new(0);
/// 本程序写回剪贴板后产生的序列号（应被忽略，避免自读）
static SKIP_SEQ: AtomicU32 = AtomicU32::new(0);
/// id 计数器，避免同毫秒碰撞
static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 最近一条图片条目的 (内容指纹, 时间戳ms)：微信等应用同图双写的判重依据。
/// 用指纹而非「宽高」——见 image_fingerprint 的说明。
static LAST_IMG: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// 连续因「同图」被拦截的次数。指纹判定不是万能的，连续命中多次说明
/// 很可能是固定区域重复截图这类正常操作，此时强制放行（安全阀）。
static DUP_STREAK: AtomicU32 = AtomicU32::new(0);
/// 剪贴板落盘工作线程的投递端。解码 / 写盘 / 生成缩略图都在那边做，
/// 主线程只负责把剪贴板里的字节拷出来就返回，见 spawn_worker。
static WORKER: OnceLock<std::sync::mpsc::Sender<RawClip>> = OnceLock::new();
/// 已投递给工作线程、尚未落盘完成的条数。退出前据此等待，避免「刚复制就退出」丢数据。
static PENDING: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: String,
    pub kind: String, // "text" | "image" | "files"
    pub ts: u64,
    /// 置顶：记录这条内容在**哪些分类**里被置顶。
    /// 取值与前端 `data-view` 一致："history" | "text" | "images" | "files" | "favorites"；
    /// 另有通配值 `"*"`（`PIN_ALL`），表示**所有分类**都置顶。
    ///
    /// 两种语义由设置项 `pin_global` 决定（默认关闭）：
    ///   - 关闭（默认）：**按分类独立** —— 在「文本」里置顶不影响它在「历史」里的位置。
    ///   - 开启：**互通** —— 整条内容只有一个置顶状态，任一分类置顶/取消其它分类同步。
    ///
    /// 互通模式下的置顶形如 `["*", "history"]`：通配项 `"*"` 是**同步置顶的永久标记**，
    /// 后面那个具体分类名是**来源**，记下用户当初是在哪个分类里置顶的。
    ///
    /// `"*"` 不会因为关掉互通而消失（除非子选项 `pin_cancel_on_disable` 要求取消），
    /// 所以「同步产生的置顶」一直认得出来：下次再开互通时它仍是同一条同步置顶，
    /// 不会退化成每个分类各一条独立置顶。见 `normalize_pins`。
    ///
    /// 注意 `rename = "pinnedViews"`：这个结构体同时用于 IPC 与 index.json 持久化，
    /// 整体加 camelCase 会让旧 index.json（text_path/img_path…）解析失败、丢掉数据，
    /// 所以只给这个新字段单独改名，保证前端拿到的直接是 pinnedViews。
    #[serde(default, rename = "pinnedViews")]
    pub pinned_views: Vec<String>,
    /// 兼容 v2.0 早期的**全局**置顶字段（已废弃）：只读不写，
    /// 载入时迁移成 pinned_views = ["history"]。字段名保留是为了还能解析旧 index.json。
    #[serde(default, skip_serializing)]
    pub pinned: bool,
    pub favorite: bool,
    /// 文本前若干字符 / "[图片]" / "N 个文件"
    pub preview: String,
    pub size: u64,
    /// 长文本落盘路径（相对 DIR/data/）
    pub text_path: Option<String>,
    /// 图片落盘路径（相对 DIR/images/）
    pub img_path: Option<String>,
    /// 文件路径清单落盘路径（相对 DIR/files/，每行一个完整路径）
    ///
    /// 和文本一样「内容落盘」：缓存目录里必须有这条记录的实体，否则用户打开缓存目录
    /// 会发现文件条目一条数据都没有。copy_back 以这个文件为唯一数据源。
    #[serde(default)]
    pub files_path: Option<String>,
    /// 文件路径列表（仅 files 类有）
    ///
    /// 保留在内存/index.json 里是刻意的：它只有几百字节，是**元数据**而不是内容，
    /// 用来做搜索（按文件名）和快速回写；真正的实体清单在 files_path 指向的文件里。
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// 对外命令
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_history() -> Vec<HistoryItem> {
    HISTORY
        .get()
        .map(|m| m.lock().unwrap().clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn delete_item(id: String) -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    if let Some(pos) = hist.iter().position(|i| i.id == id) {
        let item = hist.remove(pos);
        drop(hist);
        remove_item_files(&dir, &item);
        save_index(&dir, &HISTORY.get().unwrap().lock().unwrap());
    }
    Ok(())
}

/// `pinned_views` 里的通配项：**同步置顶的永久标记**，表示在所有分类里置顶。
/// 「置顶互通」开启时由 `toggle_pin` 写入；关掉互通后默认保留（见 `normalize_pins`）。
pub const PIN_ALL: &str = "*";

/// 切换置顶状态。返回改动后该条目的 `pinned_views`，前端直接用，不再自己推一遍。
///
/// 行为随设置项 `pin_global` 分两种：
///   - 互通：整条内容一个状态。未置顶 → `["*", 当前分类]`；已置顶 → 清空。
///   - 按分类独立（默认）：只有当前分类参与。
///       * 该条已带同步标记 `"*"` 时说明它是互通产生的**同步置顶**，而同步置顶只有
///         「有 / 没有」两个状态 —— 取消即整条取消，不是只摘掉当前分类；
///       * 否则按普通置顶处理，只增删 `view` 这一项。
#[tauri::command]
pub fn toggle_pin(id: String, view: String) -> Result<Vec<String>, String> {
    let global = crate::settings::get().pin_global;
    let mut out: Vec<String> = Vec::new();
    patch_item(&id, |i| {
        if global {
            if i.pinned_views.is_empty() {
                i.pinned_views = vec![PIN_ALL.to_string(), view.clone()];
            } else {
                i.pinned_views.clear();
            }
        } else if i.pinned_views.iter().any(|v| v == PIN_ALL) {
            // 这是一条**同步置顶**（互通产生的）。同步置顶只有「有」和「没有」两个状态，
            // 取消就是整条取消：不能只摘掉当前分类 —— 那样按一次「取消」它在别的分类
            // 仍然置顶，用户只能一个分类一个分类地取消。
            i.pinned_views.clear();
        } else if let Some(pos) = i.pinned_views.iter().position(|v| v == &view) {
            i.pinned_views.remove(pos);
        } else {
            i.pinned_views.push(view.clone());
        }
        out = i.pinned_views.clone();
    })?;
    Ok(out)
}

/// 切换「置顶互通」开关后，把已有记录归一到新模式的表示。
///
///   - 开启互通：给所有已置顶的条目补上通配项 `"*"`；**原有的具体分类名保留**，
///     它们记着来源，关掉互通时能原样回落。
///   - 关闭互通：
///       * `cancel_propagated = true`（「关闭时取消同步置顶」，默认开）→ 删掉 `"*"`，
///         同步置顶随开关一起取消，置顶回到当初那个分类；来源没记下的旧数据回落到「历史」。
///       * `cancel_propagated = false` → **原样保留 `"*"`**。同步置顶是永久标记：关掉
///         互通后它继续在各分类置顶，下次再开互通时仍是同一条同步置顶，不会退化成
///         每个分类各一条独立置顶（那样只能逐条手动取消）。
pub fn normalize_pins(global: bool, cancel_propagated: bool) {
    let (Some(dir), Some(hist)) = (DIR.get().cloned(), HISTORY.get()) else {
        return;
    };
    let mut hist = hist.lock().unwrap();
    for i in hist.iter_mut() {
        if global {
            let has_all = i.pinned_views.iter().any(|v| v == PIN_ALL);
            if !i.pinned_views.is_empty() && !has_all {
                i.pinned_views.insert(0, PIN_ALL.to_string());
            }
            continue;
        }
        if !i.pinned_views.iter().any(|v| v == PIN_ALL) {
            continue;
        }
        if !cancel_propagated {
            // 保留同步置顶：`"*"` 就是这个永久标记，不动它。
            continue;
        }
        i.pinned_views.retain(|v| v.as_str() != PIN_ALL);
        if i.pinned_views.is_empty() {
            // 来源分类没记下来（互通时代置顶的旧数据）→ 回落到「历史」：
            // 历史是默认分类，也是「全局置顶」时代迁移的目标，丢掉这条置顶不合理。
            i.pinned_views.push("history".to_string());
        }
    }
    save_index(&dir, &hist);
}

#[tauri::command]
pub fn toggle_favorite(id: String) -> Result<(), String> {
    patch_item(&id, |i| i.favorite = !i.favorite)
}

#[tauri::command]
pub fn clear_all() -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    // 只删条目自己的实体（文本 / 图片 / 文件清单），不碰目录里其它内容——
    // 尤其不碰 `log/`（见 CONTENT_DIRS 注释）。
    for item in hist.iter() {
        remove_item_files(&dir, item);
    }
    hist.clear();
    save_index(&dir, &hist);
    Ok(())
}

/// get_image 的硬上限。正常流程前端只用缩略图（get_image_thumb），
/// 原图走这里只剩历史遗留调用；超限直接拒绝——2.1.10 的 OOM 白屏就是
/// 把上百 MB 的 BMP base64 进前端所致。拒绝后前端显示占位，页面仍可用
/// （存量巨图条目因此不再拖垮整个窗口）。
const MAX_IMAGE_BYTES: u64 = 96 * 1024 * 1024;

#[tauri::command]
pub fn get_image(id: String) -> Result<String, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let hist = HISTORY.get().unwrap().lock().unwrap();
    let item = hist.iter().find(|i| i.id == id).ok_or("条目不存在")?;
    let rel = item.img_path.as_ref().ok_or("该条目不是图片")?;
    let path = dir.join(rel);
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    drop(hist);
    if size > MAX_IMAGE_BYTES {
        crate::logger::note_error(
            "clipboard",
            &format!("get_image 拒绝 {}：{}MB 超上限（考虑删除该条目）", id, size / 1048576),
        );
        return Err(format!("图片过大（{}MB），无法在界面显示", size / 1048576));
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let b64 = base64_encode(&bytes);
    Ok(format!("data:image/bmp;base64,{}", b64))
}

/// 缩略图：列表与悬浮预览专用（PNG，长边 ≤512，单张几十 KB）。
/// 返回 data URL；失败返回 Err，前端显示占位图。
#[tauri::command]
pub fn get_image_thumb(id: String) -> Result<String, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let item = {
        let hist = HISTORY.get().unwrap().lock().unwrap();
        hist.iter().find(|i| i.id == id).cloned().ok_or("条目不存在")?
    };
    match ensure_thumb(&dir, &item) {
        Some(png) => {
            let b64 = base64_encode(&png);
            Ok(format!("data:image/png;base64,{}", b64))
        }
        None => Err("缩略图不可用".into()),
    }
}

/// 把历史条目内容写回系统剪贴板（文本 / 图片 / 文件列表）。
///
/// 返回 Ok(Some(提示)) 时前端用该提示替代默认的「已复制到剪贴板」——
/// 目前只有文件条目会用到（部分原文件已失效、已跳过）。
#[tauri::command]
pub fn copy_back(id: String) -> Result<Option<String>, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let hist = HISTORY.get().unwrap().lock().unwrap();
    let item = hist.iter().find(|i| i.id == id).cloned().ok_or("条目不存在")?;
    drop(hist);

    match item.kind.as_str() {
        "text" => {
            let text = match &item.text_path {
                Some(rel) => fs::read_to_string(dir.join(rel)).map_err(|e| e.to_string())?,
                None => item.preview.clone(),
            };
            unsafe { write_text_to_clipboard(&text) }.map(|_| None)
        }
        "files" => {
            // 以落盘清单为唯一数据源（与文本条目走 text_path 一致）；
            // 旧版 index.json 把路径内联在 files 里、没有 files_path，回退到内存中的 files。
            let files: Vec<String> = match &item.files_path {
                Some(rel) => fs::read_to_string(dir.join(rel))
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
                None => item.files.clone().ok_or("文件列表缺失")?,
            };
            if files.is_empty() {
                return Err("文件列表为空".into());
            }
            // —— 失效检测 ——
            // 文件条目存的是**路径引用**而不是文件副本，原文件被移动/改名/删除后就粘不出来了。
            // 全部失效直接报错；部分失效就跳过失效的那几个，并把情况带回前端提示。
            let total = files.len();
            let alive: Vec<String> = files
                .into_iter()
                .filter(|p| std::path::Path::new(p).exists())
                .collect();
            if alive.is_empty() {
                return Err(format!(
                    "原文件都已不存在（{total} 个），可能被移动或删除"
                ));
            }
            unsafe { write_files_to_clipboard(&alive)? }
            let missing = total - alive.len();
            Ok(if missing > 0 {
                Some(format!(
                    "已复制 {} 个文件（跳过 {missing} 个失效文件）",
                    alive.len()
                ))
            } else {
                None
            })
        }
        "image" => {
            // BMP 文件去掉 14 字节文件头即为 DIB，直接放回 CF_DIB
            let rel = item.img_path.ok_or("图片文件缺失")?;
            let bmp = fs::read(dir.join(rel)).map_err(|e| e.to_string())?;
            if bmp.len() < 54 || &bmp[0..2] != b"BM" {
                return Err("图片数据损坏".into());
            }
            unsafe { write_dib_to_clipboard(&bmp[14..]) }.map(|_| None)
        }
        _ => Err("未知类型".into()),
    }
}

/// 失效检测（前端列表标记用）：传入条目 id，返回其中**至少有一个原路径已不存在**的 id。
///
/// 只做 `Path::exists()`（纯元数据查询，不读文件内容），几十条也是微秒级，
/// 所以可以在每次渲染后调用。
#[tauri::command]
pub fn check_files_alive(ids: Vec<String>) -> Vec<String> {
    let dir = DIR.get().cloned().unwrap_or_default();
    let Some(hist) = HISTORY.get() else {
        return Vec::new();
    };
    let hist = hist.lock().unwrap();
    let mut dead = Vec::new();
    for id in ids {
        let Some(item) = hist.iter().find(|i| i.id == id) else {
            continue;
        };
        if item.kind != "files" {
            continue;
        }
        let paths: Vec<String> = match &item.files_path {
            Some(rel) => fs::read_to_string(dir.join(rel))
                .unwrap_or_default()
                .lines()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            None => item.files.clone().unwrap_or_default(),
        };
        if paths.is_empty() || paths.iter().any(|p| !std::path::Path::new(p).exists()) {
            dead.push(id);
        }
    }
    dead
}

#[tauri::command]
pub fn delete_items(ids: Vec<String>) -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    let mut removed = 0usize;
    hist.retain(|i| {
        if ids.iter().any(|id| id == &i.id) {
            remove_item_files(&dir, i);
            removed += 1;
            false
        } else {
            true
        }
    });
    drop(hist);
    save_index(&dir, &HISTORY.get().unwrap().lock().unwrap());
    crate::log_print!("[clipboard] 批量删除 {removed} 条");
    Ok(())
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 启动剪贴板落盘工作线程。
///
/// 为什么要有它：`on_clipboard_update` 跑在 WndProc 里，也就是**主线程**。
/// 而落盘一张大图要经历「解码 → 展开成 RGBA → 缩放到上限 → 拼 BMP → 写盘」，
/// 手机原图能到 200MB 像素数据、耗时数秒。同步做就是界面卡死，用户看到的
/// 是「复制大图后软件卡死等待」。
///
/// 主线程只做「OpenClipboard → 拷贝字节 → CloseClipboard」（纯 memcpy），
/// 之后把字节所有权整个移交给这里，解码等重活在后台慢慢跑，跑完再 emit。
/// 单线程串行消费，历史顺序不会被打乱。
/// 计数守卫：析构时把 PENDING 减一。配合 RAII，任何退出路径都不会漏减。
struct PendingGuard;
impl Drop for PendingGuard {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
}

fn spawn_worker() {
    // app 走全局 APP（OnceLock，在 init 首行写入）；worker 在订阅监听之前
    // 启动，所以取用时必定已就绪。
    let (tx, rx) = std::sync::mpsc::channel::<RawClip>();
    std::thread::Builder::new()
        .name("clipboard-store".into())
        .spawn(move || {
            while let Ok(raw) = rx.recv() {
                // guard 保证 PENDING 一定减回去 —— handle_clip 里判重拦截会
                // 提前 return，手写 fetch_sub 迟早漏掉一次。
                let _guard = PendingGuard;
                // handle_clip 内部完成：解码 → 同图判重 → 落盘 → emit。
                // 这里只负责串行投递，保证与复制顺序一致。
                handle_clip(raw);
            }
        })
        .map_err(|e| crate::log_warn!("[clipboard] 落盘工作线程启动失败：{e}"))
        .ok();
    let _ = WORKER.set(tx);
}

pub fn init(app: &AppHandle) {
    let _ = APP.set(app.clone());
    if let Some(w) = app.get_webview_window("main") {
        if let Ok(h) = w.hwnd() {
            let _ = MAIN_HWND.set(h.0 as isize);
        }
    }

    // 目录优先级：用户自定义缓存目录 > app_data_dir/clipboard（同 v1 imageCacheDir）
    let dir = match crate::settings::get().cache_dir {
        Some(d) => PathBuf::from(strip_ext_prefix(&d)),
        None => app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| PathBuf::from("./clipboard-data"))
            .join("clipboard"),
    };

    let _ = fs::create_dir_all(dir.join("data"));
    let _ = fs::create_dir_all(dir.join("images"));
    let _ = fs::create_dir_all(dir.join("files"));
    let _ = DIR.set(dir.clone());

    let mut hist = load_index(&dir);
    // 老版本 index.json 把文件路径内联在 files 里、没有落盘清单 → 补一份，
    // 让历史文件条目在缓存目录里也有实体（返回补了几条）
    let backfilled = backfill_files(&dir, &mut hist);
    if backfilled > 0 {
        crate::log_print!("[clipboard] 已为 {backfilled} 条历史文件条目补写 files/ 清单");
        save_index(&dir, &hist);
    }

    // 保留策略：按配置天数清理过期条目（0 = 永久保留）
    let days = crate::settings::get().retention_days;
    let hist = prune_history(hist, &dir, days);

    let _ = HISTORY.set(Mutex::new(hist));

    // 工作线程必须在订阅监听**之前**就位：否则第一批 WM_CLIPBOARDUPDATE
    // 到达时 WORKER 还没初始化，会退化成主线程同步落盘。
    spawn_worker();

    // 订阅剪贴板格式监听：此后系统剪贴板变化会向我们窗口发 WM_CLIPBOARDUPDATE
    unsafe {
        if AddClipboardFormatListener(MAIN_HWND.get().copied().unwrap_or(0)) == 0 {
            crate::log_warn!("[clipboard] AddClipboardFormatListener 失败");
        } else {
            crate::log_print!("[clipboard] 已订阅 WM_CLIPBOARDUPDATE（事件驱动监听）");
        }
    }

    crate::log_print!(
        "[clipboard] 初始化完成，历史条目 {} 条，目录 {}",
        HISTORY.get().unwrap().lock().unwrap().len(),
        dir.display()
    );
}

// ---------------------------------------------------------------------------
// 被钩子调用：剪贴板变化入口
// ---------------------------------------------------------------------------

/// 由 WndProc 子类在收到 WM_CLIPBOARDUPDATE 时调用。
pub fn on_clipboard_update() {
    unsafe {
        let seq = GetClipboardSequenceNumber();
        if seq == 0 || seq == LAST_SEQ.load(Ordering::Relaxed) {
            return;
        }
        // 剪贴板 owner 就是我们自己窗口 → 这是「点击条目复制回去」写下的内容，不能再当成
        // 一次新的系统复制回灌成重复条目。
        // 为什么不能只靠 mark_skip() 的序号比对：copy_back 跑在 Tauri 的工作线程上，
        // 而 WM_CLIPBOARDUPDATE 是投递到主线程的——主线程完全可能在 mark_skip() 执行之前
        // 就处理掉这条消息。owner 由系统在 SetClipboardData 时原子设置，没有这个窗口期。
        let me = MAIN_HWND.get().copied().unwrap_or(0);
        if me != 0 && GetClipboardOwner() == me {
            LAST_SEQ.store(seq, Ordering::Relaxed);
            return;
        }
        if seq == SKIP_SEQ.load(Ordering::Relaxed) {
            SKIP_SEQ.store(0, Ordering::Relaxed);
            LAST_SEQ.store(seq, Ordering::Relaxed);
            return;
        }
        LAST_SEQ.store(seq, Ordering::Relaxed);
    }

    let raw = unsafe { read_clipboard() };
    let Some(raw) = raw else { return };

    // 交给后台线程：解码（慢）+ 同图去重 + 落盘 + 通知前端。
    //
    // 为什么必须在后台：解码一张 8000×6000 的原图要展开近 200MB 像素再缩放，
    // 耗时可达数秒。而 WM_CLIPBOARDUPDATE 是在 **WndProc 主线程**上处理的，
    // 在这里同步解码等于把整个界面（含窗口拖动、按钮响应）冻住 —— 这就是
    // 2.1.10「微信复制大图后软件卡死等待」的成因。
    //
    // 主线程现在只做一次 memcpy（把剪贴板里的字节拷出来），立刻返回；
    // 解码、缩略图、写盘、emit 全在工作线程完成。单工作线程串行消费，
    // 历史顺序与复制顺序保持一致。
    if let Some(tx) = WORKER.get() {
        if tx.send(raw).is_ok() {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // 工作线程还没起来（正常不会发生——init 里先启线程再订阅监听）
        handle_clip(raw);
    }
}

/// 等待工作线程把手头的条目落盘（最多等 timeout）。
///
/// 异步化带来的新风险：复制完立刻退出时，工作线程可能还在解码大图，
/// 进程一退就丢了这条记录。所以在退出流程里先等一小会儿。
pub fn flush_pending(timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    while PENDING.load(Ordering::Relaxed) > 0 {
        if start.elapsed() >= timeout {
            crate::log_warn!(
                "[clipboard] 退出等待超时，仍有 {} 条未落盘",
                PENDING.load(Ordering::Relaxed)
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// 工作线程里跑的一整条落盘流水线。
///
/// 顺序很重要：先 `resolve`（解码），再做图片指纹判重 —— 指纹要基于像素
/// 内容算，未解码前拿不到。
fn handle_clip(raw: RawClip) {
    let Some(raw) = resolve(raw) else { return };

    // 图片双写判重：微信复制一张图会**分两次**改写剪贴板（先放 CF_HDROP 的
    // 临时图片文件，再放 CF_DIB 位图），每次序号都变，序号判重拦不住 →
    // 同一张图出现两条。判据是「像素内容指纹一致 + 间隔很短」，
    // 详见 image_fingerprint 的注释。
    if let RawClip::Image(dib) = &raw {
        let now = now_ms();
        let fp = image_fingerprint(dib);
        let dup = match LAST_IMG.lock().ok().and_then(|g| *g) {
            Some((lfp, lt)) => lfp == fp && now - lt < SAME_IMAGE_WINDOW_MS,
            None => false,
        };
        if dup {
            let streak = DUP_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
            // 安全阀：指纹判定不是万能的。若连续拦到 3 次，说明更可能是
            // 「在固定区域反复截到画面相同的图」这类正常操作，用户本意就是要
            // 再存一条 —— 此时无条件放行。宁可多一条历史，也不能让用户
            // 复制了却存不进去（这正是 2.1.10 反馈里最难受的一点）。
            if streak < 3 {
                crate::logger::write_line(
                    "INFO",
                    "clipboard",
                    &format!("同图双写拦截：像素指纹一致（连续第 {streak} 次）"),
                );
                return;
            }
            crate::logger::write_line(
                "WARN",
                "clipboard",
                "同图判定连续触发 3 次，本次强制放行（疑似固定区域重复截图，非程序双写）",
            );
        }
        DUP_STREAK.store(0, Ordering::Relaxed);
        if let Ok(mut g) = LAST_IMG.lock() {
            *g = Some((fp, now));
        }
    }

    let item = store_raw(raw);
    if let Some(app) = APP.get() {
        let _ = app.emit("clipboard-update", item);
    }
}

/// 把「待处理」的剪贴板内容整理成可存储形态：
/// 图片文件解码成 DIB、超大位图降采样到 MAX_SIDE 以内。
///
/// 这是整条链路上唯一耗时的部分（解码大图要展开上百 MB 像素），
/// 所以只在工作线程里调用，绝不能回到主线程。
fn resolve(raw: RawClip) -> Option<RawClip> {
    match raw {
        RawClip::ImageFile(path) => {
            // 微信给的临时图片文件随时可能被它自己清掉。解码推迟到后台线程后，
            // 从「读到路径」到「真正打开文件」之间隔了一段（主线程还要处理后续
            // 消息），这个窗口足够让文件消失 —— 于是图片莫名其妙就没了。
            // 找不到文件时重试几次再放弃。
            let mut dib = None;
            for attempt in 0..3u32 {
                dib = image_file_to_dib(&path);
                if dib.is_some() {
                    break;
                }
                if attempt < 2 {
                    crate::logger::write_line(
                        "INFO",
                        "clipboard",
                        &format!(
                            "临时图片文件暂不可读（{}），重试 {}/2",
                            path.display(),
                            attempt + 1
                        ),
                    );
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            }
            match dib {
                Some(dib) => Some(RawClip::Image(dib)),
                // 确实读不出来：不是图片 / 文件已损坏或被清理
                // → 退回按「文件」记录，与改动前行为一致。
                None => Some(RawClip::Files(vec![path.to_string_lossy().to_string()])),
            }
        }
        RawClip::ImageRaw(bytes) => Some(RawClip::Image(clamp_dib(bytes))),
        other => Some(other),
    }
}

// ---------------------------------------------------------------------------
// 读取剪贴板
// ---------------------------------------------------------------------------

enum RawClip {
    Text(String),
    Image(Vec<u8>), // DIB 字节
    Files(Vec<String>),
    /// 待解码的图片文件（CF_HDROP 只给了一张）。
    /// 解码要展开上百 MB 像素，很慢 —— 只在后台线程 resolve 里做。
    ImageFile(PathBuf),
    /// 待处理的原始 CF_DIB 字节（可能超过尺寸上限）。
    /// 降采样同样只在后台线程做。
    ImageRaw(Vec<u8>),
}

/// 尝试打开剪贴板，失败则短暂退避后重试。
///
/// 剪贴板锁是全局独占的，而 WM_CLIPBOARDUPDATE 偏偏在对方刚改写完就发过来 ——
/// 大概率正撞上对方还持锁的窗口。这种「拿不到锁」不是「没有内容」，重试即可。
///
/// 退避 2/4/8/16ms 递增，总计约 30ms：对一次正常复制完全感知不到（真正的
/// 耗时在后台解码），却足以覆盖绝大多数同进程内的分次写入。上限刻意压得低，
/// 因为这个函数跑在主线程（WndProc），拖久了界面会卡。
fn open_clipboard_retry() -> bool {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 0..MAX_ATTEMPTS {
        if unsafe { OpenClipboard(0) } != 0 {
            return true;
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(2u64 << attempt));
        }
    }
    false
}

unsafe fn read_clipboard() -> Option<RawClip> {
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;

    // 「文件复制」开关（设置页，默认关闭）：关掉后完全不看 CF_HDROP。
    // 在 OpenClipboard 之前读取，避免持有剪贴板锁时再去碰 settings 的互斥锁。
    let file_copy = crate::settings::get().file_copy;

    // ⚠️ 剪贴板是**全局独占锁**，任何进程持锁期间别人都拿不到。
    //
    // 微信/QQ/浏览器复制图片时会**分多次**改写剪贴板（先写位图，再补文件列表
    // 或反之），而 WM_CLIPBOARDUPDATE 是在每次改写后立刻发给订阅方的 —— 也就是
    // 说我们极可能在它还持着锁的时候就被叫醒。此时 OpenClipboard 直接失败。
    //
    // 改动前这里是一个 `if OpenClipboard(0) == 0 { return None }`：拿不到锁就
    // 当作「这次没有内容」，可 WM_CLIPBOARDUPDATE 只有一次通知，不会重发 →
    // 这一次复制就**永久丢失**。用户看到的就是「微信复制图片有时候完全没反应」
    // （2.1.11 反馈的新问题）。
    //
    // 正确做法是重试：拿不到锁不代表剪贴板是空的，只是对方还没写完。短暂退避
    // 后重试，覆盖住整个写入窗口。
    if !open_clipboard_retry() {
        crate::logger::write_line(
            "WARN",
            "clipboard",
            "OpenClipboard 重试耗尽，放弃本次剪贴板内容（对方应用长时间持锁）",
        );
        return None;
    }

    let result = (|| {
        // 优先级：文本 > 文件 > 图片
        if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
            if let Some(h) = non_null(GetClipboardData(CF_UNICODETEXT)) {
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    return None;
                }
                let mut v = Vec::new();
                let mut i = 0;
                loop {
                    let c = *p.add(i);
                    if c == 0 {
                        break;
                    }
                    v.push(c);
                    i += 1;
                }
                GlobalUnlock(h);
                return Some(RawClip::Text(String::from_utf16_lossy(&v)));
            }
        }

        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            if let Some(h) = non_null(GetClipboardData(CF_HDROP)) {
                const ALL: u32 = 0xFFFFFFFF;
                let count = DragQueryFileW(h, ALL, std::ptr::null_mut(), 0);
                let mut files = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0) + 1;
                    let mut buf = vec![0u16; len as usize];
                    DragQueryFileW(h, i, buf.as_mut_ptr(), len);
                    files.push(String::from_utf16_lossy(&buf[..buf.len() - 1]));
                }
                // ⚠️ 这里**绝对不能**调用 DragFinish(h)。
                //
                // h 来自 GetClipboardData(CF_HDROP)，它的内存归**剪贴板**所有（不是
                // WM_DROPFILES 里那个临时 HDROP）。DragFinish 会把它 GlobalFree 掉，而剪贴板
                // 仍然持有同一个句柄，于是下一次 OpenClipboard()/EmptyClipboard() 会二次释放
                // → 堆损坏 → 进程直接崩溃且无任何 Rust 层报错。
                //
                // 实证（症状逐字吻合）：
                //   Appeon 社区 “Clipboard and DragQueryFile - Need DragFinish?”：
                //   “Calling DragFinish() will cause the application to crash on the next
                //    OpenClipboard() call … I ended up just calling EmptyClipboard() with no
                //    DragFinish() and then the crash went away.”
                //   zabkat shellFAQ 亦建议剪贴板 HDROP 不要调用 DragFinish（只适用于
                //   WM_DROPFILES 处理）。
                // 对应到轻贴的现象：复制文件（读一次）→ 之后点击文件条目（写回 → OpenClipboard）
                // → 崩溃。这正是本次上报的 bug。
                // 内存无需手动释放：CloseClipboard 后由剪贴板自身管理。
                //
                // ⚠️ 单文件图片的判定**不能**被「文件复制」开关挡住（2.1.11 反馈：
                // 「微信复制图片都有可能识别不到」）。微信/QQ 复制图片给的往往就是
                // CF_HDROP（一个临时图片文件路径）而不是 CF_DIB，而 file_copy 默认
                // 关闭 —— 改动前这里整段挂在 `file_copy &&` 后面，于是微信复制图片
                // 整条路径被跳过，什么都读不到。图片和文件是两回事：开关管的是
                // 「要不要记录文件条目」，不该决定「要不要记录图片」。
                if files.len() == 1 {
                    // 只记住路径，解码推迟到后台线程 —— 主线程（WndProc）不能
                    // 被解码大图拖住，否则界面直接冻住。
                    return Some(RawClip::ImageFile(PathBuf::from(&files[0])));
                }
                // 多张 / 非图片：属于「文件复制」范畴，听开关的。
                if file_copy {
                    return Some(RawClip::Files(files));
                }
                // 单个非图片文件且开关关闭：不记录。
                return None;
            }
        }

        if IsClipboardFormatAvailable(CF_DIB) != 0 {
            if let Some(h) = non_null(GetClipboardData(CF_DIB)) {
                let size = GlobalSize(h);
                let p = GlobalLock(h);
                if !p.is_null() && size > 0 {
                    let bytes = std::slice::from_raw_parts(p as *const u8, size as usize).to_vec();
                    GlobalUnlock(h);
                    // 与 image_file_to_dib 同一上限：CF_DIB 也可能来自超大位图
                    // （微信第二次写入、专业软件复制），超限降采样，防巨型 BMP 落盘。
                    // 降采样推迟到后台线程，主线程只做完这次 memcpy 就返回。
                    return Some(RawClip::ImageRaw(bytes));
                }
            }
        }
        None
    })();

    CloseClipboard();
    result
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

/// 把一个图片文件解码成 DIB 字节（= BMP 掐掉 14 字节文件头），供转成图片条目。
///
/// 微信/QQ 等「复制图片」放进剪贴板的是 CF_HDROP（临时 PNG/JPG 文件）而非 CF_DIB；
/// 条目存储管线吃的是 DIB（`store_raw` 的 Image 分支用 `dib_to_bmp` 落盘）。
/// 这里统一：image 库解码 → 手工拼 32bpp BI_RGB 的 DIB —— 与 CF_DIB 读到的
/// 字节形态完全一致，下游（落盘/缩略图/回贴）零改动。解码失败返回 None，
/// 调用方退回按「文件」记录，不影响原行为。
///
/// ⚠️ 尺寸上限：手机原图/高清截图可达 8000×6000，DIB 未压缩即 190MB，
/// 落盘 BMP 同量级，前端一旦读取（base64 后再放大 1.33 倍）直接打爆 WebView2
/// —— 2.1.10 的「内存不足，无法打开此页面」即此因。超过上限按比例降采样，
/// 屏幕上并看不出差别；上限取 4096 长边（≈16.7M 像素、BMP 67MB 封顶）。
const MAX_SIDE: u32 = 4096;

/// 同图双写的时间窗口（毫秒）。
///
/// 微信复制一张图会在**毫秒级**内连写两次剪贴板；而人重复截图，哪怕动作很
/// 快（截图→粘贴→再截图）也在 1 秒量级。取 800ms 把两者分开。
const SAME_IMAGE_WINDOW_MS: u64 = 800;

/// 对 DIB 的像素内容做量化采样，得出一个 64 位指纹。
///
/// **为什么不用「宽高相同」判重**：固定区域连续截图（位置相近、面积差不多）
/// 宽高必然一模一样，仅比尺寸会把用户的正常操作误判成双写 —— 后果是「复制
/// 了却存不进历史、也粘贴不出来」，这就是 2.1.10 反馈的问题。改成比像素内容
/// 后，画面不同的两张图即使尺寸相同也不会误拦。
///
/// **为什么采样前先 >>3**：微信两次写出虽是同一张图，但走了不同路径（一次
/// 从 PNG 临时文件解码、一次直接从 CF_DIB 位图取），重采样后像素值可能差个
/// 位数。丢掉低 3 位后这类微小差异被抹平，指纹仍然一致。
fn image_fingerprint(dib: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut h = FNV_OFFSET;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    };

    if dib.len() < 40 {
        // 头部都不完整，退化为按内容粗略取指纹（不至于误拦，只可能漏拦）
        for b in dib.iter().take(64) {
            mix(*b);
        }
        return h;
    }

    // 宽高参与：尺寸不同必然不是同一张图
    for b in &dib[4..12] {
        mix(*b);
    }

    // 像素区起点 = BITMAPINFOHEADER(40) + 调色板（≤8bpp 时存在）
    let bpp = u16::from_le_bytes([dib[14], dib[15]]) as u32;
    let clr_used = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]);
    let palette = if bpp <= 8 {
        (if clr_used > 0 { clr_used } else { 1u32 << bpp }) * 4
    } else {
        0
    };
    let off = (40 + palette as usize).min(dib.len());
    let px = &dib[off..];
    let n = px.len();
    if n > 0 {
        // 均匀取约 128 个采样点，覆盖整张图而不只是开头几行
        let step = (n / 128).max(1);
        let mut i = 0usize;
        while i < n {
            mix(px[i] >> 3);
            i += step;
        }
    }
    h
}

fn image_file_to_dib(path: &std::path::Path) -> Option<Vec<u8>> {
    const EXTS: [&str; 6] = ["png", "jpg", "jpeg", "bmp", "gif", "webp"];
    // 扩展名只用来「快速排除明显不是图片的路径」（如 .txt / .pdf），**不能**作为
    // 唯一的准入条件 —— 微信/QQ 的剪贴板临时图片文件常常没有扩展名
    // （形如 `wx_image_tmp`、`{guid}`），改动前按扩展名一刀切会让这些图片
    // 解码失败、退回成「文件」条目，用户反馈就是「微信复制图片识别不到」。
    // 所以有扩展名时先判；扩展名不认识或干脆没有，则交给 image 库按文件头
    // 嗅探（image::open 会验证 magic number，不是图片自然失败）。
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        let looks_like_other = !ext.is_empty()
            && !EXTS.contains(&ext.as_str())
            && !matches!(ext.as_str(), "tmp" | "dat" | "bin" | "");
        if looks_like_other {
            return None;
        }
    }
    let img = image::open(path).ok()?;
    let (w0, h0) = (img.width(), img.height());
    // thumbnail() 只缩不放：未超限的图原样通过
    let img = if w0.max(h0) > MAX_SIDE {
        crate::logger::write_line(
            "INFO",
            "clipboard",
            &format!("图片 {} 超 {}px，降采样 {}×{} 存储", path.display(), MAX_SIDE, w0, h0),
        );
        img.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return None;
    }

    // 手工拼 DIB：BITMAPINFOHEADER(40) + 像素（自下而上、BGRA、32bpp、BI_RGB）。
    // 不借 image 的 BMP 编码器再掐头，是因手拼 20 行就能保证与 CF_DIB 的字节
    // 形态一字不差，也避开一层编码器实现差异。
    let pixel_bytes = (w as usize) * (h as usize) * 4;
    let mut dib = Vec::with_capacity(40 + pixel_bytes);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight，正值 = 自下而上
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    for y in (0..h).rev() {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            dib.push(p[2]);
            dib.push(p[1]);
            dib.push(p[0]);
            dib.push(p[3]);
        }
    }
    Some(dib)
}

fn store_raw(raw: RawClip) -> HistoryItem {
    let dir = DIR.get().unwrap().clone();
    let ts = now_ms();
    let id = make_id(ts);
    let mut item = HistoryItem {
        id: id.clone(),
        kind: String::new(),
        ts,
        pinned: false,
        pinned_views: Vec::new(),
        favorite: false,
        preview: String::new(),
        size: 0,
        text_path: None,
        img_path: None,
        files_path: None,
        files: None,
    };

    match raw {
        RawClip::Text(text) => {
            item.kind = "text".into();
            item.size = text.len() as u64;
            let first_line = text.lines().next().unwrap_or("").trim().to_string();
            item.preview = if text.trim().is_empty() {
                "[空白文本]".into()
            } else {
                first_line.chars().take(120).collect()
            };
            // 无论长短都落盘全文：内存只保留截断后的 preview 摘要，
            // 这样既遵守「内容不走内存」红线，复制回去时也不会被截断。
            let rel = format!("data/{}.txt", id);
            let _ = fs::write(dir.join(&rel), text.as_bytes());
            item.text_path = Some(rel);
        }
        RawClip::Files(files) => {
            item.kind = "files".into();
            item.size = files.iter().map(|f| f.len()).sum::<usize>() as u64;
            if files.is_empty() {
                item.preview = "[空文件列表]".into();
            } else {
                let name = std::path::Path::new(&files[0])
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                item.preview = if files.len() == 1 {
                    name
                } else {
                    format!("{} 等 {} 个文件", name, files.len())
                };
            }
            // 路径清单落盘（每行一个完整路径）：缓存目录里必须有这条记录的实体，
            // 否则「文件」条目的数据在缓存目录里完全看不到。
            let rel = format!("files/{}.txt", id);
            let body = files.join("\r\n");
            let _ = fs::write(dir.join(&rel), body.as_bytes());
            item.files_path = Some(rel);
            item.files = Some(files);
        }
        RawClip::Image(bytes) => {
            item.kind = "image".into();
            item.size = bytes.len() as u64;
            item.preview = "[图片]".into();
            let rel = format!("images/{}.bmp", id);
            if let Ok(bmp) = dib_to_bmp(&bytes) {
                let _ = fs::write(dir.join(&rel), &bmp);
                item.img_path = Some(rel);
            } else {
                // 兜底：直接存 DIB，前端无法显示但至少保留
                let rel = format!("images/{}.dib", id);
                let _ = fs::write(dir.join(&rel), &bytes);
                item.img_path = Some(rel);
            }
        }
        // ImageFile / ImageRaw 是「待处理」形态，经 resolve() 后会变成
        // Image 或 Files 才到这儿；留空分支兜底，不走 panic。
        RawClip::ImageFile(_) | RawClip::ImageRaw(_) => {}
    }

    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    hist.insert(0, item.clone());
    save_index(&dir, &hist);
    crate::log_print!(
        "[clipboard] 新增 {} 条目 kind={} preview={:?} size={}",
        id, item.kind, item.preview, item.size
    );
    item
}

// DIB（BITMAPINFOHEADER + 调色板/掩码 + 像素）包装成 BMP 文件头。
//
// BMP 文件 = 14 字节文件头 + **完整 DIB**（信息头 + 掩码/调色板 + 像素）。
// 之前这里写成 `&dib[bi_size..]`，把 40 字节的 BITMAPINFOHEADER 整个丢掉了，
// 生成的是无效 BMP，浏览器一律无法解码——这正是「图片永远不显示缩略图」的根因。
fn dib_to_bmp(dib: &[u8]) -> Result<Vec<u8>, String> {
    if dib.len() < 40 {
        return Err("DIB 过短".into());
    }
    let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    if bi_size < 40 || dib.len() < bi_size {
        return Err("DIB 头不完整".into());
    }
    let bi_bit_count = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    let bi_compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    // 压缩类型：0=BI_RGB；3=BI_BITFIELDS（header 后跟 3 个 RGB 掩码）；
    // 6=BI_ALPHABITFIELDS（4 个掩码）。漏算掩码会让像素整体错位偏色。
    let mask_size = match (bi_compression, bi_bit_count) {
        (3, 16) | (3, 32) => 3 * 4,
        (6, _) => 4 * 4,
        (0, _) if bi_bit_count > 8 => 0,
        (0, _) => {
            let used = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]);
            let used = if used == 0 { 1u32 << bi_bit_count } else { used };
            used as usize * 4
        }
        _ => return Err(format!("不支持的 DIB 压缩类型 {}", bi_compression)),
    };
    // 像素数据在文件中的偏移（文件头 14 + 信息头 + 掩码/调色板）
    let pixel_offset = 14 + bi_size + mask_size;
    let file_size = 14 + dib.len();

    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // reserved
    out.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    out.extend_from_slice(dib); // 完整 DIB：信息头 + 掩码/调色板 + 像素

    // 32bpp：Chromium 会把每像素第 4 字节当作 alpha，而剪贴板 DIB 该字节
    // 常常是 0（未使用）→ 整图被判为全透明而不显示。强制置 255 保持不透明。
    if bi_bit_count == 32 && pixel_offset < out.len() {
        let px = &mut out[pixel_offset..];
        let mut i = 3;
        while i < px.len() {
            px[i] = 255;
            i += 4;
        }
    }
    Ok(out)
}

/// DIB 超上限时按比例降采样（见 MAX_SIDE 注释）。
///
/// 剪贴板里的 CF_DIB 形态多样（16/24/32bpp、可能有掩码），与其手写各bpp的
/// 解析，不如借道 BMP：`dib_to_bmp` 已处理完这些变体，image 库可直接解码
/// BMP → 缩放 → 重拼标准 32bpp DIB。失败则原样返回（不因降采样失败丢数据）。
fn clamp_dib(dib: Vec<u8>) -> Vec<u8> {
    // 头部宽高（i32，可负但剪贴板里正常为正）；负高（自顶向下）这里不处理，
    // image 解码 BMP 同样不支持负高，会走 Err 分支原样保留
    let dims = || {
        if dib.len() < 40 {
            return None;
        }
        let w = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
        let h = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
        if w <= 0 || h <= 0 {
            return None;
        }
        Some((w as u32, h as u32))
    };
    let Some((w, h)) = dims() else {
        return dib;
    };
    if w <= MAX_SIDE && h <= MAX_SIDE {
        return dib;
    }
    crate::logger::write_line(
        "INFO",
        "clipboard",
        &format!("剪贴板位图 {w}×{h} 超 {MAX_SIDE}px，降采样存储"),
    );
    let Ok(bmp) = dib_to_bmp(&dib) else {
        return dib;
    };
    let Ok(img) = image::load_from_memory(&bmp) else {
        return dib;
    };
    let small = img.thumbnail(MAX_SIDE, MAX_SIDE);
    let rgba = small.to_rgba8();
    let (nw, nh) = (rgba.width(), rgba.height());
    let pixel_bytes = (nw as usize) * (nh as usize) * 4;
    let mut out = Vec::with_capacity(40 + pixel_bytes);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(nw as i32).to_le_bytes());
    out.extend_from_slice(&(nh as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..nh).rev() {
        for x in 0..nw {
            let p = rgba.get_pixel(x, y);
            out.push(p[2]);
            out.push(p[1]);
            out.push(p[0]);
            out.push(255);
        }
    }
    out
}

/// 生成/读取条目缩略图，返回 PNG 字节。
///
/// 列表与悬浮预览**必须**用缩略图而非原图：原图 BMP 未压缩（可达几十 MB），
/// base64 后经 IPC 进前端再被 <img> 解码，峰值直接打爆 WebView2 ——
/// 2.1.10 的 OOM 白屏即此因。缩略图长边 ≤ THUMB_SIDE，单张几十 KB。
///
/// 惰性生成：老数据没有 thumbs 文件，首次请求时从原图现场缩一份并落盘，
/// 之后走缓存。原图缺失/解码失败返回 None，前端显示占位。
fn ensure_thumb(dir: &std::path::Path, item: &HistoryItem) -> Option<Vec<u8>> {
    const THUMB_SIDE: u32 = 512;
    let rel = item.img_path.as_ref()?;
    let src = dir.join(rel);
    // 1) 已有缩略图直接读
    let thumb_rel = format!("thumbs/{}.png", item.id);
    let thumb_path = dir.join(&thumb_rel);
    if let Ok(bytes) = fs::read(&thumb_path) {
        return Some(bytes);
    }
    // 2) 从原图现场生成
    let bytes = fs::read(&src).ok()?;
    if bytes.len() > 256 * 1024 * 1024 {
        crate::logger::write_line(
            "ERROR",
            "clipboard",
            &format!("图片 {} 达 {}MB，超出处理能力，缩略图跳过", src.display(), bytes.len() / 1048576),
        );
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?;
    let small = img.thumbnail(THUMB_SIDE, THUMB_SIDE);
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(small.to_rgba8())
        .write_to(&mut png, image::ImageFormat::Png)
        .ok()?;
    let data = png.into_inner();
    // 落盘失败不影响本次返回（下次再生成）
    let _ = fs::create_dir_all(thumb_path.parent()?);
    let _ = fs::write(&thumb_path, &data);
    Some(data)
}

// ---------------------------------------------------------------------------
// 写回剪贴板
// ---------------------------------------------------------------------------

unsafe fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    let hwnd = MAIN_HWND.get().copied().unwrap_or(0);
    if OpenClipboard(hwnd) == 0 {
        return Err("OpenClipboard 失败".into());
    }
    EmptyClipboard();
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = utf16.len() * 2;
    let h = GlobalAlloc(0x0002, bytes); // GMEM_MOVEABLE
    if h == 0 {
        CloseClipboard();
        return Err("GlobalAlloc 失败".into());
    }
    let p = GlobalLock(h) as *mut u16;
    if p.is_null() {
        CloseClipboard();
        return Err("GlobalLock 失败".into());
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr(), p, utf16.len());
    GlobalUnlock(h);
    let ok = SetClipboardData(13, h); // CF_UNICODETEXT
    CloseClipboard();
    if ok == 0 {
        return Err("SetClipboardData 失败".into());
    }
    mark_skip();
    Ok(())
}

unsafe fn write_files_to_clipboard(files: &[String]) -> Result<(), String> {
    let hwnd = MAIN_HWND.get().copied().unwrap_or(0);
    if OpenClipboard(hwnd) == 0 {
        return Err("OpenClipboard 失败".into());
    }
    EmptyClipboard();

    // 计算所需字节：DROPFILES 头部(20) + 各路径 UTF-16(null 结尾) + 末尾双 null
    let mut total = 20usize;
    for f in files {
        total += (f.encode_utf16().count() + 1) * 2;
    }
    total += 2; // 末尾额外 null

    let h = GlobalAlloc(0x0002, total);
    if h == 0 {
        CloseClipboard();
        return Err("GlobalAlloc 失败".into());
    }
    let base = GlobalLock(h) as *mut u8;
    if base.is_null() {
        CloseClipboard();
        return Err("GlobalLock 失败".into());
    }
    // DROPFILES: pFiles(4) + pt(8) + fNC(4) + fWide(4)
    let p_files: u32 = 20;
    let f_wide: i32 = 1;
    let p_files_b = p_files.to_le_bytes();
    let f_wide_b = f_wide.to_le_bytes();
    std::ptr::copy_nonoverlapping(p_files_b.as_ptr(), base, 4);
    std::ptr::copy_nonoverlapping([0u8; 8].as_ptr(), base.add(4), 8);
    std::ptr::copy_nonoverlapping([0u8; 4].as_ptr(), base.add(12), 4);
    std::ptr::copy_nonoverlapping(f_wide_b.as_ptr(), base.add(16), 4);

    let mut off = 20usize;
    for f in files {
        let w: Vec<u16> = f.encode_utf16().chain(std::iter::once(0)).collect();
        std::ptr::copy_nonoverlapping(w.as_ptr() as *const u8, base.add(off), w.len() * 2);
        off += w.len() * 2;
    }
    // 末尾双 null（已有一个，再补一个）
    std::ptr::write(base.add(off) as *mut u16, 0);
    GlobalUnlock(h);
    let ok = SetClipboardData(15, h); // CF_HDROP
    CloseClipboard();
    if ok == 0 {
        return Err("SetClipboardData(CF_HDROP) 失败".into());
    }
    mark_skip();
    Ok(())
}

unsafe fn mark_skip() {
    SKIP_SEQ.store(GetClipboardSequenceNumber(), Ordering::Relaxed);
}

/// DIB（BITMAPINFOHEADER + 像素）写入 CF_DIB
unsafe fn write_dib_to_clipboard(dib: &[u8]) -> Result<(), String> {
    let hwnd = MAIN_HWND.get().copied().unwrap_or(0);
    if OpenClipboard(hwnd) == 0 {
        return Err("OpenClipboard 失败".into());
    }
    EmptyClipboard();
    let h = GlobalAlloc(GMEM_MOVEABLE, dib.len());
    if h == 0 {
        CloseClipboard();
        return Err("GlobalAlloc 失败".into());
    }
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        CloseClipboard();
        return Err("GlobalLock 失败".into());
    }
    std::ptr::copy_nonoverlapping(dib.as_ptr(), p, dib.len());
    GlobalUnlock(h);
    let ok = SetClipboardData(8, h); // CF_DIB
    CloseClipboard();
    if ok == 0 {
        return Err("SetClipboardData(CF_DIB) 失败".into());
    }
    mark_skip();
    Ok(())
}

// ---------------------------------------------------------------------------
// 存储辅助
// ---------------------------------------------------------------------------

fn load_index(dir: &PathBuf) -> Vec<HistoryItem> {
    let path = dir.join("index.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let mut v: Vec<HistoryItem> = serde_json::from_str(&s).unwrap_or_else(|e| {
                crate::log_warn!("[clipboard] index.json 解析失败：{e}，返回空");
                Vec::new()
            });
            // 旧版是**全局**置顶（pinned: bool）。迁移成「历史」分类的置顶——
            // 历史是默认视图，用户当初置顶的内容在历史里仍然浮顶，行为不变。
            let mut migrated = 0usize;
            for i in v.iter_mut() {
                if i.pinned && !i.pinned_views.iter().any(|x| x == "history") {
                    i.pinned_views.push("history".to_string());
                    migrated += 1;
                }
            }
            if migrated > 0 {
                crate::log_print!("[clipboard] 迁移 {migrated} 条旧版全局置顶 → 历史分类置顶");
            }
            v
        }
        Err(_) => Vec::new(),
    }
}

fn save_index(dir: &PathBuf, hist: &Vec<HistoryItem>) {
    let path = dir.join("index.json");
    if let Ok(s) = serde_json::to_string_pretty(hist) {
        let _ = fs::write(&path, s);
    }
}

/// 为「内联了 files、但缺 files_path」的历史文件条目补写 `files/<id>.txt`，
/// 返回补写的条数。老版本 index.json 是这么存的，不补的话缓存目录里看不到这些条目。
fn backfill_files(dir: &PathBuf, hist: &mut Vec<HistoryItem>) -> usize {
    let mut n = 0usize;
    for item in hist.iter_mut() {
        if item.kind != "files" || item.files_path.is_some() {
            continue;
        }
        let Some(files) = item.files.as_ref() else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        let rel = format!("files/{}.txt", item.id);
        if fs::write(dir.join(&rel), files.join("\r\n").as_bytes()).is_ok() {
            item.files_path = Some(rel);
            n += 1;
        }
    }
    n
}

fn remove_item_files(dir: &PathBuf, item: &HistoryItem) {
    if let Some(rel) = &item.text_path {
        let _ = fs::remove_file(dir.join(rel));
    }
    if let Some(rel) = &item.img_path {
        let _ = fs::remove_file(dir.join(rel));
        // 缩略图与原图同生命周期，一并清掉
        let _ = fs::remove_file(dir.join(format!("thumbs/{}.png", item.id)));
    }
    if let Some(rel) = &item.files_path {
        let _ = fs::remove_file(dir.join(rel));
    }
}

fn patch_item<F: FnOnce(&mut HistoryItem)>(id: &str, f: F) -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    if let Some(item) = hist.iter_mut().find(|i| i.id == id) {
        f(item);
        save_index(&dir, &hist);
        Ok(())
    } else {
        Err("条目不存在".into())
    }
}

// ---------------------------------------------------------------------------
// 保留策略（迁移自 v1 RETENTION_DAYS = 3）
// ---------------------------------------------------------------------------

/// 按天数清理过期条目（0 = 不清理），返回清理后的历史。
/// v1 语义：严格按时间戳过滤，不豁免置顶/收藏。
/// 只删过期条目自己的实体文件，**不碰 `log/`**（见 CONTENT_DIRS 注释）。
pub fn prune_retention(days: u32) {
    if let Some(dir) = DIR.get() {
        let mut hist = HISTORY.get().unwrap().lock().unwrap();
        let before = hist.len();
        *hist = prune_history(hist.clone(), dir, days);
        let removed = before - hist.len();
        if removed > 0 {
            crate::log_print!("[clipboard] 保留策略清理 {removed} 条过期条目");
        }
    }
}

/// 纯函数版本：过滤 + 删文件 + 存索引
fn prune_history(hist: Vec<HistoryItem>, dir: &PathBuf, days: u32) -> Vec<HistoryItem> {
    if days == 0 {
        return hist;
    }
    let deadline = now_ms().saturating_sub(days as u64 * 24 * 60 * 60 * 1000);
    let kept: Vec<HistoryItem> = hist
        .into_iter()
        .filter(|i| {
            if i.ts >= deadline {
                true
            } else {
                remove_item_files(dir, i);
                false
            }
        })
        .collect();
    save_index(dir, &kept);
    kept
}

// ---------------------------------------------------------------------------
// 缓存目录管理（迁移自 v1 pick-cache-dir / get-image-cache-info / open-cache-dir）
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct CacheInfo {
    pub dir: String,
    pub count: u64,
    pub size: u64,
    /// 其中属于 `log/` 的那部分（文件数 / 字节）。
    /// 单独给出是为了把「缓存占用包含日志」这件事说清楚——清理动作从不碰 `log/`，
    /// 但日志确实占着空间，用户有权知道占了多少（见 `CONTENT_DIRS` 注释）。
    pub log_count: u64,
    pub log_size: u64,
}

/// 去掉 Windows `canonicalize` 产生的扩展长度前缀，让路径显示为普通形式：
/// `\\?\E:\a` → `E:\a`；`\\?\UNC\server\share` → `\\server\share`。
fn strip_ext_prefix(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// 缓存目录本身（未初始化时为空路径）。
///
/// 给「下载更新包到缓存目录」这类调用方用——它们只要路径，
/// 不想付 `cache_info()` 那趟全目录统计（会递归遍历所有文件）。
pub fn cache_dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_default()
}

#[tauri::command]
pub fn cache_info() -> CacheInfo {
    let dir = DIR.get().cloned().unwrap_or_default();
    let (count, size) = dir_stats(&dir);
    let (log_count, log_size) = dir_stats(&dir.join("log"));
    CacheInfo {
        dir: strip_ext_prefix(&dir.to_string_lossy()),
        count,
        size,
        log_count,
        log_size,
    }
}

/// 递归统计目录内文件数与总字节
fn dir_stats(dir: &PathBuf) -> (u64, u64) {
    let (mut count, mut size) = (0u64, 0u64);
    let Ok(rd) = fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let (c, s) = dir_stats(&entry.path());
            count += c;
            size += s;
        } else if let Ok(meta) = entry.metadata() {
            count += 1;
            size += meta.len();
        }
    }
    (count, size)
}

/// 选择新的缓存目录：移动全部数据 → 写配置 → 短暂延迟后重启应用使 DIR 生效。
///
/// 关键：`blocking_pick_folder` 不能在主线程调用（会死锁，表现为「点了没反应」），
/// 因此放到阻塞线程池上执行，本命令声明为 async。
#[tauri::command]
pub async fn pick_cache_dir(app: AppHandle) -> Result<Option<(String, bool)>, String> {
    use tauri_plugin_dialog::DialogExt;

    let app_for_dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_dialog
            .dialog()
            .file()
            .set_title("选择缓存目录")
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;

    let Some(picked) = picked else {
        return Ok(None); // 用户取消
    };
    let new_dir: PathBuf = match picked {
        tauri_plugin_dialog::FilePath::Path(p) => p,
        tauri_plugin_dialog::FilePath::Url(u) => {
            return Err(format!("不支持的路径：{u}"))
        }
    };
    if !new_dir.is_dir() {
        return Err("所选路径不是目录".into());
    }

    let old = match DIR.get() {
        Some(d) => d.clone(),
        None => return Err("存储目录未初始化".into()),
    };

    // ── 防线①：归一化之后判同 ──────────────────────────────
    // 字符串比较不可靠：`\\?\` 前缀、末尾反斜杠、短路径…都能让「同一个目录」
    // 长得不一样，于是同一目录也被当成"换了目录"而进入迁移 → 自我搬迁 → 无限嵌套/卡死。
    // canonicalize 会把这些形式差异全部归一。
    let old_c = fs::canonicalize(&old).map_err(|e| format!("当前缓存目录不可访问：{e}"))?;
    let new_c = fs::canonicalize(&new_dir).map_err(|e| format!("所选目录不可访问：{e}"))?;

    if old_c == new_c {
        // 选的就是当前目录：什么都不用做，更不能迁移
        return Ok(Some((strip_ext_prefix(&new_dir.to_string_lossy()), false)));
    }

    // ── 防线②：拒绝危险的包含关系 ────────────────────────
    if new_c.starts_with(&old_c) {
        return Err("新目录不能是当前缓存目录本身或其子目录，请选一个外部位置".into());
    }
    if old_c.starts_with(&new_c) {
        return Err("新目录不能是当前缓存目录的上级目录，请选一个外部位置".into());
    }

    // ── 目录识别：空目录 vs 非空目录 ─────────────────────
    // - 选中的是空目录 → 直接当作新的缓存根，data/images/files/index.json 搬进去；
    // - 选中的非空     → 不污染它，在其下建一个缓存子文件夹（沿用当前缓存目录名）再搬。
    let target_root = if dir_is_empty(&new_dir) {
        new_dir.clone()
    } else {
        let sub_name = old
            .file_name()
            .map(|s| s.to_os_string())
            .ok_or_else(|| "无法确定缓存目录名".to_string())?;
        let sub = new_dir.join(&sub_name);
        if sub.exists() {
            return Err(format!(
                "目标位置已存在同名文件夹「{}」，请先清空它或换一个目录",
                sub_name.to_string_lossy()
            ));
        }
        sub
    };

    // ── 防线③在 move_dir_contents 内部（结构性杜绝无限递归）──
    move_dir_contents(&old, &target_root).map_err(|e| format!("迁移缓存文件失败：{e}"))?;

    let new_dir_str = strip_ext_prefix(&target_root.to_string_lossy());
    crate::settings::update(|c| c.cache_dir = Some(new_dir_str.clone()));
    crate::log_print!("[clipboard] 缓存目录已迁移至 {new_dir_str}，即将重启");

    // DIR 是 OnceLock，运行期不可变：迁移完成后重启进程，让新目录在下次启动生效
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        handle.restart();
    });
    Ok(Some((new_dir_str, true)))
}

/// 目录是否为空（不含任何条目；隐藏文件也算有内容）
fn dir_is_empty(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut rd) => rd.next().is_none(),
        Err(_) => false,
    }
}

/// 递归把 old 内所有内容移动到 new（跨卷也适用：rename 失败退回 copy+delete）
///
/// **安全约束：结构性地杜绝"自己搬进自己"导致的无限嵌套目录 / 卡死闪退**
/// - **先快照、后搬运**：先把 old 的子项收集成列表，再逐个移动。若不这样做，
///   `create_dir_all(new)` 刚建出的新目录会被随后的 `read_dir(old)` 当成 old 的一个
///   条目读回来——一旦 new 位于 old 内部，就必然自我嵌套、一层套一层。
/// - **每层断言**：`old != new` 且 `new` 不在 `old` 内部，违反立即返回错误、绝不继续。
/// - **深度上限**兜底，任何异常递归都会在此终止。
fn move_dir_contents(old: &PathBuf, new: &PathBuf) -> std::io::Result<()> {
    move_dir_contents_depth(old, new, 0)
}

fn move_dir_contents_depth(old: &Path, new: &Path, depth: u32) -> std::io::Result<()> {
    const MAX_DEPTH: u32 = 64;
    if depth > MAX_DEPTH {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "目录层级异常过深，已中止迁移",
        ));
    }
    if old == new || new.starts_with(old) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "目标目录落在源目录内部，已中止迁移",
        ));
    }

    fs::create_dir_all(new)?;

    // 先快照 old 当前的子项，不边建边读
    let children: Vec<PathBuf> = fs::read_dir(old)?.flatten().map(|e| e.path()).collect();

    for from in children {
        // 兜底：万一快照里混进了 new 自身（或 new 落在某个 from 内部），跳过，防自噬
        if from == new || new.starts_with(&from) {
            continue;
        }
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = new.join(name);
        if from.is_dir() {
            move_dir_contents_depth(&from, &to, depth + 1)?;
            fs::remove_dir_all(&from).ok();
        } else if fs::rename(&from, &to).is_err() {
            fs::copy(&from, &to)?;
            let _ = fs::remove_file(&from);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn open_cache_dir() -> Result<(), String> {
    let dir = DIR.get().ok_or("存储目录未初始化")?;
    std::process::Command::new("explorer")
        .arg(dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 直接打开 `log/` 子目录。日志不会被任何清理动作删除，
/// 用户想删只能自己来——那就得让他们找得到。
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let dir = DIR.get().ok_or("存储目录未初始化")?;
    let log = dir.join("log");
    let _ = fs::create_dir_all(&log);
    std::process::Command::new("explorer")
        .arg(log)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 退出清理（迁移自 v1 cleanImagesOnClose：清空内容缓存 + 历史）
// ---------------------------------------------------------------------------

/// 允许被清理动作触碰的内容目录——**只有这三个**。
///
/// `log/` 刻意不在其中：日志是排查问题的唯一凭据，尤其是崩溃现场。
/// 因此以下所有清除路径都**从未**删除日志，也绝不允许删除：
/// 保留天数清理（`prune_history`）、清空全部历史（`clear_all`）、
/// 退出清理（`on_app_exit`）、缓存目录迁移（`move_dir_contents` 只搬不移除）。
/// 日志只会在用户于文件管理器里手动删除时才消失。
const CONTENT_DIRS: [&str; 3] = ["data", "images", "files"];

/// 在 RunEvent::Exit 时调用。clean_on_close 开启则清空全部内容文件与历史索引。
pub fn on_app_exit() {
    // 落盘已挪到后台线程，退出前得等它跑完，否则「刚复制就退出」会丢记录。
    // 上限 2 秒：大图解码可能较久，但不至于让退出卡住。
    flush_pending(std::time::Duration::from_secs(2));

    let cfg = crate::settings::get();
    if !cfg.clean_on_close {
        return;
    }
    let include_pinned = cfg.clean_include_pinned;
    crate::log_print!(
        "[clipboard] 关闭时清理缓存（include_pinned={}）",
        include_pinned
    );
    if let Some(dir) = DIR.get() {
        if include_pinned {
            // 全清：只清内容目录（CONTENT_DIRS，**不含 log/**）+ 历史索引
            for sub in CONTENT_DIRS {
                let _ = fs::remove_dir_all(dir.join(sub));
                let _ = fs::create_dir_all(dir.join(sub));
            }
            if let Some(h) = HISTORY.get() {
                h.lock().unwrap().clear();
            }
            save_index(dir, &Vec::new());
        } else {
            // 保留置顶/收藏：只删普通条目的内容文件
            let mut hist = HISTORY.get().unwrap().lock().unwrap();
            hist.retain(|i| {
                if !i.pinned_views.is_empty() || i.favorite {
                    true
                } else {
                    remove_item_files(dir, i);
                    false
                }
            });
            save_index(dir, &hist);
        }
    }
}

// ---------------------------------------------------------------------------
// 杂项
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn make_id(ts: u64) -> String {
    let c = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{}", ts, c)
}

fn non_null<T>(v: T) -> Option<T>
where
    T: PartialEq + Default,
{
    if v == T::default() {
        None
    } else {
        Some(v)
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        s.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        s.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            s.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            s.push('=');
        }
        if chunk.len() > 2 {
            s.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            s.push('=');
        }
    }
    s
}

// ---------------------------------------------------------------------------
// FFI
// ---------------------------------------------------------------------------

const GMEM_MOVEABLE: u32 = 0x0002;

extern "system" {
    fn OpenClipboard(hwnd: isize) -> i32;
    fn AddClipboardFormatListener(hwnd: isize) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn SetClipboardData(format: u32, handle: isize) -> isize;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardSequenceNumber() -> u32;
    fn GetClipboardOwner() -> isize;

    fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
    fn GlobalLock(handle: isize) -> *mut std::ffi::c_void;
    fn GlobalUnlock(handle: isize) -> i32;
    fn GlobalSize(handle: isize) -> usize;

    fn DragQueryFileW(hdrop: isize, i: u32, buf: *mut u16, buflen: u32) -> u32;
    // 注意：这里**故意不声明 DragFinish**。它只适用于 WM_DROPFILES 的临时 HDROP，
    // 用在本模块的剪贴板 HDROP 上会导致二次释放崩溃（详见 read_clipboard 内的说明）。
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个 32bpp BI_RGB 的纯色 DIB，供指纹测试用。
    fn solid_dib(w: i32, h: i32, color: [u8; 4]) -> Vec<u8> {
        let px = (w as usize) * (h as usize) * 4;
        let mut v = Vec::with_capacity(40 + px);
        v.extend_from_slice(&40u32.to_le_bytes()); // biSize
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        v.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        v.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        v.extend_from_slice(&(px as u32).to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..(w * h) {
            v.extend_from_slice(&color);
        }
        v
    }

    /// 用户反馈的核心场景：相近位置反复截图，尺寸一样但画面不同 —— 必须放行。
    #[test]
    fn same_size_different_content_is_not_dup() {
        let red = image_fingerprint(&solid_dib(400, 300, [200, 40, 40, 255]));
        let blue = image_fingerprint(&solid_dib(400, 300, [40, 40, 200, 255]));
        assert_ne!(red, blue, "同尺寸不同画面必须判为不同图");
    }

    /// 微信一次复制的两条写入内容相同 —— 必须拦截。
    #[test]
    fn same_content_is_dup() {
        let a = solid_dib(400, 300, [10, 20, 30, 255]);
        let mut b = a.clone();
        // 模拟两条写入路径的差异（一路从 PNG 解码、一路直读位图）,
        // 像素值只差几个低位。
        if let Some(p) = b.get_mut(40) {
            *p ^= 0b0000_0101;
        }
        assert_eq!(
            image_fingerprint(&a),
            image_fingerprint(&b),
            "内容一致的两次写入必须判为同图"
        );
    }

    /// 尺寸是判据的一部分：不同尺寸必然不是同一张。
    #[test]
    fn different_size_is_not_dup() {
        let wide = image_fingerprint(&solid_dib(400, 300, [200, 40, 40, 255]));
        let tall = image_fingerprint(&solid_dib(300, 400, [200, 40, 40, 255]));
        assert_ne!(wide, tall);
    }
}
