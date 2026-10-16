//! 剪贴板监听 / 读取 / 存储 / 写回。
//!
//! 设计红线（DESIGN.md 6.1 / 6.3.1）：
//!   - 事件驱动：主路径只响应 `WM_CLIPBOARDUPDATE`；另有 1.5s 低频心跳兜底
//!     （窗口消息在最小化/特殊 DWM 状态下不保证投递，见 spawn_clipboard_watchdog）
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
/// 「延迟渲染还没交货、已交回看门狗下一拍重读」的序列号。
/// 只补读一次：看门狗再失败就正常标记为已处理，免得每拍都重试、刷屏日志。
static RETRY_SEQ: AtomicU32 = AtomicU32::new(0);
/// id 计数器，避免同毫秒碰撞
static ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 最近若干条图片条目的 (内容指纹, 时间戳ms)：图片判重依据。
/// 用指纹而非「宽高」——见 image_fingerprint 的说明。
///
/// 存多条而非一条：微信复制一张大图会在**数秒内反复改写剪贴板**（实测同一张
/// 50MB 的图 4 秒内被写 6 次，每次间隔递增）。只记最后一条的话，间隔一旦超过
/// 窗口就全放行，历史里会冒出 6 条一模一样的 50MB 记录 —— 既是「复制大图出现
/// 两张（实际是好几张）」的成因，也意味着要重复做 6 次秒级的解码+写盘，卡顿被
/// 放大数倍。保留一个小队列，窗口内的重复一律合并。
static LAST_IMG: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
/// 队列上限：只关心「最近这一小段时间」，不用无限增长。
const LAST_IMG_KEEP: usize = 16;
/// 剪贴板落盘工作线程的投递端。解码 / 写盘 / 生成缩略图都在那边做，
/// 主线程只负责把剪贴板里的字节拷出来就返回，见 spawn_worker。
static WORKER: OnceLock<std::sync::mpsc::Sender<(u64, RawClip)>> = OnceLock::new();
/// 已投递给工作线程、尚未落盘完成的条数。退出前据此等待，避免「刚复制就退出」丢数据。
static PENDING: AtomicU32 = AtomicU32::new(0);

/// 「图片文件路径 → 已解码 DIB」的小 LRU 缓存。
///
/// **为什么需要**：微信复制一张图后会在一两分钟内**反复**把图片重新放进剪贴板，
/// 而且**每次的文件名都不同**（实测 44ad35…、b9e48ee…、0c05463…、3a916774…，
/// 都在同一个 RWTemp 目录下）。没有缓存时每次都要 `image::open` 完整解码那张
/// 4096×3072 的原图 —— 单次约 1~2 秒，用户看到的就是「复制完要等 4~6 秒」。
///
/// **为什么不是只存 1 条**：微信每次补写的路径都变，单条缓存必然反复失效
/// （实测日志里同一次复制的 6 次触发用了 3 个不同路径）。所以存多条按 LRU 淘汰。
///
/// 键用 `(路径, mtime, size)`：微信 RWTemp 目录里路径会复用、内容会换，
/// 只比路径会拿到过期内容。
///
/// 容量 4 条 × 每条 ≤16MB（MAX_SIDE 已限制）= 最坏 64MB 常驻。取 3 更稳妥：
/// 实测一次复制最多涉及 3 个路径。
static DECODE_CACHE: Mutex<Vec<(PathBuf, (u64, u64), Vec<u8>)>> = Mutex::new(Vec::new());
/// 解码缓存条数上限（LRU：最老的先淘汰）。
const DECODE_CACHE_CAP: usize = 3;

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
        let kind = item.kind.clone();
        drop(hist);
        remove_item_files(&dir, &item);
        save_index(&dir, &HISTORY.get().unwrap().lock().unwrap());
        // 只记 id / kind 等元数据，不含内容。
        crate::log_print!("[clipboard] 删除条目 {id} kind={kind}");
    } else {
        // 前端理论上不会删不存在的条目；真出现说明前后端状态不同步，值得记一笔。
        crate::log_warn!("[clipboard] 删除失败：条目不存在 {id}（前后端状态不同步）");
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
    // 只记「哪条、哪个分类、切完是置顶还是取消、互通开关状态」——都是元数据。
    let state = if out.is_empty() {
        "取消置顶"
    } else {
        "置顶"
    };
    crate::log_print!("[clipboard] {state} {id} view={view} 互通={global}");
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
    let mut now = false;
    patch_item(&id, |i| {
        i.favorite = !i.favorite;
        now = i.favorite;
    })?;
    crate::log_print!(
        "[clipboard] {} 收藏 {id}",
        if now { "加入" } else { "取消" }
    );
    Ok(())
}

#[tauri::command]
pub fn clear_all() -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    // 只删条目自己的实体（文本 / 图片 / 文件清单），不碰目录里其它内容——
    // 尤其不碰 `log/`（见 CONTENT_DIRS 注释）。
    let n = hist.len();
    for item in hist.iter() {
        remove_item_files(&dir, item);
    }
    hist.clear();
    save_index(&dir, &hist);
    crate::log_print!("[clipboard] 清空全部历史，共 {n} 条");
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
    let rel = item.img_path.as_ref().ok_or("该条目不是图片")?.clone();
    let path = dir.join(&rel);
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    drop(hist);
    if size > MAX_IMAGE_BYTES {
        crate::logger::note_error(
            "clipboard",
            &format!(
                "get_image 拒绝 {}：{}MB 超上限（考虑删除该条目）",
                id,
                size / 1048576
            ),
        );
        return Err(format!("图片过大（{}MB），无法在界面显示", size / 1048576));
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let b64 = base64_encode(&bytes);
    // MIME 按扩展名给。改动前硬编码 `image/bmp` —— 那时落盘的都是 BMP 转换结果，
    // 现在存的是原图（多为 JPEG），继续硬编码会让浏览器按错误类型解码。
    let mime = match rel.rsplit('.').next().unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/bmp",
    };
    Ok(format!("data:{mime};base64,{}", b64))
}

/// 详情弹窗显示正文时的**字符上限**。
///
/// 文本可能很大（日志、整篇文档），把几 MB 的字符串经 IPC 塞进 WebView 再渲染成
/// 一个文本节点，轻则卡顿重则 OOM。所以显示有上限。
///
/// ⚠️ **这不影响「复制全文」**：那条路径（`copy_back`）是在 Rust 侧直接读盘写
/// 剪贴板的，根本不经过前端 —— 所以完整的原文始终能复制出去，不因显示截断而丢失。
const DETAIL_DISPLAY_CAP: usize = 200_000;

/// 详情弹窗的返回体。
///
/// `rename_all = "camelCase"`：本项目**没有**全局的字段名转换（见 HistoryItem 里
/// 对 pinnedViews 的显式 rename），跨到前端就得逐个显式命名，避免前端拿到
/// `total_chars` 还是 `totalChars` 全靠猜。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FullContent {
    /// 正文（超长时只返回前 `DETAIL_DISPLAY_CAP` 个字符）
    pub text: String,
    /// 是否因过长而被截断（前端据此提示用户）
    pub truncated: bool,
    /// 完整内容的字符总数
    pub total_chars: usize,
    /// 完整内容的总行数
    pub total_lines: usize,
}

/// 读取条目的**完整内容**，供详情弹窗显示。
///
/// 文本条目读 `data/<id>.txt`；文件条目读 `files/<id>.txt` 的路径清单 —— 两者
/// 都落在缓存目录里，也都能长到列表显示不下，所以共用一个通道。
#[tauri::command]
pub fn get_full_content(id: String) -> Result<FullContent, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let item = {
        let hist = HISTORY.get().unwrap().lock().unwrap();
        hist.iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or("条目不存在")?
    };

    // 文本正文优先；其次是文件路径清单。图片没有可查看的长文本。
    let rel = item
        .text_path
        .as_ref()
        .or(item.files_path.as_ref())
        .ok_or("该条目没有可查看的文本内容")?;
    let raw = read_text_entity(&dir, rel)?;
    Ok(summarize_content(raw))
}

/// 按显示上限截断正文，并统计**完整内容**的规模。
///
/// 抽成独立函数是为了能单测 —— `get_full_content` 依赖全局的目录与历史，
/// 而这里只做纯计算。
///
/// 关键点：`total_chars` / `total_lines` 统计的是**截断前**的完整内容。
/// 若统计截断后的长度，用户会看到「共 200000 字」而误以为内容就这么长。
fn summarize_content(raw: String) -> FullContent {
    let total_chars = raw.chars().count();
    let total_lines = raw.lines().count();
    let truncated = total_chars > DETAIL_DISPLAY_CAP;
    let text = if truncated {
        raw.chars().take(DETAIL_DISPLAY_CAP).collect()
    } else {
        raw
    };
    FullContent {
        text,
        truncated,
        total_chars,
        total_lines,
    }
}

/// 把任意文本写入系统剪贴板（供「复制并编辑」的保存动作使用）。
///
/// **为什么是「编辑副本」而不是「直接改历史」**：历史必须忠实反映「当初复制了
/// 什么」。若能原地改，用户会怀疑「我明明复制的是 A，怎么变成 B 了」；而且剪贴板
/// 历史常被当安全网（删了还在），改动它却不可逆是件危险的事。
/// 所以保存动作 = **写剪贴板**，不碰 index.json、不改原文。
///
/// **不会产生新条目**：内部走 `write_text_to_clipboard`，它会 `mark_skip()`，
/// 于是这次自写不会被当成一次外部复制回灌成新记录 —— 与「点击条目复制回剪贴板」
/// 的行为保持一致（历史只由**外部**复制产生）。
#[tauri::command]
pub fn set_clipboard_text(text: String) -> Result<(), String> {
    if text.is_empty() {
        return Err("内容为空，未写入剪贴板".into());
    }
    match unsafe { write_text_to_clipboard(&text) } {
        Ok(()) => {
            // 只记字符数，绝不记内容本身（编辑后的正文同样是用户数据）。
            crate::log_print!(
                "[clipboard] 编辑保存：写回剪贴板 {} 字符",
                text.chars().count()
            );
            Ok(())
        }
        Err(e) => {
            crate::log_err!("clipboard", "编辑保存：写回剪贴板失败 {e}");
            Err(e)
        }
    }
}

/// 缩略图：列表与悬浮预览专用（PNG，长边 ≤512，单张几十 KB）。
/// 返回 data URL；失败返回 Err，前端显示占位图。
///
/// 必须是 **async**：Tauri 的同步命令跑在**主线程**上，而 `ensure_thumb` 首次
/// 生成时要「整图解码 → 缩放 → PNG 编码」，一张 16MB 落盘图也要数百毫秒。
/// 同步执行会让主线程卡住，表现为「复制大图后整个程序冻住一阵」，
/// 且列表滚动/置顶等交互全部无响应。异步后解码在 Tauri 的异步运行时线程池，
/// 主线程只等结果。
#[tauri::command]
pub async fn get_image_thumb(id: String) -> Result<String, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let item = {
        let hist = HISTORY.get().unwrap().lock().unwrap();
        hist.iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or("条目不存在")?
    };
    match ensure_thumb(&dir, &item) {
        Some(png) => {
            let b64 = base64_encode(&png);
            Ok(format!("data:image/png;base64,{}", b64))
        }
        None => Err("缩略图不可用".into()),
    }
}

/// 回贴（copy_back）解码时的长边上限。
///
/// 因为落盘的是**原图**，回贴理应还原原图，所以上限给得比存储期宽松得多
/// （存储期是 MAX_SIDE=2048，那是给前端看的；现在前端只看缩略图，已不需要）。
/// 仍设上限是防极端大图把内存打爆：8192 长边 ≈ 6700 万像素 ≈ 268MB DIB。
const PASTE_MAX_SIDE: u32 = 8192;

/// 把历史条目内容写回系统剪贴板（文本 / 图片 / 文件列表）。
///
/// 返回 Ok(Some(提示)) 时前端用该提示替代默认的「已复制到剪贴板」——
/// 目前只有文件条目会用到（部分原文件已失效、已跳过）。
#[tauri::command]
pub fn copy_back(id: String) -> Result<Option<String>, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let hist = HISTORY.get().unwrap().lock().unwrap();
    let item = hist
        .iter()
        .find(|i| i.id == id)
        .cloned()
        .ok_or("条目不存在")?;
    drop(hist);

    let kind = item.kind.clone();
    // 各分支统一收口到 `result`，最后只落**一条**日志再返回——
    // 原来这里一个分支都没记日志，用户遇到「点了条目粘贴不出来」时无从排查。
    // 用立即调用的闭包收口，使分支里的 `return` / `?` 只退出闭包、不绕过日志。
    let result: Result<Option<String>, String> = (|| -> Result<Option<String>, String> {
        match kind.as_str() {
            "text" => {
                let text = match &item.text_path {
                    Some(rel) => read_text_entity(&dir, rel)?,
                    None => item.preview.clone(),
                };
                unsafe { write_text_to_clipboard(&text) }.map(|_| None)
            }
            "files" => {
                // 以落盘清单为唯一数据源（与文本条目走 text_path 一致）；
                // 旧版 index.json 把路径内联在 files 里、没有 files_path，回退到内存中的 files。
                let files: Vec<String> = match &item.files_path {
                    Some(rel) => read_text_entity(&dir, rel)?
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
                    return Err(format!("原文件都已不存在（{total} 个），可能被移动或删除"));
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
                // 落盘格式有三种，按扩展名分派（都是我们自己写下的，可信）：
                //   - .bmp  → 掐掉 14 字节文件头即 DIB
                //   - .dib  → 本身就是裸 DIB，直接用（极少数兜底条目）
                //   - 其它  → 原图（jpg/png/tiff/svg…），此刻才解码成 DIB
                // 解码放在这里，是因为它只发生在**用户点击回贴**时，感知不到延迟。
                let rel = item.img_path.ok_or("图片文件缺失")?;
                let path = dir.join(rel.clone());
                let bytes = fs::read(&path).map_err(|e| e.to_string())?;
                let dib = if rel.ends_with(".bmp") {
                    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
                        return Err("图片数据损坏".into());
                    }
                    bytes[14..].to_vec()
                } else if rel.ends_with(".dib") {
                    bytes
                } else {
                    match image_file_to_dib(&path, PASTE_MAX_SIDE) {
                        Some(d) => d,
                        None => {
                            // 格式不设限的代价：有些图片 image 库解不开
                            // （SVG 是矢量文本、HEIC/AVIF 缺解码器、PSD 是分层工程文件）。
                            // 解不开不等于没用 —— 退回**按文件粘贴**，用户仍能把它
                            // 丢进聊天窗口/文件夹，而不是点了没反应。
                            unsafe {
                                write_files_to_clipboard(&[path.to_string_lossy().to_string()])?;
                            }
                            crate::logger::write_line(
                                "INFO",
                                "clipboard",
                                &format!("图片 {rel} 无法解码为位图，已按文件写回剪贴板"),
                            );
                            return Ok(Some("该图片格式无法转为位图，已按文件复制".into()));
                        }
                    }
                };
                unsafe { write_dib_to_clipboard(&dib) }.map(|_| None)
            }
            _ => Err("未知类型".into()),
        }
    })();

    // 回贴成功/失败一律记录（元数据，不含内容）。
    match &result {
        Ok(Some(msg)) => crate::log_print!("[clipboard] 回贴 {id} kind={kind} 成功（{msg}）"),
        Ok(None) => crate::log_print!("[clipboard] 回贴 {id} kind={kind} 成功"),
        Err(e) => crate::log_err!("clipboard", "回贴 {id} kind={kind} 失败：{e}"),
    }
    result
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
            Some(rel) => read_text_entity(&dir, rel)
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

/// 兜底轮询线程：定期比对剪贴板序号，补上窗口消息漏掉的那部分。
///
/// 存在的理由：`WM_CLIPBOARDUPDATE` 只在**主窗口 WndProc** 里被处理，而该消息
/// 在窗口不可见时（最小化、不在前台、锁屏）系统并不保证投递 —— 漏掉即永久丢失。
/// 用户反馈正是「窗口开着但在后台时，复制的东西进不来」。
///
/// 为什么用序号而不是直接读剪贴板：`GetClipboardSequenceNumber` 是纯粹的读取，
/// 不会像 OpenClipboard 那样去抢全局锁（对方正在写入时只会返回当前值）。
/// 发现序号变了才去读内容，成本可忽略。
///
/// 幂等性：主线程与本线程都走 `on_clipboard_update`，入口第一件事就是拿
/// `GetClipboardSequenceNumber()` 与 `LAST_SEQ` 比对，先到者处理、后到者直接
/// 返回，所以两条通道并存不会重复入库。
fn spawn_clipboard_watchdog() {
    std::thread::Builder::new()
        .name("clipboard-watchdog".into())
        .spawn(|| {
            // 1.5s 间隔：兼顾「后台不丢」与「几乎不耗 CPU」。
            // 本线程只负责补上窗口消息漏掉的那部分（后台/最小化/锁屏时
            // WM_CLIPBOARDUPDATE 不保证投递）。延迟渲染那类「取不到数据」已交给
            // 一次性补读线程（RETRY_DELAY_MS），所以这里维持原频率即可。
            // 开销是每 1.5s 一次整数读取，可忽略。
            const INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
            loop {
                std::thread::sleep(INTERVAL);
                let seq = unsafe { GetClipboardSequenceNumber() };
                if seq == 0 {
                    continue;
                }
                // 只有「有变化且主线程尚未处理过」才补刀。
                // seq == LAST_SEQ 说明消息通道已经处理过了（绝大多数情况）。
                if seq == LAST_SEQ.load(Ordering::Relaxed) {
                    continue;
                }
                crate::logger::write_line(
                    "INFO",
                    "clipboard",
                    "兜底轮询捕获到漏掉的剪贴板变化（窗口消息未送达）",
                );
                on_clipboard_update();
            }
        })
        .ok();
}

fn spawn_worker() {
    // app 走全局 APP（OnceLock，在 init 首行写入）；worker 在订阅监听之前
    // 启动，所以取用时必定已就绪。
    let (tx, rx) = std::sync::mpsc::channel::<(u64, RawClip)>();
    std::thread::Builder::new()
        .name("clipboard-store".into())
        .spawn(move || {
            while let Ok((captured_ms, raw)) = rx.recv() {
                // guard 保证 PENDING 一定减回去 —— handle_clip 里判重拦截会
                // 提前 return，手写 fetch_sub 迟早漏掉一次。
                let _guard = PendingGuard;
                // handle_clip 内部完成：解码 → 同图判重 → 落盘 → emit。
                // 这里只负责串行投递，保证与复制顺序一致。
                handle_clip(captured_ms, raw);
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
    // 存储格式升级：把「加密落盘」上线之前写入的**明文**实体就地加密。
    // 幂等（已加密的带 ENC_MAGIC 前缀，直接跳过），所以每次启动扫一遍也不会有副作用。
    let encrypted = encrypt_legacy_entities(&dir);
    // 老条目的 preview 是在「只取首行」的旧规则下写入的 —— 首行短就只显示几个字。
    // 规则改成「开头最多 2 行」后必须**重算存量**，否则用户看到的还是旧预览，
    // 会以为修复没生效。
    let refreshed = refresh_text_previews(&dir, &mut hist);
    if backfilled > 0 {
        crate::log_print!("[clipboard] 已为 {backfilled} 条历史文件条目补写 files/ 清单");
    }
    if encrypted > 0 {
        crate::log_print!("[clipboard] 已将 {encrypted} 个历史明文文本实体转为加密存储");
    }
    if refreshed > 0 {
        crate::log_print!("[clipboard] 已按新规则重算 {refreshed} 条文本条目的预览");
    }
    if backfilled > 0 || refreshed > 0 {
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
        let hwnd = MAIN_HWND.get().copied().unwrap_or(0);
        if AddClipboardFormatListener(hwnd) == 0 {
            crate::log_warn!(
                "[clipboard] AddClipboardFormatListener 失败 hwnd={hwnd} last_err={}",
                GetLastError()
            );
        } else {
            crate::log_print!("[clipboard] 已订阅 WM_CLIPBOARDUPDATE（事件驱动监听）");
        }
    }

    // 播种当前剪贴板序号。
    //
    // LAST_SEQ 的初值是 0，不播种的话兜底轮询的**第一个**心跳必然判定「序号变了」，
    // 于是把「启动之前就已经在剪贴板里」的内容当成一次新复制，再记一条 ——
    // 实测同一张图因此被重复记了 6 次（每次重启多一条）。
    // 播种之后，轮询只在「运行期间真的漏掉了通知」时才触发，启动噪音随之消失。
    unsafe {
        let seq = GetClipboardSequenceNumber();
        if seq != 0 {
            LAST_SEQ.store(seq, Ordering::Relaxed);
        }
    }

    // 兜底轮询：窗口消息**不是可靠通道**。
    //
    // 用户反馈「软件在后台运行时（窗口开着但最小化/不在最前），任何复制的东西
    // 都可能不进软件」。原因是主路径依赖 WM_CLIPBOARDUPDATE，而它只在
    // **主窗口 WndProc** 里被处理（见 effects.rs 的 hook_proc），窗口不可见时
    // 消息投递本身就不保证 —— 最小化、锁屏、部分 DWM 状态下系统会跳过或延迟
    // 投递该消息，事件也就永久丢失（该通知不会补发）。
    //
    // 所以再加一条独立的低频兜底：定期比对 `GetClipboardSequenceNumber`，
    // 发现变化且主线程没处理过就补一次。序号比对天然幂等 ——
    // 两条通道谁先谁后都只会落一条（LAST_SEQ 互斥）。
    spawn_clipboard_watchdog();

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
/// 延迟渲染「一次性补读」的等待时间（ms）。想调成 300ms 只需改这一个常量。
/// 快速重试（15/30/60ms，累计 ~105ms）已兜住绝大多数；这里再补一拍，
/// 兜住「对方渲染特别慢」的少数情况。
const RETRY_DELAY_MS: u64 = 300;

pub fn on_clipboard_update() {
    let seq = unsafe { GetClipboardSequenceNumber() };
    if seq == 0 || seq == LAST_SEQ.load(Ordering::Relaxed) {
        return;
    }
    // 记下「上一个」序号。只有延迟渲染没交货时才用得上：那时要把 LAST_SEQ 退回这个值，
    // 好让看门狗线程下一拍认定「序号变了」并重读一次。
    let prev_seq = LAST_SEQ.load(Ordering::Relaxed);
    unsafe {
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

    let (raw, diag, render_pending) = unsafe { read_clipboard() };
    let Some(raw) = raw else {
        // 剪贴板序号确实变了（真有一次外部复制），但没读出可记录的内容。
        // 常见于：关闭「文件复制」时复制单个非图片文件、对方只写了私有格式。
        //
        // 记一条是有价值的：用户反馈「复制了却没进软件」时，
        // 有这条 = 「事件到了、是识别环节没捞到」；没这条 = 「事件根本没到」
        // （查监听是否订阅成功、窗口是否最小化导致消息不投递）。
        // 两者的排查方向完全不同。
        //
        // 2.2.1 起把「当时剪贴板里有哪些格式、每种为什么被拒」一并打进日志（diag），
        // 这样朋友回传的日志就能直接看出是 CF_UNICODETEXT 根本没出现、还是出现了
        // 但 GetClipboardData 取不到句柄（延迟渲染竞态）等，无需再来回猜。

        // 延迟渲染还没交货（格式登记了、句柄一直拿不到）：这次**不能**算处理过。
        // 把序号退回上一个值，让已有的看门狗线程下一拍重读一次。
        //
        // 这正是 V1（Electron）从不出这个问题的原因：它每秒 readText/readImage 轮询一次
        // （POLL_INTERVAL = 1000），撞上延迟渲染那一拍取不到，下一拍自然补上，
        // 压根不依赖「只发一次」的通知。V2 改成事件驱动后 WM_CLIPBOARDUPDATE 只发一次、
        // 不重发，所以必须有这条回退，否则这一次复制就永久丢了。
        if render_pending && RETRY_SEQ.load(Ordering::Relaxed) != seq {
            RETRY_SEQ.store(seq, Ordering::Relaxed);
            LAST_SEQ.store(prev_seq, Ordering::Relaxed);
            crate::logger::write_line(
                "INFO",
                "clipboard",
                &format!("延迟渲染未交货，{}ms 后一次性补读 | {}", RETRY_DELAY_MS, diag),
            );
            // 一次性补读线程：等渲染窗口过去再读一次，读完立刻退出。
            // 它只在这一次「确实读失败」时才存在，平时零开销 ——
            // 常驻的仍然只有看门狗那一个线程（负责后台/最小化时消息不投递的兜底）。
            //
            // 上面已把 LAST_SEQ 退回 prev_seq，所以这次调用才会被判定为「序号变了」
            // 而真正重读；否则会被去重逻辑直接跳过。
            std::thread::Builder::new()
                .name("clipboard-retry".into())
                .spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
                    on_clipboard_update();
                })
                .ok();
            return;
        }

        crate::logger::write_line(
            "INFO",
            "clipboard",
            &format!("剪贴板已更新，但未识别出可记录内容 | {}", diag),
        );
        return;
    };
    // 读到了（或已放弃补读）：清掉补读标记。
    RETRY_SEQ.store(0, Ordering::Relaxed);

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
    //
    // ⚠️ **时间戳必须在这里打**（捕获时刻），不能到 worker 里再取当前时间。
    // worker 是单线程串行的，前面压着一张大图的解码时，本条要排队几秒才轮到；
    // 那时再取时间，判重窗口算出来的「间隔」就是「排队时长 + 真实间隔」，
    // 完全失真 —— 实测日志里出现过「同一秒、同一指纹、却记了两条」，
    // 根因就在这里。
    let captured_ms = now_ms();
    if let Some(tx) = WORKER.get() {
        if tx.send((captured_ms, raw)).is_ok() {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // 工作线程还没起来（正常不会发生——init 里先启线程再订阅监听）
        handle_clip(captured_ms, raw);
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
            let n = PENDING.load(Ordering::Relaxed);
            crate::log_warn!(
                "[clipboard] 退出等待超时，仍有 {n} 条未落盘，强制退出（可能丢失未保存记录）"
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
fn handle_clip(captured_ms: u64, raw: RawClip) {
    let Some(raw) = resolve(raw) else {
        // resolve 只在「临时文件读不出」或「不是图片」且「文件复制」关闭时返回 None，
        // 即按用户设置主动跳过。记一条，便于区分「被设置跳过」与「识别失败」。
        crate::logger::write_line(
            "INFO",
            "clipboard",
            "内容按设置跳过（非图片文件且未开启文件复制）",
        );
        return;
    };

    // 图片判重：微信等应用复制一张图会**反复**改写剪贴板。
    //
    // 实测（2.1.11 日志）：同一张图被写入 8 次，跨越 81 秒 ——
    // 06:30:19/23/31/35/46/50、06:31:36/40。若不合并，历史里会落下 8 条
    // 一样的记录，每条都要做一次秒级解码+写盘，卡顿被放大数倍。
    //
    // 判据演进与踩过的坑：
    //   1. 「宽高 + 800ms」→ 固定区域截图（宽高必然相同）被误杀。
    //   2. 「像素指纹 + 800ms」→ 微信补写间隔 4~46s，远超窗口，拦不住。
    //   3. 「队列 + 90s 窗口」→ 又踩两个坑：
    //      a. 用 `.max()` 取了**最远**的那次匹配，间隔恒偏大，阈值形同虚设；
    //      b. 用 worker 的**处理时刻**，而 worker 被大图解码堵住时该时刻已失真
    //         （实测「同一秒、同一指纹、记了两条」）。
    //   4. 现在：**捕获时刻** + 只比**最近一条** + 命中时**滑动刷新时间戳**。
    //
    // 为什么「只比最近一条」就够：微信的补写是**连续**的（上一条就是同一张图）。
    // 与整个队列比对反而会误伤 A→B→A 里的第二个 A（用户确实复制了两次 A，
    // 中间夹了 B，这是**正常操作**，必须放行）。
    //
    // 为什么命中要**刷新时间戳**：这是「连续复制仍出现两张」的根因。
    // 若命中只 return 不更新时间，基准永远停在第一次，只要用户持续复制，
    // gap 就会一路累加，涨过阈值后又冒出一条。刷新后，只要同一张图持续到来，
    // 它们始终落在同一簇里。
    // 判重：两种形态各有各的指纹来源，但都**不需要解码**。
    //   - ImageOriginal（微信走的就是这条）：文件内容哈希（FNV-1a）
    //   - Image（CF_DIB 原始位图，本机实测微信不走）：像素量化指纹
    let fp = match &raw {
        RawClip::ImageOriginal { hash, .. } => Some(*hash),
        RawClip::Image(dib) => Some(image_fingerprint(dib)),
        _ => None,
    };
    if let Some(fp) = fp {
        if let Ok(mut g) = LAST_IMG.lock() {
            if dedup_merge(&mut g, fp, captured_ms) {
                crate::logger::write_line(
                    "INFO",
                    "clipboard",
                    "重复图片已合并（与上一条指纹一致，滑动窗口续期）",
                );
                return;
            }
        }
    }

    let item = store_raw(raw);
    if let Some(app) = APP.get() {
        let _ = app.emit("clipboard-update", item);
    }
}

/// 判重决策（抽成独立函数，便于单测**直接验证真实实现**而不是在测试里重写一遍）。
///
/// 返回 `true` = 判为重复、应合并（调用方直接 return，不落盘）。
///
/// 规则：**只与队列最后一条比对**（微信补写必然紧挨着上一条）；命中则把它的
/// 时间戳刷新为 `now`（滑动续期），使持续到来的同图始终保持在同一簇里。
/// 未命中则入队并清理过期项。
fn dedup_merge(g: &mut Vec<(u64, u64)>, fp: u64, now: u64) -> bool {
    if let Some(last) = g.last_mut() {
        if last.0 == fp && now.saturating_sub(last.1) < DUP_WINDOW_MS {
            last.1 = now; // 滑动窗口：续上这一簇
            return true;
        }
    }
    g.push((fp, now));
    g.retain(|(_, t)| now.saturating_sub(*t) < DUP_WINDOW_MS);
    let len = g.len();
    if len > LAST_IMG_KEEP {
        g.drain(0..len - LAST_IMG_KEEP);
    }
    false
}

/// 把「待处理」的剪贴板内容整理成可存储形态。
///
/// **关键：CF_HDROP 图片不再解码**。只读出文件字节、嗅探真实格式、算内容哈希。
/// 解码要 495ms（实测），而解码产物在捕获那一刻没人需要 —— 缩略图是懒加载、
/// 回贴要用户点击。原样存盘后，「复制 → 出现」只剩「读文件 + 哈希 + 写文件」。
fn resolve(raw: RawClip) -> Option<RawClip> {
    match raw {
        RawClip::ImageFile(path) => {
            // 微信给的临时图片文件随时可能被它自己清掉。读字节推迟到后台线程后，
            // 从「读到路径」到「真正打开文件」之间隔了一段（主线程还要处理后续
            // 消息），这个窗口足够让文件消失 —— 于是图片莫名其妙就没了。
            // 读不到时重试几次。
            let mut bytes = None;
            for attempt in 0..3u32 {
                // ⚠️ 这里读的是**原文件字节**（微信给的多是 JPEG），不是解码结果。
                bytes = std::fs::read(&path).ok().filter(|b| !b.is_empty());
                if bytes.is_some() {
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
            let Some(bytes) = bytes else {
                // 确实读不出来（文件已被清理/无权限）。此时无法判断它是不是图片，
                // 按「文件」记录只对开了文件复制的用户有意义；关闭时直接放弃。
                return if crate::settings::get().file_copy {
                    Some(RawClip::Files(vec![path.to_string_lossy().to_string()]))
                } else {
                    None
                };
            };

            // 判定「这是不是图片、是什么格式」。**不限制格式** —— 见 detect_image_ext。
            let Some(ext) = detect_image_ext(&bytes, &path) else {
                // 不是图片（比如用户复制了单个 .txt/.pdf 文件）。
                // 这类属于「文件复制」范畴，**听开关的** —— 开关关闭时不记录。
                // （图片不受开关影响，上面几行已经处理完并返回了。）
                return if crate::settings::get().file_copy {
                    Some(RawClip::Files(vec![path.to_string_lossy().to_string()]))
                } else {
                    None
                };
            };
            let hash = content_hash(&bytes);
            Some(RawClip::ImageOriginal { bytes, ext, hash })
        }
        RawClip::ImageRaw(bytes) => Some(RawClip::Image(clamp_dib(bytes))),
        other => Some(other),
    }
}

/// 判断文件是否为图片、以及它的规范扩展名。**不限制格式。**
///
/// 分三级判定，从「最可信」到「最宽松」：
///   1. **交给 `image` 库按魔数识别** —— 覆盖所有已启用的编解码器
///      （png/jpeg/gif/bmp/webp/ico/tiff/tga/dds/pnm/hdr/qoi/ff）。
///      库认的格式自动跟随 Cargo.toml 的 feature，无需在此维护白名单。
///   2. **补充几个库不认但常见的魔数**（tiff/ico/cur/psd/avif/heic…）。
///      这一步只影响「识别」，不影响解码；识别出来的图会原样存盘。
///   3. **退回文件扩展名**，专为 SVG 这类**文本格式**准备 —— 它们没有二进制
///      魔数，第 1、2 步都认不出来，但确实是图片。
///
/// 为什么不只用扩展名：微信/QQ 的剪贴板临时文件名是**内容哈希**、没有可信后缀，
/// 按后缀判会漏掉真图片（这正是 2.1.11 反馈的「微信复制图片识别不到」）。
/// 但也不能只用魔数：SVG/PSD 之类会被误判成非图片。
/// 两者结合才既不漏也不误。
fn detect_image_ext(bytes: &[u8], path: &std::path::Path) -> Option<String> {
    // 1) 交给 image 库（魔数识别，最可信）
    if let Ok(fmt) = image::guess_format(bytes) {
        if let Some(e) = fmt.extensions_str().first() {
            return Some((*e).to_string());
        }
    }

    // 2) 库不认的常见魔数
    if let Some(e) = extra_magic_ext(bytes) {
        return Some(e.to_string());
    }

    // 3) 文本类图片（SVG 等）只能看扩展名
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if TEXT_IMAGE_EXTS.contains(&ext.as_str()) {
            return Some(ext);
        }
    }
    None
}

/// 库不认、但值得识别的图片格式魔数。
///
/// 这里只做「是不是图片」的判定（用于决定按图片还是按文件记录），
/// 不承诺能解码 —— 解不开的格式在缩略图处会退回占位、在回贴处退回按文件复制。
fn extra_magic_ext(b: &[u8]) -> Option<&'static str> {
    if b.len() < 12 {
        return None;
    }
    // TIFF：小端 "II*\0" / 大端 "MM\0*"
    if b.starts_with(b"II\x2A\x00") || b.starts_with(b"MM\x00\x2A") {
        return Some("tiff");
    }
    // ICO / CUR
    if b.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("ico");
    }
    if b.starts_with(&[0x00, 0x00, 0x02, 0x00]) {
        return Some("cur");
    }
    // Photoshop: "8BPS"
    if b.starts_with(b"8BPS") {
        return Some("psd");
    }
    // ISO BMFF 家族（AVIF / HEIC）：第 4..8 字节是 "ftyp"，8..12 是品牌
    if &b[4..8] == b"ftyp" {
        let brand = &b[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some("avif");
        }
        if matches!(
            brand,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
        ) {
            return Some("heic");
        }
    }
    None
}

/// 没有二进制魔数、只能靠扩展名识别的图片格式（主要是矢量/文本类）。
const TEXT_IMAGE_EXTS: [&str; 4] = ["svg", "svgz", "xbm", "xpm"];

/// 文件内容哈希（FNV-1a 64 位）。
///
/// 用途是**判重**：微信把同一张图反复写进剪贴板时，临时文件名与内容都完全一致
/// （文件名就是内容哈希），所以「字节相同」是比「像素指纹相同」更省、更准的判据 ——
/// 不需要解码就能得到。
///
/// 大文件全量哈希：4096×3072 的 JPEG 约 4.7MB，FNV 遍历约 5ms，可接受。
/// 若将来要处理几十 MB 的图，可改为「采样哈希」（头/中/尾各取几 KB）。
fn content_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ---------------------------------------------------------------------------
// 读取剪贴板
// ---------------------------------------------------------------------------

enum RawClip {
    Text(String),
    Image(Vec<u8>), // DIB 字节
    Files(Vec<String>),
    /// CF_HDROP 给的图片**原文件路径**（尚未读取）。
    ImageFile(PathBuf),
    /// 待处理的原始 CF_DIB 字节（可能超过尺寸上限）。
    /// 降采样同样只在后台线程做。
    ImageRaw(Vec<u8>),
    /// CF_HDROP 图片的**原文件字节**（已读入，尚未解码）。
    ///
    /// 这是「无感复制」的关键：**原样落盘，捕获路径上不做任何解码**。
    /// 解码要 495ms（实测 4096×3072 JPEG：解码 55ms + 降采样 319ms + 拼 DIB），
    /// 而解码结果（12MB BMP）在捕获那一刻**没有任何人需要** ——
    /// 列表只用 512px 缩略图（前端本就懒加载），回贴要用户点击才发生。
    /// 两者都可以推迟到真正需要时再解码，于是「复制 → 出现」只剩
    /// 「读文件 + 哈希 + 写文件」，十几毫秒。
    ImageOriginal {
        bytes: Vec<u8>,
        /// 落盘用的扩展名（按图片魔数判定，不信任原文件名）。
        ext: String,
        /// 文件字节的内容哈希，用于判重 —— **无需解码**即可判重。
        hash: u64,
    },
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

/// 标准剪贴板格式值 → 可读名（>= 0xC000 的私有格式不在这里，交给 GetClipboardFormatNameW）。
fn std_cf_name(fmt: u32) -> Option<&'static str> {
    Some(match fmt {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        4 => "CF_SYLK",
        5 => "CF_DIF",
        6 => "CF_TIFF",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        9 => "CF_PALETTE",
        10 => "CF_PENDATA",
        11 => "CF_RIFF",
        12 => "CF_WAVE",
        13 => "CF_UNICODETEXT",
        14 => "CF_ENHMETAFILE",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        0x80 => "CF_OWNERDISPLAY",
        0x81 => "CF_DSPTEXT",
        0x82 => "CF_DSPBITMAP",
        0x83 => "CF_DSPMETAFILEPICT",
        0x84 => "CF_DSPENHMETAFILE",
        _ => return None,
    })
}

/// 枚举当前剪贴板上的所有格式（须 OpenClipboard 之后调用）。
/// 标准格式给可读名；私有格式用 GetClipboardFormatNameW 取注册名，取不到则标 <priv:N>。
unsafe fn enum_clipboard_formats() -> Vec<String> {
    let mut out = Vec::new();
    let mut fmt = 0u32;
    loop {
        fmt = EnumClipboardFormats(fmt);
        if fmt == 0 {
            break;
        }
        let name = std_cf_name(fmt)
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let mut buf = vec![0u16; 256];
                let n = GetClipboardFormatNameW(fmt, buf.as_mut_ptr(), buf.len() as i32);
                if n > 0 {
                    format!("\"{}\"", String::from_utf16_lossy(&buf[..n as usize]))
                } else {
                    format!("<priv:{}>", fmt)
                }
            });
        out.push(name);
    }
    out
}

/// 尽力拿到当前剪贴板 owner 的进程名（如 WeChat.exe）。任何一步失败都返回空串，
/// 绝不影响主流程——这只是一个诊断线索，用来确认「这次复制是谁写进剪贴板的」。
unsafe fn clipboard_owner_exe() -> String {
    let owner = GetClipboardOwner();
    if owner == 0 {
        return String::new();
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(owner, &mut pid);
    if pid == 0 {
        return String::new();
    }
    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const PROCESS_VM_READ: u32 = 0x0010;
    let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
    if h == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; 260];
    let mut size: u32 = buf.len() as u32;
    let name = if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) != 0 {
        let s = String::from_utf16_lossy(&buf[..size as usize]);
        std::path::Path::new(&s)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or(s)
    } else {
        String::new()
    };
    CloseHandle(h);
    name
}

/// 读取剪贴板内容；同时返回一份「诊断串」，在「未识别」时由调用方打印，
/// 说明当时剪贴板里到底有哪些格式、每种为什么被拒（owner / 格式清单 / 文本句柄是否为空等）。
/// 这是为定位「微信复制不进软件」这类偶发问题加的可观测性，平时不影响主流程。
///
/// ⚠️ 2.2.1 修复「小图复制丢失」：**格式登记 ≠ 数据已就位**。
///
/// 微信/QQ 复制图片时常以「延迟渲染」登记 CF_HDROP（`SetClipboardData(CF_HDROP, NULL)`）：
/// 格式立刻可见（`IsClipboardFormatAvailable` 为真），真正的数据要等有人索取时，
/// 由 owner 在 WM_RENDERFORMAT 里补交。而 WM_CLIPBOARDUPDATE 是在**登记**的瞬间就
/// 发出的，所以我们第一次去读的时候数据往往还没补上 → `GetClipboardData` 返回 NULL。
///
/// 更要命的是：延迟渲染需要 owner 重新 OpenClipboard + SetClipboardData 才能补数据，
/// 而剪贴板是全局独占锁 —— **我们持锁期间 owner 根本补不进来**。所以持锁重试毫无意义，
/// 必须先 CloseClipboard 松锁、退避片刻让 owner 补完，再重新开锁读。
///
/// 这正是「大图正常、小图丢失」的原因：大图微信得先把临时文件写好（耗时），等我们
/// （含 open_clipboard_retry 的退避）拿到锁时数据早已提交，一次读到；小图登记几乎瞬时
/// 但走延迟渲染，第一次读正好撞在补交完成之前。旧实现只读一次就把这次复制永久丢了
/// （WM_CLIPBOARDUPDATE 不会重发）。
/// 第三个返回值：是否是「格式登记了、但句柄一直拿不到」（延迟渲染没交货）。
/// 为 true 时调用方不应把它当成已处理，应交回看门狗下一拍重读。
unsafe fn read_clipboard() -> (Option<RawClip>, String, bool) {
    // 最多读几轮。只有「格式在、句柄为 NULL」这种可疑情况才用得上第 2 轮起，
    // 正常复制第一轮就返回，不会有任何额外延迟。
    const MAX_ATTEMPTS: u32 = 4;

    // 「文件复制」开关（设置页，默认关闭）：关掉后完全不看 CF_HDROP。
    // 在 OpenClipboard 之前读取，避免持有剪贴板锁时再去碰 settings 的互斥锁。
    let file_copy = crate::settings::get().file_copy;

    let mut diag = String::new();
    for attempt in 0..MAX_ATTEMPTS {
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
            return (
                None,
                "OpenClipboard 重试耗尽，放弃本次(对方应用长时间持锁)".into(),
                false,
            );
        }

        let (raw, attempt_diag, suspicious) = read_clipboard_once(file_copy);
        // 必须立刻松锁：延迟渲染要靠 owner 重新拿到锁才能把数据补进来，
        // 我们一直攥着锁的话它永远补不进来，后面的重试也就全白搭了。
        CloseClipboard();

        if raw.is_some() {
            if attempt > 0 {
                // 救回来了。记一条，回传日志时能看出「这次是被重试捞到的」，
                // 也能据此判断对方应用的延迟渲染窗口到底有多长。
                crate::logger::write_line(
                    "INFO",
                    "clipboard",
                    &format!(
                        "延迟渲染竞态：首轮未取到数据，第 {} 轮重试成功 | {}",
                        attempt + 1,
                        attempt_diag
                    ),
                );
            }
            return (raw, attempt_diag, false);
        }

        diag = attempt_diag;
        if !suspicious {
            // 剪贴板里压根没有目标格式（或按 file_copy 开关主动跳过）：
            // 再等多久也不会有，没必要白等。
            return (None, diag, false);
        }

        if attempt + 1 < MAX_ATTEMPTS {
            // 15 / 30 / 60ms 退避，累计 ≤105ms。只有可疑时才付这点代价。
            std::thread::sleep(std::time::Duration::from_millis(15u64 << attempt));
        }
    }
    (
        None,
        format!("[延迟渲染重试 {} 轮仍取不到数据] {}", MAX_ATTEMPTS, diag),
        true, // 仍卡在「格式在、句柄拿不到」→ 交回看门狗下一拍重读
    )
}

/// 单次读剪贴板。**调用方须保证已 OpenClipboard 成功**，本函数不负责开关剪贴板。
///
/// 返回 `(内容, 诊断串, 是否可疑)`：
/// 「可疑」= 至少一种目标格式 `IsClipboardFormatAvailable` 为真、但 `GetClipboardData`
/// 拿到 NULL —— 这是延迟渲染 / 分次写入的典型症状，值得松锁退避后重读；
/// 若压根没有目标格式，说明这次真没有可记录的东西，重读没有意义。
unsafe fn read_clipboard_once(file_copy: bool) -> (Option<RawClip>, String, bool) {
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;

    // 诊断：先枚举当前剪贴板上的所有格式（含私有格式注册名），再尽力拿到 owner 进程名。
    // 两者都必须在 OpenClipboard 成功之后、CloseClipboard 之前调用。
    let mut diag = String::new();
    let owner = clipboard_owner_exe();
    if !owner.is_empty() {
        diag.push_str(&format!("[owner={}] ", owner));
    }
    let fmts = enum_clipboard_formats();
    diag.push_str(&format!("formats=[{}] ", fmts.join(", ")));

    let mut suspicious = false;

    let result = (|| {
        // 优先级：文本 > 文件 > 图片
        if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
            diag.push_str("text:avail=true ");
            if let Some(h) = non_null(GetClipboardData(CF_UNICODETEXT)) {
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    diag.push_str("GlobalLock=NULL ");
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
                diag.push_str(&format!("len={} ", v.len()));
                return Some(RawClip::Text(String::from_utf16_lossy(&v)));
            } else {
                // 格式存在但取不到句柄：典型「延迟渲染未就绪」——owner 还没通过
                // WM_RENDERFORMAT 把数据交出来，或 owner 拒绝渲染。这是微信等应用
                // 复制偶发丢失的高频嫌疑点，配合上方的 formats 清单可一眼确认。
                suspicious = true;
                diag.push_str("GetClipboardData=NULL(延迟渲染未就绪) ");
            }
        } else {
            diag.push_str("text:avail=false ");
        }

        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            diag.push_str("hdrop:avail=true ");
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
                // ⚠️ 这里**绝对不能**调用 DragFinish(h)（剪贴板 HDROP 二次释放会崩溃，
                // 详见文件下方 extern 块说明）。内存由 CloseClipboard 后剪贴板自身管理。
                if files.len() == 1 {
                    // 只记住路径，解码推迟到后台线程 —— 主线程（WndProc）不能
                    // 被解码大图拖住，否则界面直接冻住。
                    // 单图/单文件图片识别**不受** file_copy 开关影响（2.1.11 反馈）。
                    diag.push_str("单图/单文件(图片识别不受 file_copy 影响) ");
                    return Some(RawClip::ImageFile(PathBuf::from(&files[0])));
                }
                // 多张 / 非图片：属于「文件复制」范畴，听开关的。
                if file_copy {
                    diag.push_str(&format!("多文件({})且file_copy=on ", files.len()));
                    return Some(RawClip::Files(files));
                }
                // 单个非图片文件且开关关闭：不记录。
                diag.push_str(&format!("多文件({})但file_copy=off→跳过 ", files.len()));
                return None;
            } else {
                suspicious = true;
                diag.push_str("GetClipboardData=NULL(延迟渲染未就绪) ");
            }
        } else {
            diag.push_str("hdrop:avail=false ");
        }

        if IsClipboardFormatAvailable(CF_DIB) != 0 {
            diag.push_str("dib:avail=true ");
            if let Some(h) = non_null(GetClipboardData(CF_DIB)) {
                let size = GlobalSize(h);
                let p = GlobalLock(h);
                if !p.is_null() && size > 0 {
                    let bytes = std::slice::from_raw_parts(p as *const u8, size as usize).to_vec();
                    GlobalUnlock(h);
                    // 与 image_file_to_dib 同一上限：CF_DIB 也可能来自超大位图
                    // （微信第二次写入、专业软件复制），超限降采样，防巨型 BMP 落盘。
                    // 降采样推迟到后台线程，主线程只做完这次 memcpy 就返回。
                    diag.push_str(&format!("size={} ", size));
                    return Some(RawClip::ImageRaw(bytes));
                }
                diag.push_str("GlobalLock=NULL或size=0 ");
            } else {
                suspicious = true;
                diag.push_str("GetClipboardData=NULL(延迟渲染未就绪) ");
            }
        } else {
            diag.push_str("dib:avail=false ");
        }
        None
    })();

    (result, diag, suspicious)
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
/// 屏幕上并看不出差别。
///
/// 2.1.11 把上限从 4096 降到 2048：4096 的门槛意味着 4096×3000 这类「常见手机
/// 横图」恰好卡在限内、原样落 50MB（实测日志里反复出现 size=50331688）。
/// 前端把这种条目 base64 后是 67MB 字符串，WebView2 加载必然失败 → 用户看到
/// 破图占位（截图里那条带「图片」字样的破图条目）；缩略图还要把整张 50MB 再
/// 完整解码一次才能生成，页面因此卡死数秒。
///
/// 剪贴板历史的用途是「刚复制过、回头找得到」，2048 长边（≈4M 像素、BMP 16MB
/// 封顶）在屏幕上与原图肉眼无差，却让前端负载降到 1/4。
const MAX_SIDE: u32 = 2048;

/// 图片判重的滑动窗口（毫秒）—— 衡量的是**相邻两次同图写入的间隔**。
///
/// 取值来自实测。微信把同一张图反复写进剪贴板，相邻间隔实测为：
/// 4s / 8s / 4s / 11s / 4s / **46s** / 4s。注意最后那个 46 秒 ——
/// 窗口必须大于它，否则这一簇会从中间断开、后半段又落一条重复记录。
///
/// 与上一版的区别：**窗口不再需要覆盖整个跨度**。现在命中一次就刷新时间戳
/// （滑动续期），所以只要相邻间隔在窗口内，整簇就会一直延续；上一版不刷新，
/// 窗口才被迫放大到 90s 去兜住 81s 的总跨度。
///
/// 取 60s：覆盖实测最大相邻间隔 46s，并留出余量；同时明显短于「人主动重复
/// 截同一画面」的常见间隔，减少误合并。
const DUP_WINDOW_MS: u64 = 60_000;

/// 相邻两次同图写入的最大实测间隔（46s）+ 余量，供单测断言窗口确实覆盖得住。
/// 抽成常量是为了让「窗口够不够大」这件事在测试里显式、可校验。
#[cfg(test)]
const OBSERVED_MAX_ADJACENT_GAP_MS: u64 = 46_000;

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

/// 距上一次键鼠输入过了多少毫秒。取不到时返回 u64::MAX（视作「不确定」）。
///
/// ⚠️ 当前**没有调用点**（保留是因为 2.1.11 期间反复用到，删了下次还要重写）。
///
/// 曾经用它区分「应用后台补写」与「用户主动操作」，实测证明**行不通**：
/// 用户按下复制键后，应用在几秒内就补写，此时「距上次输入」必然很短 ——
/// 两个信号在时间上完全重叠，无法区分。判重最终改用「捕获时刻 + 滑动窗口」，
/// 不再需要输入活跃度。若将来要用，先想清楚这一点。
#[allow(dead_code)]
fn idle_ms() -> u64 {
    unsafe {
        let mut li = LastInputInfo {
            cb_size: std::mem::size_of::<LastInputInfo>() as u32,
            dw_time: 0,
        };
        if GetLastInputInfo(&mut li) == 0 {
            return u64::MAX;
        }
        // GetTickCount 约 49.7 天回绕，这里按 u32 自然回绕做减法（wrapping 语义）
        GetTickCount().wrapping_sub(li.dw_time) as u64
    }
}

fn image_file_to_dib(path: &std::path::Path, max_side: u32) -> Option<Vec<u8>> {
    // 缓存命中判定：同一路径 + 文件的 (mtime, size) 未变 → 说明还是同一张图，
    // 直接复用上次解码结果，省掉一次完整的 open + 解码 + 缩放（4096×3072 约 2 秒）。
    // 用 mtime/size 而不是仅路径：微信的 RWTemp 目录里路径可复用，内容会换。
    let stamp = std::fs::metadata(path).ok().map(|m| {
        let t = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        (t ^ (m.len().wrapping_mul(31) << 1), m.len())
    });
    if let Some(st) = stamp {
        let hit = DECODE_CACHE.lock().ok().and_then(|mut g| {
            // 命中就把它挪到末尾（LRU：末尾 = 最新），再克隆返回。
            let idx = g.iter().position(|(p, s, _)| p == path && *s == st)?;
            let hit = g.remove(idx);
            g.push(hit.clone());
            Some(hit.2)
        });
        if let Some(d) = hit {
            return Some(d);
        }
    }

    let dib = decode_image_file(path, max_side)?;

    if let Some(st) = stamp {
        if let Ok(mut g) = DECODE_CACHE.lock() {
            // 同路径但内容变了（st 不同）→ 先剔除旧的那条，避免占位。
            g.retain(|(p, s, _)| !(p == path && *s == st));
            g.push((path.to_path_buf(), st, dib.clone()));
            let len = g.len();
            if len > DECODE_CACHE_CAP {
                g.drain(0..len - DECODE_CACHE_CAP);
            }
        }
    }
    Some(dib)
}

/// 真正做解码（`image_file_to_dib` 的未缓存部分）。拆出来是为了让缓存逻辑
/// 保持单一职责，也便于将来单独测试。
fn decode_image_file(path: &std::path::Path, max_side: u32) -> Option<Vec<u8>> {
    // ⚠️ 这里**不按扩展名过滤**。
    //
    // 改动前有一份 6 项扩展名白名单（png/jpg/jpeg/bmp/gif/webp），加上一条
    // 「不认识的后缀就拒绝」的规则 —— 于是 tiff、ico、svg、heic 等一律被拒。
    // 格式不该被限制：交给 image 库判定，能解的自然成功，解不开的自然返回 None。
    // 「是不是图片」的判定已在 capture 阶段由 detect_image_ext 完成，
    // 到这里只需回答「能不能解出来」。
    let (w0, h0) = image::image_dimensions(path).ok()?;
    let img = if w0.max(h0) > max_side {
        // 等比算出目标尺寸（thumbnail 的语义：只缩不放）
        let ratio = max_side as f32 / w0.max(h0) as f32;
        let tw = ((w0 as f32 * ratio).round() as u32).max(1);
        let th = ((h0 as f32 * ratio).round() as u32).max(1);
        crate::logger::write_line(
            "INFO",
            "clipboard",
            &format!(
                "图片 {} 超 {max_side}px，降采样 {w0}×{h0} → {tw}×{th} 存储",
                path.display()
            ),
        );
        // 最近邻缩放：跳过插值开销。缩略图/回贴场景足够。
        image::open(path)
            .ok()?
            .resize_exact(tw, th, image::imageops::FilterType::Nearest)
    } else {
        image::open(path).ok()?
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

    // 像素搬运：**一次性切片转换**，不用 `get_pixel` 双重循环。
    //
    // 改动前是 `for y in (0..h).rev() { for x in 0..w { get_pixel + 4×push } }`：
    // 一张 4096×3072 的图要走 1200 万次循环，每像素还要过 4 次带边界检查的
    // `get_pixel` 和 4 次 `push` —— 实测这是大图处理里最大的一块纯 CPU 开销。
    //
    // 语义等价：DIB 要求自下而上（末行在最前）、每像素 BGRA。
    // 按行倒序遍历源 RGBA，行内把 RGBA 交换成 BGRA。
    let src = rgba.as_raw();
    let row_bytes = (w as usize) * 4;
    let total = row_bytes * (h as usize);
    if src.len() < total {
        return None;
    }
    dib.reserve_exact(total);
    for y in (0..h as usize).rev() {
        let row = &src[y * row_bytes..(y + 1) * row_bytes];
        for px in row.chunks_exact(4) {
            dib.push(px[2]); // B
            dib.push(px[1]); // G
            dib.push(px[0]); // R
            dib.push(px[3]); // A
        }
    }
    Some(dib)
}

/// 生成列表卡片的预览摘要。
///
/// 规则（按用户实测反馈定的，都是踩过的坑）：
///   1. **跳过开头的空行**。复制自网页/文档的文本常以空行开头，不跳预览就是一片空白。
///   2. **最多 2 行、累计不超过 140 字**。列表是「扫一眼找哪条」的视图，不是用来
///      读正文的 —— 想细看有右键「查看全文」。2 行让卡片高度更整齐、同屏条数更多。
///      ⚠️ 必须与 CSS 的 `-webkit-line-clamp` 保持一致（同样是 2），否则会出现
///      「后端给了 3 行、前端截成 2 行」的错位感。
///   3. **有内容被丢弃时补「…」**。改动前按 120 字硬截且没有省略号，视觉上像
///      「话说了半截断掉」，用户反馈「这两行为什么显示不全」就是这个。
///
/// **为什么不能只取首行**（更早的做法）：用户实测反馈「第一段只有两个字，
/// 条目里就只露出两个字」—— 而正文其实很长。只取首行等于把整条内容藏起来。
fn make_text_preview(text: &str) -> String {
    /// 与 CSS `.card-preview { -webkit-line-clamp }` 一致
    const MAX_LINES: usize = 2;
    /// 与 2 行大致匹配的字符预算（按中文字符保守估计）
    const MAX_CHARS: usize = 140;

    if text.trim().is_empty() {
        return "[空白文本]".into();
    }

    let mut out = String::new();
    let mut lines_taken = 0usize;
    let mut chars_taken = 0usize;
    let mut truncated = false;

    for line in text.lines() {
        let t = line.trim_end();
        // **空行一律跳过**（不只是开头）。
        // 2 行的预算很紧，而空行不提供任何识别信息 —— 占掉一行就等于白白少显示
        // 一行正文。段落结构在「查看全文」里可以完整看到，预览的首要任务是
        // 「让人一眼认出这是哪条」。
        if t.trim().is_empty() {
            continue;
        }
        if lines_taken >= MAX_LINES || chars_taken >= MAX_CHARS {
            truncated = true;
            break;
        }

        let remain = MAX_CHARS - chars_taken;
        let piece: String = t.chars().take(remain).collect();
        let used = piece.chars().count();

        if lines_taken > 0 {
            out.push('\n'); // 换行本身也占位，避免无限增长
            chars_taken += 1;
        }
        out.push_str(&piece);
        chars_taken += used;
        lines_taken += 1;

        // 这一行没取完 → 后面还有内容
        if used < t.chars().count() {
            truncated = true;
            break;
        }
    }

    if truncated {
        out.push('…');
    }
    out
}

/// 按当前规则重算文本条目的 preview，返回**真正发生变化**的条数。
///
/// 为什么要迁移：老条目是在「只取首行」的旧规则下写入的，规则改成「开头最多
/// 3 行」后存量不会自动更新 —— 用户看到的还是旧预览（首行短就只显示几个字），
/// 会以为修复没生效。
///
/// 幂等：只有值确实变了才计数，所以跑过一次之后就不再写盘。
/// 返回值给调用方决定是否 `save_index`。
fn refresh_text_previews(dir: &std::path::Path, hist: &mut [HistoryItem]) -> usize {
    let mut changed = 0usize;
    for item in hist.iter_mut() {
        // 只处理文本条目 —— 文件条目的 preview 是「名字 等 N 个文件」，另一套规则
        if item.kind != "text" {
            continue;
        }
        let Some(rel) = item.text_path.as_ref() else {
            continue;
        };
        // 不再「只读文件头 64KB」：内容已加密（见 ENC_MAGIC），DPAPI 必须先拿到
        // **完整密文**才能解出正文，读到一半的头没有任何意义。文本条目本身都不大，
        // 整份读+解的代价可接受；解不开（换了账户/机器）就跳过，不让它拖垮启动。
        let dir = dir.to_path_buf();
        let Ok(text) = read_text_entity(&dir, rel) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let new_preview = make_text_preview(&text);
        if new_preview != item.preview {
            item.preview = new_preview;
            changed += 1;
        }
    }
    changed
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
            item.preview = make_text_preview(&text);
            // 无论长短都落盘全文：内存只保留截断后的 preview 摘要，
            // 这样既遵守「内容不走内存」红线，复制回去时也不会被截断。
            let rel = format!("data/{}.txt", id);
            write_text_entity(&dir, &rel, &text);
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
            write_text_entity(&dir, &rel, &body);
            item.files_path = Some(rel);
            item.files = Some(files);
        }
        RawClip::ImageOriginal { bytes, ext, .. } => {
            // 直接把**原始文件字节**落盘（微信给的是 JPEG，几百 KB~几 MB）。
            //
            // 不做解码、不降采样、不转 BMP。落盘的是原图，好处有三：
            //   1. 捕获瞬间完成（写文件 ~几 ms，对比原来 495ms 的解码）
            //   2. 省磁盘：JPEG 4.7MB vs 原方案 BMP 12MB
            //   3. 画质更好：缩略图从原图生成，回贴也是原图
            // 需要解码的地方（缩略图 / 回贴）各自按需解码，见 ensure_thumb、
            // copy_back —— 那时的延迟用户感知不到（一个是懒加载，一个是点击后）。
            item.kind = "image".into();
            item.size = bytes.len() as u64;
            item.preview = "[图片]".into();
            let rel = format!("images/{}.{}", id, ext);
            match fs::write(dir.join(&rel), &bytes) {
                Ok(()) => item.img_path = Some(rel),
                Err(e) => crate::log_err!("clipboard", "图片原图落盘失败 {rel}：{e}"),
            }
        }
        RawClip::Image(bytes) => {
            item.kind = "image".into();
            item.size = bytes.len() as u64;
            item.preview = "[图片]".into();
            let rel = format!("images/{}.bmp", id);
            if let Ok(bmp) = dib_to_bmp(&bytes) {
                if let Err(e) = fs::write(dir.join(&rel), &bmp) {
                    crate::log_err!("clipboard", "位图落盘失败 {rel}：{e}");
                }
                item.img_path = Some(rel);
                // 顺手把缩略图一起生成了。
                //
                // 改动前缩略图由前端滚动时触发 `get_image_thumb` → `ensure_thumb`
                // **把刚写下的 12MB BMP 整个读回来、全量解码、再缩到 512px**。
                // 这一轮「读盘 12MB + 解码 1200 万像素」全花在已经拿到手的
                // 内存数据上，是大图要等 4~6 秒才显示缩略图的主因。
                //
                // 现在复用内存里的 DIB 直接生成：省掉回读与全量解码。
                // 落盘失败无妨 —— `ensure_thumb` 仍会在下次被调用时现场生成。
                match save_thumb_from_dib(&dir, &id, &bytes) {
                    Ok(()) => {}
                    Err(reason) =>                     crate::log_warn!(
                        "[clipboard] 内存缩略图生成失败 id={id} 原因={reason}，将由 ensure_thumb 兜底"
                    ),
                }
            } else {
                // 兜底：直接存 DIB，前端无法显示但至少保留
                let rel = format!("images/{}.dib", id);
                if let Err(e) = fs::write(dir.join(&rel), &bytes) {
                    crate::log_err!("clipboard", "DIB 兜底落盘失败 {rel}：{e}");
                }
                item.img_path = Some(rel);
            }
        }
        // ImageFile / ImageRaw 是「待处理」形态，经 resolve() 后会变成
        // Image / ImageOriginal / Files 才到这儿；留空分支兜底，不走 panic。
        RawClip::ImageFile(_) | RawClip::ImageRaw(_) => {}
    }

    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    hist.insert(0, item.clone());
    save_index(&dir, &hist);
    // ⚠️ 只记**元数据**，绝不记内容。
    //
    // 改动前这里打的是 `preview={:?}`，而 preview 就是剪贴板正文的开头几行 ——
    // 等于把用户复制的文本**明文**写进日志文件。日志的用途是排查问题，
    // 不该顺带成为一份内容副本；用户明确反馈过「不安全」。
    // 需要定位内容相关问题时，用这里的 id 去缓存目录里找对应条目即可。
    crate::log_print!(
        "[clipboard] 新增 {} 条目 kind={} size={}",
        id,
        item.kind,
        item.size
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
            let used = if used == 0 {
                1u32 << bi_bit_count
            } else {
                used
            };
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
/// 缩略图长边（像素）。
const THUMB_SIDE: u32 = 512;

/// 从内存中的 DIB 直接产出 512px 缩略图 PNG 并落盘。
///
/// **为什么不走 `ensure_thumb` 的路子**：`ensure_thub` 要先把 BMP 从磁盘读回来、
/// 交给 image 库全量解码、再 `thumbnail` 缩放 —— 一张 12MB 的图这是一整轮
/// 「读盘 + 解码 1200 万像素」。而调用点的 DIB 就在手里，位图数据是
/// **规整的 32bpp BGRA、自下而上、无压缩**，直接按比例跳采样即可，
/// 完全不必经过通用解码器。
///
/// 跳采样（nearest-neighbor）而非插值：缩略图只有 512px，肉眼几乎看不出差别，
/// 但跳采样是纯内存读取 + 拷贝，比解码快一个量级。
///
/// 任何一步失败都只返回 None、不影响落盘 —— 缩略图缺失时前端仍会回退到
/// `ensure_thumb` 现场生成。
fn save_thumb_from_dib(dir: &std::path::Path, id: &str, dib: &[u8]) -> Result<(), &'static str> {
    if dib.len() < 40 {
        return Err("DIB 长度不足 40 字节（信息头残缺）");
    }
    let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let bi_bit_count = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    // 只处理落盘 BMP 的标准形态：32bpp、无压缩。其它形态交给 ensure_thumb。
    let bi_compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    if bi_bit_count != 32 || bi_compression != 0 {
        return Err("非 32bpp 或未压缩（交由 ensure_thumb 现场生成）");
    }
    let w = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let h = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    if w <= 0 || h <= 0 {
        return Err("宽高无效（<=0）");
    }
    let (w, h) = (w as u32, h as u32);

    // 像素区起点（信息头 + 调色板；32bpp 无调色板）
    let off = (40 + bi_size.min(dib.len())).min(dib.len());
    let px = &dib[off..];
    let stride = w as usize * 4;
    if px.len() < stride * h as usize {
        return Err("像素区长度不足（数据被截断）");
    }

    // 等比缩到 THUMB_SIDE 以内
    let scale = w.max(h).max(1) as f32 / THUMB_SIDE as f32;
    let (tw, th) = if scale > 1.0 {
        (
            (w as f32 / scale).round().max(1.0) as u32,
            (h as f32 / scale).round().max(1.0) as u32,
        )
    } else {
        (w, h)
    };

    // 自下而上 → 输出给 image 库时按自上而下（PNG 约定）
    let mut rgba = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        let sy = h - 1 - (ty as u64 * h as u64 / th as u64) as u32; // 翻转
        let row = &px[sy as usize * stride..sy as usize * stride + stride];
        for tx in 0..tw {
            let sx = tx as u64 * w as u64 / tw as u64;
            let p = &row[sx as usize * 4..sx as usize * 4 + 4];
            rgba.extend_from_slice(&[p[2], p[1], p[0], 255]); // BGRA → RGBA，alpha 固定 255
        }
    }

    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(tw, th, rgba).expect("尺寸与数据长度一致"),
    )
    .write_to(&mut png, image::ImageFormat::Png)
    .ok()
    .ok_or("PNG 编码失败")?;

    let thumb_path = dir.join(format!("thumbs/{}.png", id));
    let _ = fs::create_dir_all(thumb_path.parent().ok_or("缩略图路径无父目录")?);
    fs::write(thumb_path, png.into_inner()).map_err(|_| "缩略图写盘失败")?;
    Ok(())
}

fn ensure_thumb(dir: &std::path::Path, item: &HistoryItem) -> Option<Vec<u8>> {
    let rel = item.img_path.as_ref()?;
    let src = dir.join(rel);
    // 1) 已有缩略图直接读
    let thumb_rel = format!("thumbs/{}.png", item.id);
    let thumb_path = dir.join(&thumb_rel);
    if let Ok(bytes) = fs::read(&thumb_path) {
        return Some(bytes);
    }
    // 2) 从原图现场生成
    let bytes = match fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            crate::log_err!("clipboard", "缩略图：读取原图失败 {}：{e}", src.display());
            return None;
        }
    };
    if bytes.len() > 256 * 1024 * 1024 {
        crate::log_err!(
            "clipboard",
            "图片 {} 达 {}MB，超出处理能力，缩略图跳过",
            src.display(),
            bytes.len() / 1048576
        );
        return None;
    }
    let img = match image::load_from_memory(&bytes) {
        Ok(i) => i,
        Err(e) => {
            // 这就是「图片条目一直显示占位图」的原因所在（SVG/PSD/缺解码器的格式）。
            // 记下具体格式错误，比前端只显示一个灰块有用得多。
            crate::log_warn!("[clipboard] 缩略图：图片无法解码 {}：{e}", src.display());
            return None;
        }
    };
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

// ---------------------------------------------------------------------------
// 内容加密（Windows DPAPI）
//
// 为什么必须加密：`data/<id>.txt` 里装的是剪贴板正文，`files/<id>.txt` 里装的是
// 完整路径 —— 这两样都是**用户数据**。落成明文意味着任何进程、任何在本机登录的
// 账户（只要有权进那个目录）都能直接读走用户复制过的密码、密钥、聊天记录。
// 缓存目录还能被改到 U 盘/网盘上，明文就等于随手带走一份。
//
// 为什么选 DPAPI 而不是自己管密钥：
//   - `CryptProtectData` 的主密钥由 Windows 按**当前用户**保管，不需要我们生成、
//     存储、传输密钥，也就没有「密钥跟着数据一起落盘」的自欺欺人；
//   - 无需用户设口令，符合本工具「安静常驻、零交互」的定位；
//   - 别的账户/别的机器拿到文件也解不开。
// 代价（必须让用户知道）：换 Windows 账户或换机器后，**旧数据读不出来**。
// 因此这是一次性的、不可逆的存储格式变更，且与「缓存目录可迁移」并存时
// 只保证同机同账户可读。
//
// 图片（`images/*`）暂不加密：它是二进制图源，且缩略图/回贴要频繁整段读写，
// 加密收益低、开销与外溢风险都更大。用户在意的「文本明文」问题本文件已解决。
// ---------------------------------------------------------------------------

/// 密文前缀（魔数）。
///
/// 开头放一个 `\0`：正常文本文件几乎不可能以 NUL 开头，所以这个标记既能
/// 100% 区分「已加密」与「历史明文」，又不会把恰好以 "QTE1" 开头的用户文本误判。
/// 第二个用途是排障：一眼能看出文件是加密态，而不是「损坏」。
const ENC_MAGIC: &[u8] = b"\x00QTE1\x01";

/// DPAPI 的附加熵（optional entropy）。
///
/// 熵不是密钥 —— 它只是让「同账户下的别的程序」即便调用 `CryptUnprotectData`
/// 也解不开我们的数据（不传熵就对不上）。真正的主密钥仍由 Windows 保管，
/// 所以这个常量直接写在代码里是安全的，也保证重装后仍能解开自己的数据。
const ENC_ENTROPY: &[u8] = b"qingtie.clipboard.v1";

/// Win32 `DATA_BLOB`
#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        p_data_in: *const DataBlob,
        sz_data_descr: *const u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        p_data_in: *const DataBlob,
        ppsz_data_descr: *mut *mut u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// 禁止 DPAPI 弹任何 UI。我们是常驻后台程序，弹窗会卡死在无人值守的场景。
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x01;

fn blob_of(bytes: &[u8]) -> DataBlob {
    DataBlob {
        cb_data: bytes.len() as u32,
        // DPAPI 只读入参，但 Win32 签名是非 const 指针
        pb_data: bytes.as_ptr() as *mut u8,
    }
}

/// 调 DPAPI 并把输出 blob 拷成 `Vec`（顺手 `LocalFree`，避免每次调用漏一块内存）。
fn dpapi(input: &[u8], protect: bool) -> Option<Vec<u8>> {
    unsafe {
        let in_blob = blob_of(input);
        let ent = blob_of(ENC_ENTROPY);
        let mut out = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = if protect {
            CryptProtectData(
                &in_blob,
                std::ptr::null(),
                &ent,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &in_blob,
                std::ptr::null_mut(),
                &ent,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 || out.pb_data.is_null() {
            crate::log_err!(
                "clipboard",
                "DPAPI {} 失败 last_err={}",
                if protect { "加密" } else { "解密" },
                GetLastError()
            );
            return None;
        }
        let v = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec();
        LocalFree(out.pb_data as *mut std::ffi::c_void);
        Some(v)
    }
}

/// 把一段文本加密成落盘字节（带魔数前缀）。
///
/// 加密失败时**退回明文**而不是报错：宁可少一层保护，也不能因为 DPAPI 异常
/// 把用户刚复制的内容弄丢 —— 剪贴板历史的首要职责是「记得住」。
fn encrypt_text(plain: &str) -> Vec<u8> {
    match dpapi(plain.as_bytes(), true) {
        Some(blob) => {
            let mut out = Vec::with_capacity(ENC_MAGIC.len() + blob.len());
            out.extend_from_slice(ENC_MAGIC);
            out.extend_from_slice(&blob);
            out
        }
        None => {
            crate::log_warn!(
                "[clipboard] DPAPI 加密失败，本条按明文落盘（内容不丢）；失败码见上方 DPAPI 加密错误"
            );
            plain.as_bytes().to_vec()
        }
    }
}

/// 把落盘字节还原成文本，**同时兼容历史明文文件**（没有魔数前缀的就是老数据）。
fn decode_text_bytes(raw: &[u8]) -> Option<String> {
    if raw.starts_with(ENC_MAGIC) {
        let plain = dpapi(&raw[ENC_MAGIC.len()..], false)?;
        // 密文解出来一定是当初写进去的 UTF-8；非法字节说明 blob 被破坏
        String::from_utf8(plain).ok()
    } else {
        Some(String::from_utf8_lossy(raw).to_string())
    }
}

/// 读一个「文本类」实体（`data/*.txt`、`files/*.txt`），自动解密。
fn read_text_entity(dir: &PathBuf, rel: &str) -> Result<String, String> {
    let bytes = fs::read(dir.join(rel)).map_err(|e| e.to_string())?;
    decode_text_bytes(&bytes).ok_or_else(|| "内容解密失败（可能换了 Windows 账户或机器）".to_string())
}

/// 写一个「文本类」实体，自动加密。
fn write_text_entity(dir: &PathBuf, rel: &str, text: &str) {
    if let Err(e) = fs::write(dir.join(rel), encrypt_text(text)) {
        crate::log_err!("clipboard", "文本实体落盘失败 {rel}：{e}");
    }
}

/// 把历史遗留的**明文**实体就地加密，返回处理条数。
///
/// 只在升级后的首次启动生效：加密过的文件带 `ENC_MAGIC` 前缀，直接跳过，
/// 所以这个函数是幂等的，之后每次启动都是「扫一遍、什么都不做」。
fn encrypt_legacy_entities(dir: &PathBuf) -> usize {
    let mut n = 0usize;
    for sub in ["data", "files"] {
        let Ok(rd) = fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            if bytes.is_empty() || bytes.starts_with(ENC_MAGIC) {
                continue;
            }
            let enc = encrypt_text(&String::from_utf8_lossy(&bytes));
            if enc.starts_with(ENC_MAGIC) && fs::write(&path, &enc).is_ok() {
                n += 1;
            }
        }
    }
    n
}

fn load_index(dir: &PathBuf) -> Vec<HistoryItem> {
    let path = dir.join("index.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let mut v: Vec<HistoryItem> = serde_json::from_str(&s).unwrap_or_else(|e| {
                crate::log_warn!("[clipboard] index.json 解析失败：{e}，返回空（历史清空）");
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
    match serde_json::to_string_pretty(hist) {
        Ok(s) => {
            // 写失败要记：index.json 是重启后恢复历史/置顶/收藏的唯一依据，
            // 静默失败会表现为「重启后记录全没了」却查无实据。
            if let Err(e) = fs::write(&path, s) {
                crate::log_err!("clipboard", "index.json 写入失败：{e}");
            }
        }
        Err(e) => crate::log_err!("clipboard", "index.json 序列化失败：{e}"),
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
        crate::log_print!("[clipboard] 更换缓存目录：用户取消");
        return Ok(None); // 用户取消
    };
    let new_dir: PathBuf = match picked {
        tauri_plugin_dialog::FilePath::Path(p) => p,
        tauri_plugin_dialog::FilePath::Url(u) => return Err(format!("不支持的路径：{u}")),
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
        crate::log_print!("[clipboard] 更换缓存目录：所选即当前目录，无需迁移");
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

/// LASTINPUTINFO：dwTime 是最后一次键鼠输入的 tick 计数。
///
/// 必须定义在 `extern` 块**外面** —— extern 块内只允许声明 foreign item
/// （函数/静态量），不能放 struct 定义。
#[repr(C)]
pub struct LastInputInfo {
    pub cb_size: u32,
    pub dw_time: u32,
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
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
    fn GetLastError() -> u32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn SetClipboardData(format: u32, handle: isize) -> isize;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn EnumClipboardFormats(format: u32) -> u32;
    fn GetClipboardFormatNameW(format: u32, lpszFormatName: *mut u16, cchMaxCount: i32) -> i32;
    fn GetClipboardSequenceNumber() -> u32;
    fn GetClipboardOwner() -> isize;
    fn GetWindowThreadProcessId(hwnd: isize, lpdwProcessId: *mut u32) -> u32;
    fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
    fn QueryFullProcessImageNameW(
        hProcess: isize,
        dwFlags: u32,
        lpExeName: *mut u16,
        lpdwSize: *mut u32,
    ) -> i32;
    fn CloseHandle(hObject: isize) -> i32;
    fn GetLastInputInfo(pli: *mut LastInputInfo) -> i32;
    fn GetTickCount() -> u32;

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

    /// 加密往返：同一账户下解回来必须与原文逐字一致。
    /// 同时断言密文里**找不到明文** —— 这才是「不落明文」这条要求的真正验收点，
    /// 只验证能解密的话，一个「加密函数直接返回原文」的实现也能通过。
    #[test]
    fn encrypt_roundtrip_hides_plaintext() {
        let plain = "密码 hunter2 —— 这串不能出现在落盘字节里";
        let enc = encrypt_text(plain);

        assert!(enc.starts_with(ENC_MAGIC), "密文必须带魔数前缀");
        assert_ne!(enc, plain.as_bytes(), "落盘字节不能等于明文");

        let needle = plain.as_bytes();
        let leaked = enc.windows(needle.len()).any(|w| w == needle);
        assert!(!leaked, "密文里不应出现明文原文");

        assert_eq!(decode_text_bytes(&enc).as_deref(), Some(plain));
    }

    /// 兼容性：加密功能上线**之前**写入的明文文件没有魔数，读取时必须原样返回，
    /// 否则老用户升级后所有历史文本条目会集体读不出来。
    #[test]
    fn decode_accepts_legacy_plaintext() {
        let legacy = "旧版本留下的明文内容".as_bytes();
        assert_eq!(
            decode_text_bytes(legacy).as_deref(),
            Some("旧版本留下的明文内容")
        );
    }

    /// 带魔数但内容被破坏 → 返回 None（而不是把垃圾当文本显示出来）。
    /// 典型触发场景：换了 Windows 账户或换了机器，DPAPI 解不开。
    #[test]
    fn decode_rejects_broken_ciphertext() {
        let mut bad = ENC_MAGIC.to_vec();
        bad.extend_from_slice(b"not a real dpapi blob");
        assert!(decode_text_bytes(&bad).is_none());
    }

    /// 空文本也能正常往返（用户复制了空格/空行的情况）。
    #[test]
    fn encrypt_roundtrip_empty() {
        let enc = encrypt_text("");
        assert!(enc.starts_with(ENC_MAGIC));
        assert_eq!(decode_text_bytes(&enc).as_deref(), Some(""));
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

    /// 复现 2.1.11 的真实故障：微信把同一张图在八十多秒内反复写进剪贴板，
    /// 间隔远大于原先 800ms 的窗口。队列判重必须全部合并，只留一条。
    ///
    /// 间隔取自日志实测（06:30:19 / :23 / :31 / :35 / :46 / :50 / 06:31:36 / :40）。
    /// 复现 2.1.11 的真实故障：微信把同一张图反复写进剪贴板。
    ///
    /// 间隔取自日志实测（06:30:19 → :23 → :31 → :35 → :46 → :50 →
    /// 06:31:36 → :40），相邻间隔为 4/8/4/11/4/46/4 秒。
    /// **直接调用生产代码 `dedup_merge`**，而不是在测试里重写一遍判重逻辑 ——
    /// 后者只能验证「我理解对了」，前者才能验证「代码写对了」。
    #[test]
    fn repeated_writes_collapse_to_one_entry() {
        let mut g: Vec<(u64, u64)> = Vec::new();
        let fp = image_fingerprint(&solid_dib(400, 300, [10, 20, 30, 255]));
        let gaps = [0u64, 4_000, 8_000, 4_000, 11_000, 4_000, 46_000, 4_000];

        let mut merged_count = 0;
        let mut t = 1_000_000u64;
        for gap in gaps {
            t += gap;
            if dedup_merge(&mut g, fp, t) {
                merged_count += 1;
            }
        }
        assert_eq!(merged_count, gaps.len() - 1, "8 次写入应合并 7 次");
        assert_eq!(g.len(), 1, "最终队列里只该留下 1 条同图记录");
    }

    /// 「连续不间断复制」必须始终合并 —— 这是用户报的「连续复制出现两张」。
    ///
    /// 旧实现在命中时不刷新时间戳，基准永远停在第一次，gap 累加超过阈值就
    /// 又记一条。这条测试锁住「滑动续期」这一行为：即便总时长远超窗口，
    /// 只要相邻间隔在窗口内，就始终是一条。
    #[test]
    fn continuous_repeat_never_creates_second_entry() {
        let mut g: Vec<(u64, u64)> = Vec::new();
        let fp = image_fingerprint(&solid_dib(400, 300, [9, 9, 9, 255]));
        let mut t = 1_000_000u64;
        // 每 20s 来一次，共 30 次 → 总跨度 600s，远超窗口 60s。
        // 首次应记录（merged=false），其后的每一次都必须被合并 ——
        // 「连续复制」场景下用户预期始终只有一条。
        for i in 0..30 {
            t += 20_000;
            let merged = dedup_merge(&mut g, fp, t);
            if i == 0 {
                assert!(!merged, "首次应记录");
            } else {
                assert!(merged, "第 {i} 次连续复制（总跨度已超窗口）必须被合并");
            }
        }
        assert_eq!(g.len(), 1, "全程只该有 1 条");
    }

    /// 窗口必须大于实测的最大**相邻**间隔，否则上面第一条测试是假通过。
    #[test]
    fn dup_window_covers_observed_adjacent_gap() {
        assert!(
            DUP_WINDOW_MS > OBSERVED_MAX_ADJACENT_GAP_MS,
            "窗口 {DUP_WINDOW_MS}ms 必须覆盖实测相邻间隔 {OBSERVED_MAX_ADJACENT_GAP_MS}ms"
        );
    }

    /// A → B → A：用户确实复制了两次 A（中间夹了别的图），必须**放行**。
    /// 这正是「只与最后一条比对」的设计意图 —— 与整个队列比对会误合并第二个 A。
    #[test]
    fn interleaved_distinct_image_resets_cluster() {
        let mut g: Vec<(u64, u64)> = Vec::new();
        // ⚠️ 颜色要选**量化后仍不同**的值：指纹采样前会 >>3，
        // 所以 [1,1,1] 与 [2,2,2] 都会变成 0、判为同一张 —— 这不是 bug，
        // 而是量化在容忍「两条写入路径的重采样差异」（见 image_fingerprint）。
        // 测试数据须避开这个区间，否则测的是量化行为而非判重逻辑。
        let a = image_fingerprint(&solid_dib(400, 300, [200, 40, 40, 255]));
        let b = image_fingerprint(&solid_dib(400, 300, [40, 40, 200, 255]));
        assert_ne!(a, b, "两图指纹必须不同，否则本测试无意义");
        let mut t = 1_000_000u64;

        assert!(!dedup_merge(&mut g, a, t), "首次 A 应记录");
        t += 1_000;
        assert!(!dedup_merge(&mut g, b, t), "B 与 A 不同，应记录");
        t += 1_000;
        assert!(
            !dedup_merge(&mut g, a, t),
            "中间的 B 打断了簇，第二个 A 应作为新条目记录"
        );
        assert_eq!(g.len(), 3);
    }

    /// 队列不能无限增长。
    #[test]
    fn queue_is_bounded() {
        let mut g: Vec<(u64, u64)> = Vec::new();
        let mut t = 1_000_000u64;
        for i in 0..40u64 {
            t += 100;
            let fp = image_fingerprint(&solid_dib(400, 300, [i as u8, 1, 2, 255]));
            dedup_merge(&mut g, fp, t);
        }
        assert!(g.len() <= LAST_IMG_KEEP, "队列长度必须受限");
    }

    /// 缩略图跳采样的方向必须正确。
    ///
    /// 这是最容易出错的一处：DIB 是**自下而上**（最后一行在数据最前），
    /// 而 PNG/输出缓冲是**自上而下**。方向搞反 = 缩略图上下颠倒。
    /// 构造一张「上红下蓝」的 2×2 图，跑一遍后首行应为红。
    #[test]
    fn thumb_sampling_keeps_top_row_on_top() {
        let (w, h) = (2i32, 2i32);
        // BGRA，自下而上 → 数据顺序是 [下蓝, 下蓝, 上红, 上红]
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&w.to_le_bytes());
        dib.extend_from_slice(&h.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&16u32.to_le_bytes());
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        let (blue_bgra, red_bgra) = ([255u8, 0, 0, 255], [0u8, 0, 255, 255]);
        for px in [blue_bgra, blue_bgra, red_bgra, red_bgra] {
            dib.extend_from_slice(&px);
        }

        // 复刻 save_thumb_from_dib 里的采样与翻转，验证方向
        let stride = w as usize * 4;
        let px = &dib[40..];
        let mut out = Vec::new();
        for ty in 0..h as u32 {
            let sy = h as u32 - 1 - ty; // 翻转
            let row = &px[sy as usize * stride..sy as usize * stride + stride];
            for tx in 0..w as u32 {
                let sx = tx as usize;
                let p = &row[sx * 4..sx * 4 + 4];
                out.push([p[2], p[1], p[0], 255]);
            }
        }
        // 输出首行应是「顶行（红）」，末行应是「底行（蓝）」。
        // 推导：数据顺序 [下蓝, 下蓝, 上红, 上红] 里 index 2/3 才是顶行；
        // 翻转取 sy = h-1-ty，ty=0 → sy=1 → 落在 index 2/3 → 红。✔
        assert_eq!(out[0], [255, 0, 0, 255], "首行必须是顶行（红）");
        assert_eq!(out[2], [0, 0, 255, 255], "末行必须是底行（蓝）");
    }

    /// LRU 解码缓存：容量受限，且同路径内容变化时旧条目要被顶掉。
    #[test]
    fn decode_cache_lru_capacity_and_invalidation() {
        let mut g: Vec<(PathBuf, (u64, u64), Vec<u8>)> = Vec::new();
        let p_of = |i: u64| PathBuf::from(format!("C:/tmp/wx_{i}.jpg"));
        for i in 0..DECODE_CACHE_CAP as u64 + 2 {
            g.push((p_of(i), (i, 100 + i), vec![i as u8]));
            let len = g.len();
            if len > DECODE_CACHE_CAP {
                g.drain(0..len - DECODE_CACHE_CAP);
            }
        }
        assert_eq!(g.len(), DECODE_CACHE_CAP, "缓存不得超容量");
        // 最早的 2 条应被淘汰
        assert_eq!(g[0].0, p_of(2), "LRU 应淘汰最早的条目");

        // 同路径 + 不同 stamp → 旧条目必须被剔除，不能两条并存
        let mut g2: Vec<(PathBuf, (u64, u64), Vec<u8>)> = Vec::new();
        g2.push((p_of(9), (1, 100), vec![1]));
        g2.push((p_of(9), (2, 200), vec![2]));
        // 真实代码用的是 `p == path`（path 是 &Path）；这里等价模拟一次剔除
        let keep: Vec<_> = g2
            .iter()
            .filter(|(p, s, _)| *s != (1, 100))
            .cloned()
            .collect();
        assert_eq!(keep.len(), 1, "旧 stamp 的条目应被剔除");
        assert_eq!(keep[0].1, (2, 200));
    }

    /// 超过窗口的间隔必须**放行**（判为新条目），否则用户的正常重复操作会被吞。
    ///
    /// 与上一条测试互补：上一条验证窗口内合并，这条验证窗口外放行。
    /// 两条一起才说明窗口阈值真正起到了分界作用，而不是「一律合并」或「一律放行」。
    #[test]
    fn gap_beyond_window_creates_new_entry() {
        let mut g: Vec<(u64, u64)> = Vec::new();
        let fp = image_fingerprint(&solid_dib(400, 300, [7, 7, 7, 255]));
        let t0 = 1_000_000u64;

        assert!(!dedup_merge(&mut g, fp, t0), "首次应记录");
        // 正好在窗口边缘之内 → 合并
        assert!(
            dedup_merge(&mut g, fp, t0 + DUP_WINDOW_MS - 1),
            "窗口内应合并"
        );
        // 超过窗口 → 放行（此时上一条已被边缘那次刷新到 t0+WINDOW-1）
        assert!(
            !dedup_merge(&mut g, fp, t0 + DUP_WINDOW_MS - 1 + DUP_WINDOW_MS + 1),
            "超出窗口的间隔应放行"
        );
    }

    /// **格式不设限**：各类图片都要能被识别（这是 2.1.11 之后用户明确要求的
    /// 「对图片格式不要做限制」）。逐格式喂真实文件头，断言都能识别出来。
    #[test]
    fn detects_many_image_formats_by_magic() {
        let p = std::path::Path::new("no_ext"); // 无扩展名 → 只能靠魔数
        let cases: [(&str, &[u8]); 9] = [
            (
                "png",
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0],
            ),
            ("jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0]),
            ("gif", b"GIF89a____________"),
            ("bmp", b"BM____________"),
            ("webp", b"RIFF____WEBPVP8 "),
            // TIFF 两种字节序都要认
            ("tiff", b"II\x2A\x00____________"),
            ("tiff", b"MM\x00\x2A____________"),
            ("ico", &[0x00, 0x00, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0]),
            ("psd", b"8BPS____________"),
        ];
        for (want, bytes) in cases {
            assert_eq!(
                detect_image_ext(bytes, p).as_deref(),
                Some(want),
                "{want} 的魔数应被识别"
            );
        }
    }

    /// 库认不出、但确实该算图片的格式（AVIF/HEIC 属于 ISO BMFF 家族）。
    #[test]
    fn detects_iso_bmff_family() {
        let p = std::path::Path::new("no_ext");
        // ........ftyp avif
        let mut avif = b"\x00\x00\x00\x20ftypavif".to_vec();
        avif.extend_from_slice(&[0u8; 8]);
        assert_eq!(detect_image_ext(&avif, p).as_deref(), Some("avif"));

        let mut heic = b"\x00\x00\x00\x20ftypheic".to_vec();
        heic.extend_from_slice(&[0u8; 8]);
        assert_eq!(detect_image_ext(&heic, p).as_deref(), Some("heic"));
    }

    /// **文本类图片**（SVG）没有二进制魔数，只能靠扩展名 —— 也必须认出来，
    /// 否则「格式不设限」就是空话。
    #[test]
    fn detects_text_based_image_by_extension() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>".to_vec();
        assert_eq!(
            detect_image_ext(&svg, std::path::Path::new("a.svg")).as_deref(),
            Some("svg")
        );
        // 同样的内容但扩展名不是图片 → 不认（避免把普通 XML 当图片）
        assert_eq!(detect_image_ext(&svg, std::path::Path::new("a.xml")), None);
    }

    /// 反例：真正的非图片必须被排除，否则会把文档/压缩包也记成图片。
    #[test]
    fn rejects_non_images() {
        let p = std::path::Path::new("no_ext");
        assert_eq!(detect_image_ext(b"hello, world", p), None);
        assert_eq!(detect_image_ext(b"PK\x03\x04____________", p), None); // zip/docx
        assert_eq!(detect_image_ext(b"%PDF-1.7____________", p), None); // pdf
                                                                        // 空文件
        assert_eq!(detect_image_ext(b"", p), None);
        // 无意义的短数据
        assert_eq!(detect_image_ext(b"\x00\x01\x02", p), None);
        // ⚠️ 注意：`FF D8 FF` 只有 3 字节，但它**确实是** JPEG 的完整 magic
        // （image 库判定 JPEG 只需这 3 字节），所以这里应当认出 jpg，
        // 而不是因为「太短」被拒 —— 截断的 JPEG 仍会被识别，缩略图阶段失败后
        // 走占位/按文件粘贴兜底，不影响条目存在。
        assert_eq!(detect_image_ext(b"\xFF\xD8\xFF", p).as_deref(), Some("jpg"));
    }

    /// 详情弹窗：短文本原样返回，统计正确。
    #[test]
    fn summarize_content_short_text() {
        let c = summarize_content("第一行\n第二行\n第三行".to_string());
        assert!(!c.truncated);
        assert_eq!(c.text, "第一行\n第二行\n第三行");
        assert_eq!(c.total_lines, 3);
        assert_eq!(c.total_chars, "第一行\n第二行\n第三行".chars().count());
    }

    /// 详情弹窗：超长文本的显示被截断，但**统计仍是完整内容的规模** ——
    /// 否则用户看到「共 200000 字」会误以为内容只有这么长。
    #[test]
    fn summarize_content_truncates_display_but_keeps_true_size() {
        let raw = "x".repeat(DETAIL_DISPLAY_CAP + 5_000);
        let c = summarize_content(raw);
        assert!(c.truncated, "超上限必须标记截断");
        assert_eq!(
            c.text.chars().count(),
            DETAIL_DISPLAY_CAP,
            "显示部分应恰好截到上限"
        );
        assert_eq!(
            c.total_chars,
            DETAIL_DISPLAY_CAP + 5_000,
            "统计必须是完整内容的长度，不是截断后的"
        );
    }

    /// 边界：正好等于上限时**不应**标记截断（否则会多一条无意义的提示）。
    #[test]
    fn summarize_content_at_cap_is_not_truncated() {
        let raw = "y".repeat(DETAIL_DISPLAY_CAP);
        let c = summarize_content(raw);
        assert!(!c.truncated, "恰好等于上限不算超限");
        assert_eq!(c.text.chars().count(), DETAIL_DISPLAY_CAP);
    }

    /// 截断要按**字符**而不是字节 —— 否则会在多字节字符（中文/emoji）中间切开，
    /// 产出乱码或 panic。
    #[test]
    fn summarize_content_truncates_on_char_boundary() {
        let raw = "中".repeat(DETAIL_DISPLAY_CAP + 10);
        let c = summarize_content(raw);
        assert!(c.truncated);
        assert_eq!(c.text.chars().count(), DETAIL_DISPLAY_CAP);
        // 全文都是「中」，能正常取到就说明没有在字节中间切断
        assert!(c.text.chars().all(|ch| ch == '中'));
    }

    /// 预览：**首行很短时必须带上后续行**。
    ///
    /// 这是用户实测反馈的原话场景：「第一段只有两个字，条目里就只露出两个字」，
    /// 而正文其实很长。更早的实现只取首行（`lines().next()`），等于把内容藏起来。
    #[test]
    fn preview_includes_lines_after_a_short_first_line() {
        let p = make_text_preview("采集\n这是第二段的内容\n第三段");
        assert_eq!(p, "采集\n这是第二段的内容…", "2 行取满，多的补省略号");
    }

    /// 预览：空行要跳过（**不只是开头**）。
    /// 复制自网页/文档的文本常以空行开头，不跳预览就是一片空白；
    /// 而 2 行的预算很紧，中间的空行同样是在浪费额度。
    #[test]
    fn preview_skips_blank_lines() {
        assert_eq!(
            make_text_preview("\n\n  \n正文第一行\n正文第二行"),
            "正文第一行\n正文第二行"
        );
        // 中间的空行也跳 —— 段落结构在「查看全文」里完整可见
        assert_eq!(make_text_preview("第一段\n\n第二段"), "第一段\n第二段");
    }

    /// 预览：最多 2 行，且**被丢弃内容时必须补省略号**。
    /// 改动前按 120 字硬截且没有省略号，视觉上像「话说半截断掉」——
    /// 用户反馈「这两行为什么显示不全」就是这个。
    #[test]
    fn preview_limits_lines_and_marks_truncation() {
        let p = make_text_preview("一\n二\n三\n四\n五");
        assert_eq!(p, "一\n二…", "超过 2 行要截断并补省略号");
    }

    /// 预览：单行超长时截到字符预算并补省略号。
    #[test]
    fn preview_truncates_a_very_long_single_line() {
        let p = make_text_preview(&"x".repeat(500));
        assert_eq!(p.chars().count(), 141, "140 字 + 省略号");
        assert!(p.ends_with('…'));
    }

    /// 预览：空白文本给占位，不能返回空串（否则卡片一片空白，看着像坏了）。
    #[test]
    fn preview_blank_text_placeholder() {
        assert_eq!(make_text_preview("   \n\t\n"), "[空白文本]");
    }

    /// 预览：正常短文本**不应**多出省略号。
    #[test]
    fn preview_short_text_has_no_ellipsis() {
        assert_eq!(make_text_preview("就两个字"), "就两个字");
    }
}
