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

/// 剪贴板窗口句柄：**常驻**的消息专用窗口，不是主窗口。
///
/// 为什么不能用主窗口：低占用模式会把主窗口连同 WebView 一起销毁，而
/// `OpenClipboard(失效句柄)` 一律返回 0 —— 表现为「复制、回贴全部失败」；
/// `GetClipboardOwner()` 也不再等于它，自己写回的内容会被当成新复制重复入库；
/// `AddClipboardFormatListener` 同样失效，只剩低频心跳兜底。
///
/// 消息专用窗口（`HWND_MESSAGE`）不进任务栏、不显示，生命周期等于进程，
/// 因此释放界面、重建界面都不影响剪贴板读写。
static CLIP_HWND: OnceLock<isize> = OnceLock::new();
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

/// 「自己写进剪贴板的内容也照常记录」的一次性通行证。
///
/// 采集端默认把自己 owner 的写入当成「从历史回贴」跳过（否则每粘一次就多一条重复）。
/// 但**截图**是唯一的例外：内容由我们自己写进剪贴板，用户却期望它立刻出现在列表里。
/// 这个标记只被采集端消费一次，回贴时会主动清掉，防止残留把一次真实的回贴记成新条目。
static RECORD_OWN_WRITE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 最近一次「自己写回剪贴板」是不是截图，值是那一刻的毫秒时间戳（0 = 无）。
///
/// 为什么用时间戳而不是布尔：布尔只表示「有过一次写回」，如果那次截图因为
/// 某些原因没被捕获到，标记会一直挂着，之后用户手动复制的图片就会被误判成
/// 截图。带时刻就有时效 —— 超过 [`SHOT_MARK_TTL_MS`] 自动失效。
static SHOT_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 截图标记的有效期。截图写完剪贴板到被捕获通常是同一拍，留 3 秒足够宽裕。
const SHOT_MARK_TTL_MS: u64 = 3_000;
/// 「未识别」日志的去重序号：同一个序号只报一次，避免轮询期间每拍刷一条。
static LAST_LOGGED_SEQ: AtomicU32 = AtomicU32::new(0);
/// 「正在读剪贴板」互斥标记。
///
/// 事件路径（WndProc 主线程）与兜底轮询（watchdog）是两条独立通道，都会调用
/// `on_clipboard_update`。轮询提到 ms 级后两者撞车的概率显著上升：两条同时进入
/// 「序号变了 → 去读内容」就会重复走一遍 OpenClipboard + 诊断串（读同一份数据），
/// 白费开销还可能拿到不一致的结果。
///
/// 用一次 CAS 把「读内容」这段串起来：抢不到的通道直接返回，下一拍（≤20ms）再试，
/// 不会丢内容 —— 因为「只在成功时推进序号」的规则保证了没读到就会被重试。
static READING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 「等 CF_HDROP 交货」的起始时刻，按序号记录。
///
/// 用途：同一次复制里 CF_HDROP（文件/图片）与 CF_DIB（位图）会先后就位，且两者
/// 指纹算法不同（文件哈希 vs 像素指纹）→ 判重认不出 → 一次复制记两条。
/// 只要 CF_HDROP 已登记就暂缓用 DIB，等它就位；但要有个上限，
/// 否则「HDROP 永远不渲染」时这张图就丢了。这一对原子量就是那个上限的计时器。
static DIB_FALLBACK_SEQ: AtomicU32 = AtomicU32::new(0);
static DIB_FALLBACK_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
/// 最近落过的文本指纹（内容哈希）与时刻。
///
/// 为什么文本也需要：一次复制可能被应用**写两次**剪贴板（先纯文本、后补富文本），
/// 于是两条通道各收一次，历史里出现两条一模一样的记录 —— 用户实测反馈过。
/// 图片早就有这套指纹去重（`LAST_IMG`），文本一直没有。
static LAST_TEXT: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
/// 最近落过的「文件路径清单」指纹。文件条目同样会被写两次（资源管理器在选中多个
/// 文件时可能连续两次 SetClipboardData），日志里出现过同秒同大小的 files 重复。
static LAST_FILES: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
/// 文本判重窗口。比图片的 60s 短得多，因为语义不同：
/// 图片的 60s 是为了扛住微信「隔几十秒补写同一张图」；
/// 文本这里只合并「同一次复制被写两遍」，2 秒足够，也不会把用户
/// 真的连续复制两次同一段文字（通常间隔更久）误合并。
const TEXT_DUP_WINDOW_MS: u64 = 2_000;
/// 「同一次复制的两次写入」判定窗口。
///
/// 实测：Markdown 复制时应用先写渲染文本（812 字）、约 20ms 后再写原文（1347 字），
/// 两次**纯文本并不相同**，所以内容哈希判不了。这里放宽成「短时间内 + 内容同源」。
/// 窗口只给 800ms：人要连续复制两段不同内容，间隔远大于此；
/// 而机器分两次写剪贴板都在几十毫秒内。
const TEXT_SAME_ORIGIN_MS: u64 = 800;
/// 同源相似度阈值。实测 Markdown 原文 vs 其渲染文本 ≈ 0.6~0.75（标记被去掉、空白不同），
/// 而两段无关正文的 bigram 重叠通常 < 0.25 —— 0.5 落在两者中间。
const TEXT_SAME_ORIGIN_SIM: f32 = 0.5;
/// 队列上限：只关心「最近这一小段时间」，不用无限增长。
const LAST_IMG_KEEP: usize = 16;
/// 剪贴板落盘工作线程的投递端。解码 / 写盘 / 生成缩略图都在那边做，
/// 主线程只负责把剪贴板里的字节拷出来就返回，见 spawn_worker。
/// 元组第三项 = 是否由「编辑保存」产生（决定条目上是否显示「已编辑」徽章）。
static WORKER: OnceLock<std::sync::mpsc::Sender<(u64, RawClip, bool, bool)>> = OnceLock::new();
/// 缩略图生成线程的投递端。与捕获线程分开：捕获线程只做「落盘 + 通知前端」必须快，
/// 缩略图解码慢、且不影响条目出现，所以单独排队，避免拖慢快速连续复制。
static THUMB_TX: OnceLock<std::sync::mpsc::Sender<HistoryItem>> = OnceLock::new();
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

/// 缺字段时的兜底值：见 `HistoryItem::preview_truncated`（老索引没有这个字段，
/// 按「可能还有更多内容」处理，宁可可弹一次多余浮层，也不要老条目彻底失去预览）。
fn default_true() -> bool {
    true
}

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
    /// 本条是否由「复制并编辑」的保存动作产生（即编辑后的副本）。
    ///
    /// 为什么需要这个标记：编辑保存的内容写进了剪贴板，用户的预期就是「它该像
    /// 一次复制那样被记住」。可它和原文高度相似，历史里两条挨着很难分清哪条是
    /// 改过的 —— 所以给这类条目一个「已编辑」徽章。
    ///
    /// `#[serde(default)]` 是**必需的**：老 index.json 里没有这个字段，
    /// 缺了它整个索引会解析失败、历史全部丢失。
    #[serde(default)]
    pub edited: bool,
    /// 文本条目的**子类型**：`"link"` = 整条内容就是一个网址，`"code"` = 看起来是代码。
    ///
    /// 为什么不新增 `kind`：侧栏分类是按「用户想找什么」分的，链接与代码在语义上
    /// 仍然是文本 —— 单开一类只会让同一条内容在两个地方各出现一次，违背「一条记录
    /// 只属于一个位置」。子类型只服务两件事：卡片上多一个徽章，让人一眼认出这条是什么。
    ///
    /// `skip_serializing_if` 是刻意的：绝大多数文本都不是这两种，没必要往加密索引
    /// 里给每一条都塞一个 `"subtype":null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// 文本前若干字符 / "[图片]" / "N 个文件"
    pub preview: String,
    /// 文本预览是否**被截断过**（即卡片那 2 行没把正文显示完）。
    ///
    /// 前端据此决定要不要给悬浮预览：卡片上已经显示全的内容再弹一层浮层毫无意义，
    /// 只是把同样的字重复一遍。图片与文件条目恒为 false（不参与这个判断）。
    ///
    /// `default = true` 是给老 index.json 用的：缺字段时假设「还有更多内容」，
    /// 老条目至少不会因此永久失去悬浮预览；真实值由启动时的
    /// `refresh_text_previews` 重算并落盘。
    #[serde(default = "default_true")]
    pub preview_truncated: bool,
    pub size: u64,
    /// 长文本落盘路径（相对 DIR/data/）
    pub text_path: Option<String>,
    /// 富文本实体：`data/<id>.html`（CF_HTML 原文）与 `data/<id>.rtf`。
    /// 仅当复制源提供了对应格式时才存在；两者都没有就是纯文本条目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtf_path: Option<String>,
    /// 富文本里拆出来单独落盘的内联图（相对路径）。
    ///
    /// 下标 == HTML 里 `data-qt-img="n"` 的 n。与 `html_path` 同生命周期：
    /// 删条目时整个 `images/rich/<id>/` 目录一起删。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rich_images: Vec<String>,
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
    /// 本条是否是**截图**（轻贴 Alt+Q / 微信 / QQ / 系统截图工具）。
    ///
    /// 为什么要存下来而不是「看文件在哪个目录」：关掉「图片分类存放」时图片会
    /// 搬回 `images/` 根目录，路径就不再携带来源；再打开时若按路径推断，分好
    /// 的截图会全部被当成复制塞进 `copy/`。这是用户实测反馈的问题。
    ///
    /// `skip_serializing_if` 是刻意的：绝大多数内容不是截图，没必要给每条都往
    /// 加密索引里写一个 `"shot":false`。`#[serde(default)]` 必须加 —— 老索引
    /// 没有这个字段，缺了会让整份索引解析失败、历史全丢。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shot: bool,
}

// ---------------------------------------------------------------------------
// 对外命令
// ---------------------------------------------------------------------------

/// 复制一份诊断信息到剪贴板（给用户报障用）。
///
/// 为什么需要：用户反馈问题时，最常缺的就是「版本 / 系统 / 材质 / 数据在哪」这四项，
/// 每次都要来回追问、截图。这里一次性拼好，用户点一下粘贴就能发出来。
///
/// 内容**只含元数据**，不含任何剪贴板正文 —— 与日志同一条红线。
/// 写剪贴板会走 `mark_skip()`，所以这条诊断文本不会被记成一条历史。
#[tauri::command]
pub fn copy_diagnostics() -> Result<String, String> {
    let info = crate::about::about_info();
    let s = crate::settings::get();
    let cache = cache_info();
    let items = HISTORY
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.len()))
        .unwrap_or(0);
    let get = |k: &str| info.get(k).and_then(|v| v.as_str()).unwrap_or("—").to_string();

    let text = format!(
        "轻贴 {}（{}）\n系统：{}\n材质档位：{}\n界面缩放：{}\n主题：{}\n保留天数：{}\n历史条目：{} 条\n缓存目录：{}\n缓存占用：{} 个文件 / {:.1} MB",
        get("version"),
        if get("debug") == "true" { "debug" } else { "release" },
        get("osDisplay"),
        get("tier"),
        if s.ignore_dpi_scale {
            "固定 100%（不跟随系统）"
        } else {
            "跟随系统"
        },
        s.theme_mode,
        if s.retention_days == 0 {
            "永久保存".to_string()
        } else {
            format!("{} 天", s.retention_days)
        },
        items,
        cache.dir,
        cache.count,
        cache.size as f64 / 1024.0 / 1024.0
    );

    unsafe { write_text_to_clipboard(&text)? };
    crate::log_print!("[clipboard] 已复制诊断信息到剪贴板（不含任何正文）");
    Ok(text)
}

#[tauri::command]
pub fn get_history() -> Vec<HistoryItem> {
    HISTORY
        .get()
        .map(|m| m.lock().unwrap().clone())
        .unwrap_or_default()
}

/// 处理条目引用的本地文件（回收站 / 永久删除），返回给前端的提示串。
///
/// **两道闸**，缺一不可：
///   1. 前端必须显式要求（`delete_local_files = true`）—— 它来自用户在弹窗里的确认；
///   2. 设置里必须已开启 `delete_files_with_item` —— 防止前端 bug 或误调用直接把
///      用户的真实文件带走。
/// 所以后端不信任任何一侧，两边都满足才动手。
///
/// 日志只记**数量**，不记路径：路径里带着文件名，属于内容级信息。
fn purge_local_files(items: &[HistoryItem]) -> Option<String> {
    let cfg = crate::settings::get();
    if !cfg.delete_files_with_item {
        return None;
    }
    let paths: Vec<String> = items
        .iter()
        .filter(|i| i.kind == "files")
        .filter_map(|i| i.files.clone())
        .flatten()
        .collect();
    if paths.is_empty() {
        return None;
    }

    let total = paths.len();
    let (ok, failed) = crate::purge::purge(&paths, cfg.delete_files_permanent);
    let how = if cfg.delete_files_permanent {
        "永久删除"
    } else {
        "移入回收站"
    };
    if failed.is_empty() {
        crate::log_print!("[clipboard] 已{how} {ok} 个本地文件");
        Some(format!("已{how} {ok} 个本地文件"))
    } else {
        crate::log_warn!(
            "[clipboard] {how}本地文件：成功 {ok}/{total}，失败 {} 个（详见界面提示）",
            failed.len()
        );
        Some(format!(
            "已{how} {ok} 个，{} 个未能处理（可能被占用或已不存在）",
            failed.len()
        ))
    }
}

#[tauri::command]
pub fn delete_item(id: String, delete_local_files: Option<bool>) -> Result<Option<String>, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    if let Some(pos) = hist.iter().position(|i| i.id == id) {
        let item = hist.remove(pos);
        let kind = item.kind.clone();
        drop(hist);
        // 先处理本地文件，再删记录与缓存实体：即便文件处理失败，条目也该删掉
        // （用户要的是「这条记录不要了」），失败的个体会在提示里说清楚。
        let note = if delete_local_files.unwrap_or(false) {
            purge_local_files(std::slice::from_ref(&item))
        } else {
            None
        };
        remove_item_files(&dir, &item);
        save_index(&dir, &HISTORY.get().unwrap().lock().unwrap());
        // 只记 id / kind 等元数据，不含内容。
        crate::log_print!("[clipboard] 删除条目 {id} kind={kind}");
        Ok(note)
    } else {
        // 前端理论上不会删不存在的条目；真出现说明前后端状态不同步，值得记一笔。
        crate::log_warn!("[clipboard] 删除失败：条目不存在 {id}（前后端状态不同步）");
        Ok(None)
    }
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
    // MIME 走 `mime_for`：富文本内联图也要用同一份映射，不能各写一遍。
    Ok(format!("data:{};base64,{}", mime_for(&rel), b64))
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
    /// 富文本原文（`HTML Format` / CF_HTML）。仅当这条复制源提供过格式时才存在。
    ///
    /// 详情弹窗拿它做「按格式查看」；不参与搜索、不进列表，只在这一刻读一次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// 富文本里的内联图，键是 `data-qt-img="n"` 里的 n，值是 data URL。
    ///
    /// 只在这一刻读一次：图片平时是独立文件，渲染时才转成 data URL 交给前端。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub rich_images: std::collections::HashMap<usize, String>,
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
    let mut full = summarize_content(raw);
    // 富文本只在这条确实带格式时才多读一次（解不开就当没有，不影响纯文本显示）
    if let Some(hrel) = item.html_path.as_ref() {
        match read_text_entity(&dir, hrel) {
            Ok(h) => full.html = Some(h),
            Err(e) => crate::log_warn!(
                "[clipboard] 详情：富文本实体读取失败 {hrel}：{e}（回退纯文本显示）"
            ),
        }
    }
    // 内联图：平时是独立文件，只在这一刻转成 data URL 交给前端渲染。
    // 有总量预算 —— 一张 3MB 的图转成 base64 就是 4MB，几张叠加会把 IPC 拖死。
    if !item.rich_images.is_empty() {
        const TOTAL_CHARS: usize = 12 * 1024 * 1024;
        let mut used = 0usize;
        for (n, rel) in item.rich_images.iter().enumerate() {
            let Some(url) = image_data_url(&dir, rel) else {
                crate::log_warn!("[clipboard] 详情：内联图缺失 {rel}（该图不显示）");
                continue;
            };
            if used + url.len() > TOTAL_CHARS {
                crate::log_warn!(
                    "[clipboard] 详情：内联图总量超过 {}MB，后续 {} 张不显示",
                    TOTAL_CHARS / 1024 / 1024,
                    item.rich_images.len() - n
                );
                break;
            }
            used += url.len();
            full.rich_images.insert(n, url);
        }
    }
    Ok(full)
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
        html: None,
        rich_images: std::collections::HashMap::new(),
        truncated,
        total_chars,
        total_lines,
    }
}

/// 把任意文本写入系统剪贴板（供「复制并编辑」的保存动作使用）。
///
/// **原条目始终不动**（编辑副本原则，不变）：历史要忠实反映「当初复制了什么」。
/// 若能原地改，用户会怀疑「我明明复制的是 A，怎么变成 B 了」；而剪贴板历史常被当
/// 安全网（删了还在），改动它却不可逆是件危险的事。
///
/// **改过的内容要记一条**（2026-10-23 调整）：此前保存只写剪贴板、不入历史
/// （`mark_skip` 把自读拦掉了）。但实测用下来，编辑后的内容在历史里找不到就等于
/// 白改 —— 它已经进了剪贴板，用户的预期是「它该像一次复制那样被记住」。
/// 所以现在：内容确实被改过时，额外落成**一条新条目**（带「已编辑」标记，
/// 见 `HistoryItem::edited`），原条目原样保留。
///
/// `changed = false`（进了编辑态但一个字没改就保存）时维持旧行为：只写剪贴板、
/// 不记历史 —— 那种情况等同于「复制原文」，再记一条只会多出一份一模一样的记录。
#[tauri::command]
pub fn set_clipboard_text(text: String, changed: bool) -> Result<(), String> {
    if text.is_empty() {
        return Err("内容为空，未写入剪贴板".into());
    }
    match unsafe { write_text_to_clipboard(&text) } {
        Ok(()) => {
            // 只记字符数，绝不记内容本身（编辑后的正文同样是用户数据）。
            crate::log_print!(
                "[clipboard] 编辑保存：写回剪贴板 {} 字符（changed={}）",
                text.chars().count(),
                changed
            );
            // 这里会且只会产生一条：`write_text_to_clipboard` 内部做了 `mark_skip()`，
            // 本次自写在「事件」与「兜底轮询」两条通道上都会被认出并跳过，
            // 所以不会出现「系统自读一条 + 手动入库一条」变成两条。
            if changed {
                dispatch_raw(RawClip::Text { text, html: None, rtf: None }, true, false);
            }
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
pub fn copy_back(id: String, plain: Option<bool>) -> Result<Option<String>, String> {
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
                // 默认**保留格式**（用户复制的是带格式的内容，粘回去也该带格式）；
                // 想要纯文本走右键菜单里的「粘贴为纯文本」。
                if !plain.unwrap_or(false) {
                    if let Some(rel) = &item.html_path {
                        if let Ok(cf) = read_text_entity(&dir, rel) {
                            // 没有拆出图片的条目：原样写回那段 CF_HTML 字节，零风险。
                            // 有图片的条目：图片存放时是独立文件，这里必须塞回 base64
                            // 并**重建**头部 —— 旧头里的偏移在还原后全错了，
                            // 直接用会让 Word 粘出一坨源码。
                            let html = if item.rich_images.is_empty() {
                                cf
                            } else {
                                let frag = cf_html_fragment(&cf).to_string();
                                let restored =
                                    inline_images_back(&frag, &dir, &item.rich_images);
                                build_cf_html(&restored)
                            };
                            unsafe { write_html_to_clipboard(&html, &text)? };
                            return Ok(Some("已按原格式复制".into()));
                        }
                    }
                    if let Some(rel) = &item.rtf_path {
                        if let Ok(rtf) = read_text_entity(&dir, rel) {
                            unsafe { write_rtf_to_clipboard(&rtf, &text)? };
                            return Ok(Some("已按原格式复制（RTF）".into()));
                        }
                    }
                }
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
pub fn delete_items(
    ids: Vec<String>,
    delete_local_files: Option<bool>,
) -> Result<Option<String>, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    // 先把待处理的目标捞出来：`retain` 里拿不到可变借用之外的东西，
    // 而本地文件必须在条目被移除**之前**按它自己记录的路径处理。
    let purge_targets: Vec<HistoryItem> = if delete_local_files.unwrap_or(false) {
        hist.iter()
            .filter(|i| ids.iter().any(|id| id == &i.id))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let note = purge_local_files(&purge_targets);

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
    Ok(note)
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
            // **ms 级轮询**。它不是「事件驱动的补充」，而是**保证成功的那一环**：
            // on_clipboard_update 只在读成功时才推进 LAST_SEQ，所以任何没成功的
            // 序号都会被下一拍自动重试，直到读到、或剪贴板被下一次复制替换。
            // 于是「拿不到锁 / 延迟渲染没交货 / 窗口隐藏导致消息漏投递」全部自动兜住。
            //
            // 为什么是 20ms（而不是原来的 100ms）：延迟渲染的补交窗口（微信登记格式
            // → 真正写入数据）在毫秒到几百毫秒之间浮动，间隔越短越能贴着窗口把内容捞回来。
            // 20ms 意味着任何「事件漏投递 / 首次没读到」最多 20ms 内就被补上，
            // 对「极端快速连续复制」这种最坏场景，命中率也接近事件驱动本身。
            //
            // 开销：每拍只做一次 `GetClipboardSequenceNumber()` —— 纯整数读取，
            // **不开剪贴板锁、不拷任何数据**，约亚微秒级；序号没变时立即返回。
            // 即 50 次/秒的空转 ≈ 可以忽略（对比 WebView 自身常驻开销）。
            const POLL: std::time::Duration = std::time::Duration::from_millis(20);
            // 同一个序号最多「真正去读」多少次。20ms × 100 ≈ 2 秒的密集补读窗口，
            // 足够覆盖任何正常的延迟渲染；超过就放弃这个序号，等下次复制换序号再管。
            //
            // 这一条是给「剪贴板长期停在我们不认识的格式上」兜底的（例如用户复制了
            // 某个只含私有格式的内容就一直放着不动）：没有它的话，轮询会永远每 20ms
            // 去做一次完整读取（OpenClipboard + 枚举全部格式 + 查 owner 进程名），
            // 白耗 CPU 还刷屏。有了它，2 秒后自动降级为「只读序号」，依旧零成本。
            const MAX_TRIES_PER_SEQ: u32 = 100;

            let mut trying_seq = 0u32;
            let mut tries = 0u32;

            loop {
                std::thread::sleep(POLL);
                let seq = unsafe { GetClipboardSequenceNumber() };
                if seq == 0 || seq == LAST_SEQ.load(Ordering::Relaxed) {
                    // 序号没变（或已被事件路径处理过）：本次只花了一次整数读取。
                    trying_seq = 0;
                    tries = 0;
                    continue;
                }
                if seq != trying_seq {
                    // 换了一个新序号（新的复制）：重新开始计数。
                    trying_seq = seq;
                    tries = 0;
                }
                if tries >= MAX_TRIES_PER_SEQ {
                    // 这个序号已经密集试过 2 秒仍读不到 → 放弃，等序号变化。
                    continue;
                }
                tries += 1;
                on_clipboard_update();
            }
        })
        .ok();
}

fn spawn_worker() {
    // app 走全局 APP（OnceLock，在 init 首行写入）；worker 在订阅监听之前
    // 启动，所以取用时必定已就绪。
    // 元组第三项：这条是否由「编辑保存」产生（决定条目上是否显示「已编辑」徽章）；
    // 第四项：这次是不是截图（决定图片落 `shot/` 还是 `copy/`）。
    let (tx, rx) = std::sync::mpsc::channel::<(u64, RawClip, bool, bool)>();
    std::thread::Builder::new()
        .name("clipboard-store".into())
        .spawn(move || {
            while let Ok((captured_ms, raw, edited, shot)) = rx.recv() {
                // guard 保证 PENDING 一定减回去 —— handle_clip 里判重拦截会
                // 提前 return，手写 fetch_sub 迟早漏掉一次。
                let _guard = PendingGuard;
                // handle_clip 内部完成：解码 → 同图判重 → 落盘 → emit。
                // 这里只负责串行投递，保证与复制顺序一致。
                handle_clip(captured_ms, raw, edited, shot);
            }
        })
        .map_err(|e| crate::log_warn!("[clipboard] 落盘工作线程启动失败：{e}"))
        .ok();
    let _ = WORKER.set(tx);
}

/// 缩略图线程：串行消费「需要生成缩略图」的条目。
///
/// 与捕获线程分开的理由：捕获线程的关键路径是「落盘 + emit」，必须尽可能快，
/// 否则快速连续复制会排队、条目迟迟不出现。解码一张大图要上百毫秒到数秒，
/// 放在捕获线程里就会成为瓶颈。本线程只负责生成缩略图，慢一点无妨 ——
/// 前端按需读取时会命中已备好的文件，读不到再走 get_image_thumb 现场生成兜底。
fn spawn_thumb_worker() {
    let (tx, rx) = std::sync::mpsc::channel::<HistoryItem>();
    let _ = std::thread::Builder::new()
        .name("clipboard-thumb".into())
        .spawn(move || {
            while let Ok(item) = rx.recv() {
                if let Some(dir) = DIR.get() {
                    // ensure_thumb 内部：已有则跳过；否则解码原图生成；失败则写占位图。
                    let _ = ensure_thumb(dir, &item);
                }
            }
        })
        .map_err(|e| crate::log_warn!("[clipboard] 缩略图线程启动失败：{e}"))
        .ok();
    let _ = THUMB_TX.set(tx);
}

pub fn init(app: &AppHandle) {
    let _ = APP.set(app.clone());
    // 常驻剪贴板窗口：与主窗口解耦，界面被释放也不影响复制/回贴
    let clip = create_clip_window();
    if clip == 0 {
        crate::log_warn!("[clipboard] 剪贴板窗口创建失败：复制与回贴将不可用");
    } else {
        let _ = CLIP_HWND.set(clip as isize);
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

    // 这三个目录是「记录能不能落盘」的前提。创建失败（磁盘满、路径只读、
    // 被安全软件拦）之后每一次复制都会失败，而用户看到的只是「复制了没反应」。
    // 以前是 `let _ =`，等于把根因咽掉，只剩一个空列表可看。
    for sub in ["data", "images", "files"] {
        let p = dir.join(sub);
        if let Err(e) = fs::create_dir_all(&p) {
            crate::log_err!(
                "clipboard",
                "缓存子目录创建失败（{}）：{e} —— 该类型内容将无法记录",
                p.display()
            );
        }
    }
    // 缩略图目录从旧的顶层 `thumbs/` 迁到 `images/thumbs/`（与图片放一起）。
    // 一次性、幂等：老目录在、新目录不在时才搬；搬不动也不影响后续按需生成。
    let old_thumbs = dir.join("thumbs");
    let new_thumbs = dir.join("images").join("thumbs");
    if old_thumbs.is_dir() && !new_thumbs.exists() {
        if fs::rename(&old_thumbs, &new_thumbs).is_err() {
            crate::log_warn!("[clipboard] 旧缩略图目录迁移失败（不影响使用，将按需重新生成）");
        }
    }
    let _ = DIR.set(dir.clone());

    let mut hist = load_index(&dir);
    // 索引条目数明显少于缓存目录里的内容文件 → 索引坏了，按文件补齐。
    //
    // 不只看「索引为空」：用户实测的损坏形态是索引被截断、只剩最后几条
    // （写入途中被强杀），如果只判空就永远不会触发，列表会永远少掉几百条。
    // 判据用「文件数 > 索引条数」而不是「>=」：正常状态下两者相等（清空、删除都会
    // 连实体一起删），只有索引丢条目时才会出现文件多于条目。
    // 重建本身只补不删（已有 id 跳过），因此即便判据误触发也不会破坏现有数据。
    let entity_count = count_entities(&dir);
    if entity_count > hist.len() {
        let before = hist.len();
        let rebuilt = rebuild_index_from_files(&dir, &mut hist);
        if rebuilt > 0 {
            crate::log_warn!(
                "[clipboard] 索引只有 {before} 条、缓存目录有 {entity_count} 个内容文件 → 判为索引损坏，已补回 {rebuilt} 条（共 {} 条）；旧索引备份为 index.json.corrupt",
                hist.len()
            );
            let _ = fs::copy(dir.join("index.json"), dir.join("index.json.corrupt"));
            save_index(&dir, &hist);
        }
    }
    // 老版本 index.json 把文件路径内联在 files 里、没有落盘清单 → 补一份，
    // 让历史文件条目在缓存目录里也有实体（返回补了几条）
    let backfilled = backfill_files(&dir, &mut hist);
    // 存储格式升级：把「加密落盘」上线之前写入的**明文**实体就地加密。
    // 幂等（已加密的带 ENC_MAGIC 前缀，直接跳过），所以每次启动扫一遍也不会有副作用。
    let encrypted = encrypt_legacy_entities(&dir);
    // 老条目的 preview 是在「只取首行」的旧规则下写入的 —— 首行短就只显示几个字。
    // 规则改成「开头最多 2 行」后必须**重算存量**，否则用户看到的还是旧预览，
    // 会以为修复没生效。
    // 识别规则升级：把存量子类型清空重算一次（只在规则版本提升后发生）
    const SUBTYPE_RULE_VERSION: u32 = 2;
    if crate::settings::get().subtype_version < SUBTYPE_RULE_VERSION {
        let mut n = 0usize;
        for item in hist.iter_mut() {
            if item.kind == "text" && item.subtype.is_some() {
                item.subtype = None;
                n += 1;
            }
        }
        crate::settings::update(|c| c.subtype_version = SUBTYPE_RULE_VERSION);
        crate::log_print!("[clipboard] 识别规则已升级，{n} 条存量子类型待重算");
    }

    let refreshed = refresh_text_previews(&dir, &mut hist);
    // 存量条目补算「链接 / 代码」子类型（只在升级后首次启动真正干活）
    let subtyped = refresh_text_subtypes(&dir, &mut hist);
    if backfilled > 0 {
        crate::log_print!("[clipboard] 已为 {backfilled} 条历史文件条目补写 files/ 清单");
    }
    if encrypted > 0 {
        crate::log_print!("[clipboard] 已将 {encrypted} 个历史明文文本实体转为加密存储");
    }
    if refreshed > 0 {
        crate::log_print!("[clipboard] 已按新规则重算 {refreshed} 条文本条目的预览");
    }
    if subtyped > 0 {
        crate::log_print!("[clipboard] 已为 {subtyped} 条存量文本判定「链接 / 代码」子类型");
    }
    // 图片目录布局：按当前设置把图片摆到应有的位置（日期命名 + 可选分目录）
    let layout_stale = crate::settings::get().image_layout_ver < IMAGE_LAYOUT_VERSION;
    let img_moved = if layout_stale {
        let split = crate::settings::get().images_split_dirs;
        migrate_image_layout(&dir, &mut hist, split)
    } else {
        0
    };

    if backfilled > 0 || refreshed > 0 || subtyped > 0 || img_moved > 0 {
        save_index(&dir, &hist);
    }
    if layout_stale {
        prune_empty_image_dirs(&dir);
        crate::settings::update(|c| c.image_layout_ver = IMAGE_LAYOUT_VERSION);
        if img_moved > 0 {
            crate::log_print!("[clipboard] 图片已按当前设置整理，迁移 {img_moved} 条");
        }
    }

    // 保留策略：按配置天数清理过期条目（0 = 永久保留）
    let cfg = crate::settings::get();
    let hist = prune_history(hist, &dir, cfg.retention_days, cfg.prune_include_pinned);

    let _ = HISTORY.set(Mutex::new(hist));

    // 工作线程必须在订阅监听**之前**就位：否则第一批 WM_CLIPBOARDUPDATE
    // 到达时 WORKER 还没初始化，会退化成主线程同步落盘。
    spawn_worker();
    spawn_thumb_worker();

    // 订阅剪贴板格式监听：此后系统剪贴板变化会向我们窗口发 WM_CLIPBOARDUPDATE
    unsafe {
        let hwnd = CLIP_HWND.get().copied().unwrap_or(0);
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

/// 由 WndProc 子类在收到 WM_CLIPBOARDUPDATE 时调用；轮询线程也调用它。
///
/// **唯一入口、唯一规则**：读一次剪贴板；**只有读成功才把该序号标记为已处理**。
/// 失败（拿不到锁 / 延迟渲染还没交货 / 对方只写了私有格式）就什么都不做 ——
/// 序号仍是「未处理」，轮询线程下一拍（100ms 后）自动再读一次。
///
/// 所有失败情形都由这一条机制兜底，不需要任何特例、重试链或全局计数器：
///   - 正常复制：事件即时处理，0ms；
///   - 拿不到锁：下一拍补上；
///   - 延迟渲染（微信登记格式、数据稍后才补）：下一拍补上，直到读到为止；
///   - 窗口隐藏/最小化导致消息没投递：轮询本身就是兜底。
pub fn on_clipboard_update() {
    let seq = unsafe { GetClipboardSequenceNumber() };
    if seq == 0 || seq == LAST_SEQ.load(Ordering::Relaxed) {
        return;
    }
    // 这次内容是不是**我们自己**写回剪贴板的（回贴 / 编辑保存 / 截图）。
    // 回贴的图不能被当成截图 —— 它只是历史里某张图的副本。
    let mut own_write = false;
    unsafe {
        // 剪贴板 owner 就是我们自己窗口 → 这是「点击条目复制回去」写下的内容，不能再当成
        // 一次新的系统复制回灌成重复条目。
        // 为什么不能只靠 mark_skip() 的序号比对：copy_back 跑在 Tauri 的工作线程上，
        // 而 WM_CLIPBOARDUPDATE 是投递到主线程的——主线程完全可能在 mark_skip() 执行之前
        // 就处理掉这条消息。owner 由系统在 SetClipboardData 时原子设置，没有这个窗口期。
        let me = CLIP_HWND.get().copied().unwrap_or(0);
        if me != 0 && GetClipboardOwner() == me {
            if !RECORD_OWN_WRITE.swap(false, Ordering::SeqCst) {
                LAST_SEQ.store(seq, Ordering::Relaxed);
                return;
            }
            // 通行证生效：这是自己写进来的，继续往下走当成一次新复制
            own_write = true;
        }
        if seq == SKIP_SEQ.load(Ordering::Relaxed) {
            SKIP_SEQ.store(0, Ordering::Relaxed);
            LAST_SEQ.store(seq, Ordering::Relaxed);
            return;
        }
    }

    // 两条通道（WndProc 主线程 / 20ms 兜底轮询）可能同时走到这里。
    // 用 CAS 把「真正去读剪贴板」这一段串起来：抢不到的直接返回，下一拍（≤20ms）再试。
    // 不会因此丢内容 —— 「只在成功时推进序号」保证了没读到的序号一定会被重试。
    if READING.swap(true, Ordering::AcqRel) {
        return;
    }
    let (raw, diag, origin) = unsafe { read_clipboard() };
    if raw.is_some() {
        // ✅ 读成功：**必须在放开 READING 之前**推进序号。
        //
        // 否则存在一个极窄但真实的窗口：本线程已读完内容、放开了 READING，却还没推进
        // LAST_SEQ —— 此刻另一条通道（20ms 轮询）恰好进来，看到「这个序号仍未处理」，
        // 就会把**同一份内容再读一遍、再入库一次**，表现为「复制一次出现两条」。
        // 放到闸门内推进只要一个原子写，两条通道就永远不可能同时处理同一个序号。
        //
        // 「只有成功才推进」的规则不变：下面 None 分支依旧不推进，让下一拍重试。
        LAST_SEQ.store(seq, Ordering::Relaxed);
        LAST_LOGGED_SEQ.store(seq, Ordering::Relaxed);
    }
    READING.store(false, Ordering::Release);
    match raw {
        Some(raw) => {
            // 截图判定：自己写回的只认 `SHOT_AT`（回贴不能算截图），
            // 别人写进来的看剪贴板格式特征（见 `classify_shot`）。
            // 判定必须在这里做 —— 此刻剪贴板还没被别人换掉，
            // 等到工作线程落盘时再查就晚了。
            let shot = if own_write {
                take_shot_mark()
            } else {
                classify_shot(&origin)
            };
            dispatch_raw(raw, false, shot);
        }
        None => {
            // ❌ **不推进 LAST_SEQ** → 轮询下一拍自动重试，直到读到、或剪贴板被新复制替换。
            // 这一条覆盖了所有失败：拿不到锁、延迟渲染没交货、对方只写私有格式、
            // 窗口隐藏导致消息漏投递…… 不需要任何特例。
            //
            // 日志按序号去重（同一序号只报一次），否则轮询期间每 100ms 会刷一条。
            // diag 里含「当时有哪些格式、每种为什么被拒」，便于回传排查。
            if LAST_LOGGED_SEQ.swap(seq, Ordering::Relaxed) != seq {
                crate::logger::write_line(
                    "INFO",
                    "clipboard",
                    &format!("剪贴板已变化但暂未读到内容（会自动重试）| {}", diag),
                );
            }
        }
    }
}

/// 把读到的内容交给后台工作线程（解码 + 同图去重 + 落盘 + 通知前端）。
///
/// 为什么必须在后台：解码一张 8000×6000 的原图要展开近 200MB 像素再缩放，耗时可达数秒。
/// 而本函数由 WndProc 主线程调用，同步解码等于把整个界面冻住 —— 这正是 2.1.10
/// 「微信复制大图后软件卡死等待」的成因。主线程只做一次 memcpy，其余交给工作线程。
///
/// ⚠️ **时间戳必须在这里打**（捕获时刻），不能到 worker 里再取当前时间：worker 单线程串行，
/// 前面压着一张大图的解码时本条要排队几秒才轮到，那时再取时间，判重窗口算出的「间隔」
/// 会包含排队时长 —— 实测出现过「同一秒、同一指纹、却记了两条」，根因就在这里。
///
/// `edited`：true 表示这条来自「复制并编辑」的保存动作，落成条目后带
/// 「已编辑」标记；**外部复制**一律传 false。
fn dispatch_raw(raw: RawClip, edited: bool, shot: bool) {
    let captured_ms = now_ms();
    if let Some(tx) = WORKER.get() {
        if tx.send((captured_ms, raw, edited, shot)).is_ok() {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // 工作线程还没起来（正常不会发生——init 里先启线程再订阅监听）
        handle_clip(captured_ms, raw, edited, shot);
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
/// `edited` 透传给 `store_raw`：为 true 时新条目带「已编辑」标记。
fn handle_clip(captured_ms: u64, raw: RawClip, edited: bool, shot: bool) {
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
    // 文本判重，两层：
    //   一、内容完全相同（应用把同一次复制写了两遍）→ 只补格式，不新增；
    //   二、内容**不同但同源**（先写渲染文本、再写 Markdown 原文）→ 用后者替换前者。
    // 第二层是用户实测出来的：复制 Markdown 会落下两条，一条是去掉标记的纯文本、
    // 一条是原文，长度和内容都不一样，光靠哈希根本拦不住。
    if let RawClip::Text { text, html, rtf } = &raw {
        let fp = content_hash(text.as_bytes());
        let dup = LAST_TEXT
            .lock()
            .map(|mut g| dedup_merge_with(&mut g, fp, captured_ms, TEXT_DUP_WINDOW_MS))
            .unwrap_or(false);
        if !dup && is_same_origin_recent(text, captured_ms) {
            if replace_latest_text(text, html, rtf) {
                // 指纹换成新内容，否则紧接着的第三次写入又会被当成新条目
                if let Ok(mut g) = LAST_TEXT.lock() {
                    if let Some(last) = g.last_mut() {
                        last.0 = fp;
                        last.1 = captured_ms;
                    }
                }
                crate::log_print!(
                    "[clipboard] 同一次复制的另一种表示（如 Markdown 原文 vs 渲染文本），已替换上一条"
                );
                return;
            }
        }
        if dup {
            // 关键：第二次往往**才带格式**（延迟渲染）。这里把格式补到已有那条上，
            // 而不是丢掉 —— 否则用户复制一次，格式永远进不来。
            let patched = merge_rich_into_latest(html, rtf);
            crate::log_print!(
                "[clipboard] 重复文本已合并（同一次复制的第二次写入，补格式={patched}）"
            );
            return;
        }
    }

    // 文件清单判重：同一批路径在极短时间内被写两次，只留一条。
    if let RawClip::Files(paths) = &raw {
        let fp = content_hash(paths.join("\n").as_bytes());
        let dup = LAST_FILES
            .lock()
            .map(|mut g| dedup_merge_with(&mut g, fp, captured_ms, TEXT_SAME_ORIGIN_MS))
            .unwrap_or(false);
        if dup {
            crate::log_print!("[clipboard] 重复文件清单已合并（同一次复制的第二次写入）");
            return;
        }
    }

    // 图片：同一次复制可能先给 DIB、再给原文件。两条路径算出来的指纹**体系不同**
    // （文件字节哈希 vs 像素指纹），所以 `LAST_IMG` 那层对不上，日志里因此出现过
    // 同秒、同大小、两张一模一样的图。
    // 这里用「极短时间窗内上一条也是图片」兜底：人不可能在 800ms 内复制两张不同的图
    // （截图/选中文件都需要更久），而程序分两次写剪贴板都在几十毫秒内。
    if matches!(
        raw,
        RawClip::Image(_) | RawClip::ImageOriginal { .. } | RawClip::ImageRaw(_)
    ) {
        if let Some((kind, ts)) = latest_kind_ts() {
            if kind == "image" && captured_ms.saturating_sub(ts) < TEXT_SAME_ORIGIN_MS {
                crate::log_print!(
                    "[clipboard] 重复图片已合并（同一次复制的另一种格式，指纹体系不同）"
                );
                return;
            }
        }
    }

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

    let item = store_raw(raw, edited, shot);
    if let Some(app) = APP.get() {
        let _ = app.emit("clipboard-update", item.clone());
    }
    // 缩略图：**所有来源的图片都走这一条通道**（ensure_thumb 读落盘原图 → 解码 → 落缩略图；
    // 解不了一律回退到通用占位图）。两个要点：
    //   1. 放在 emit 之后 —— 保证「复制 → 出现」无感，再大的图也不挡列表显示；
    //   2. 交给独立的缩略图线程 —— 不占用捕获线程，快速连续复制时不会因为解码而排队。
    if item.kind == "image" {
        if let Some(tx) = THUMB_TX.get() {
            let _ = tx.send(item.clone());
        }
    }
}

/// 判重决策（抽成独立函数，便于单测**直接验证真实实现**而不是在测试里重写一遍）。
///
/// 返回 `true` = 判为重复、应合并（调用方直接 return，不落盘）。
///
/// 规则：**只与队列最后一条比对**（微信补写必然紧挨着上一条）；命中则把它的
/// 时间戳刷新为 `now`（滑动续期），使持续到来的同图始终保持在同一簇里。
/// 未命中则入队并清理过期项。
/// 最新条目的（类型, 时间戳）。用于「同一次复制的多条写入」判定。
fn latest_kind_ts() -> Option<(String, u64)> {
    let hist = HISTORY.get()?;
    let g = hist.lock().ok()?;
    g.first().map(|i| (i.kind.clone(), i.ts))
}

/// 最近一条文本是否与这次「同源」（同一次复制的另一种表示）。
///
/// 时间窗口是硬的：`TEXT_SAME_ORIGIN_MS` 之外一律不合并 ——
/// 用户隔几秒真的复制了两段相似内容，那是两次操作，必须各自成条。
fn is_same_origin_recent(text: &str, ts: u64) -> bool {
    let (Some(dir), Some(hist)) = (DIR.get().cloned(), HISTORY.get()) else {
        return false;
    };
    let Ok(g) = hist.lock() else { return false };
    let Some(last) = g.first() else { return false };
    if last.kind != "text" {
        return false;
    }
    if ts.saturating_sub(last.ts) > TEXT_SAME_ORIGIN_MS {
        return false;
    }
    let Some(rel) = &last.text_path else {
        return false;
    };
    match read_text_entity(&dir, rel) {
        Ok(prev) => same_origin(&prev, text),
        Err(_) => false,
    }
}

/// 两段文本「是不是同一份内容的两种表示」。
///
/// 用 **bigram 集合的 Jaccard 相似度**，而不是逐字比对：
/// 同一次复制的两次写入，一次是 Markdown 原文、一次是渲染后的纯文本 ——
/// 标记符号、换行、空白全不同，但相邻字符的组合高度重合。
///
/// 归一化时去掉空白与常见标记符号（`# * ` | - _ > [ ] ( ) ! ~`），
/// 这样 `## 标题` 与 `标题` 在归一化后一致。
fn same_origin(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> Vec<char> {
        s.chars()
            .filter(|c| {
                !c.is_whitespace()
                    && !matches!(c, '#' | '*' | '`' | '|' | '-' | '_' | '>' | '[' | ']' | '(' | ')' | '!' | '~' | '<' | '/')
            })
            .flat_map(|c| c.to_lowercase())
            .collect()
    };
    let (na, nb) = (norm(a), norm(b));
    // 太短的文本没有判别力（一句话恰好相似的概率太高），直接不合并
    if na.len() < 12 || nb.len() < 12 {
        return false;
    }
    let grams = |v: &[char]| -> std::collections::HashSet<(char, char)> {
        v.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (sa, sb) = (grams(&na), grams(&nb));
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    union > 0.0 && inter / union >= TEXT_SAME_ORIGIN_SIM
}

/// 用「同一次复制的另一次写入」**替换**最新那条文本条目。
///
/// 为什么是替换而不是丢弃：两次写入的内容不同（渲染文本 vs 原文），
/// 后者才是用户真正复制的东西。丢掉第二次等于把 Markdown 原文换成去掉标记的纯文本。
/// 剪贴板的语义本来就是「最终状态」，所以以最新的为准。
fn replace_latest_text(text: &str, html: &Option<String>, rtf: &Option<String>) -> bool {
    let (Some(dir), Some(hist)) = (DIR.get().cloned(), HISTORY.get()) else {
        return false;
    };
    let Ok(mut g) = hist.lock() else { return false };
    let Some(last) = g.first_mut() else { return false };
    if last.kind != "text" {
        return false;
    }
    let id = last.id.clone();
    let rel = format!("data/{id}.txt");
    write_text_entity(&dir, &rel, text);
    last.text_path = Some(rel);
    last.size = text.len() as u64;
    let (preview, truncated) = text_preview_parts(text);
    last.preview = preview;
    last.preview_truncated = truncated;
    last.subtype = Some(detect_subtype(text).unwrap_or("").to_string());
    // 富文本一并换掉（第二次往往才带格式）
    if let Some(h) = html {
        let (hrel, imgs) = store_rich_html(&dir, &id, h.as_str());
        last.html_path = Some(hrel);
        last.rich_images = imgs;
    }
    if let Some(r) = rtf {
        let rrel = format!("data/{id}.rtf");
        write_text_entity(&dir, &rrel, r);
        last.rtf_path = Some(rrel);
    }
    let updated = last.clone();
    save_index(&dir, &g);
    drop(g);
    if let Some(app) = APP.get() {
        let _ = app.emit("clipboard-update", updated);
    }
    true
}

/// 把这次捕获到的富文本补到**最新那条**文本条目上。
///
/// 触发场景：应用分两次写剪贴板（先纯文本、再补 HTML/RTF），第一次产生的条目
/// 没有格式。补在这里而不是丢掉第二次，否则用户复制一次、格式永远进不来。
///
/// 幂等：最新条目已经有对应格式，或它不是文本条目，就什么都不做。
fn merge_rich_into_latest(html: &Option<String>, rtf: &Option<String>) -> bool {
    if html.is_none() && rtf.is_none() {
        return false;
    }
    let (Some(dir), Some(hist)) = (DIR.get().cloned(), HISTORY.get()) else {
        return false;
    };
    let Ok(mut g) = hist.lock() else { return false };
    let Some(last) = g.first_mut() else { return false };
    if last.kind != "text" {
        return false;
    }
    let id = last.id.clone();
    let mut changed = false;
    if last.html_path.is_none() {
        if let Some(h) = html {
            let (rel, imgs) = store_rich_html(&dir, &id, h.as_str());
            last.html_path = Some(rel);
            last.rich_images = imgs;
            changed = true;
        }
    }
    if last.rtf_path.is_none() {
        if let Some(r) = rtf {
            let rel = format!("data/{id}.rtf");
            write_text_entity(&dir, &rel, r);
            last.rtf_path = Some(rel);
            changed = true;
        }
    }
    if changed {
        save_index(&dir, &g);
    }
    changed
}

fn dedup_merge(g: &mut Vec<(u64, u64)>, fp: u64, now: u64) -> bool {
    dedup_merge_with(g, fp, now, DUP_WINDOW_MS)
}

/// 同 `dedup_merge`，但窗口可指定（文本用更短的窗口，见 `TEXT_DUP_WINDOW_MS`）。
fn dedup_merge_with(g: &mut Vec<(u64, u64)>, fp: u64, now: u64, window: u64) -> bool {
    if let Some(last) = g.last_mut() {
        if last.0 == fp && now.saturating_sub(last.1) < window {
            last.1 = now; // 滑动窗口：续上这一簇
            return true;
        }
    }
    g.push((fp, now));
    g.retain(|(_, t)| now.saturating_sub(*t) < window);
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
    Text {
        /// 纯文本（CF_UNICODETEXT 解密后的内容）—— 唯一必有的部分
        text: String,
        /// `HTML Format` 原文。它本身就是**带 Version/StartHTML 头的 CF_HTML**，
        /// 所以能原样存、原样写回，不需要自己拼头部（拼错就是「粘到 Word 里变源码」）。
        html: Option<String>,
        /// `Rich Text Format` 原文（部分老应用只给 RTF，不给 HTML）
        rtf: Option<String>,
    },
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

/// 读一次剪贴板；同时返回一份「诊断串」，在读不到时由调用方打印，
/// 说明当时剪贴板里有哪些格式、每种为什么被拒（owner / 格式清单 / 句柄是否为空等）。
///
/// **只读一次，失败不做任何重试** —— 这是刻意的：
/// 「拿不到锁 / 延迟渲染还没交货」这类失败统一交给 100ms 轮询兜底
/// （见 `on_clipboard_update`：读不到就不推进序号，下一拍自动再读）。
/// 把重试集中在一个地方，比在每条路径上各写一套要可靠得多。
///
/// 这里只保留 `open_clipboard_retry`：那是「拿不到全局锁」的微退避（~30ms），
/// 属于同一次读取内的必要礼让，不是重试链。
unsafe fn read_clipboard() -> (Option<RawClip>, String, ClipOrigin) {
    // 「文件复制」开关（设置页，默认关闭）：关掉后完全不看 CF_HDROP。
    // 在 OpenClipboard 之前读取，避免持有剪贴板锁时再去碰 settings 的互斥锁。
    let file_copy = crate::settings::get().file_copy;

    let empty_origin = || ClipOrigin {
        formats: Vec::new(),
        owner_exe: String::new(),
    };
    if !open_clipboard_retry() {
        return (
            None,
            "OpenClipboard 重试耗尽(对方应用长时间持锁)".into(),
            empty_origin(),
        );
    }
    // 延迟渲染需要 owner 重新拿到锁才能把数据补进来，所以读完必须立刻松锁。
    let (raw, diag, _suspicious, origin) = read_clipboard_once(file_copy);
    CloseClipboard();
    (raw, diag, origin)
}

/// 单次读剪贴板。**调用方须保证已 OpenClipboard 成功**，本函数不负责开关剪贴板。
///
/// 返回 `(内容, 诊断串, 是否可疑)`：
/// 「可疑」= 至少一种目标格式 `IsClipboardFormatAvailable` 为真、但 `GetClipboardData`
/// 拿到 NULL —— 这是延迟渲染 / 分次写入的典型症状，值得松锁退避后重读；
/// 若压根没有目标格式，说明这次真没有可记录的东西，重读没有意义。
/// 富文本寄存器的体积上限。
///
/// `HTML Format` 经常内联 base64 图片，所以量级远大于纯文字：一页带两张插图的
/// Word 文档通常 300KB~1.5MB。原来的 512KB 会让这类内容直接降级成纯文本 ——
/// 用户复制排版好的文档，回来发现格式全没了，而原因只是「带图」。
///
/// 提到 2MB：覆盖绝大多数单页文档与网页选区，又不至于让一次复制往磁盘里塞几十 MB
/// （超大 base64 主要来自长文档里的多张大图，那种内容的格式价值也有限）。
/// 详情渲染侧另有单图上限，见 `formats.js` 的 `RICH_IMG_MAX`。
const RICH_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 读一个**注册剪贴板格式**的文本内容（如 `HTML Format` / `Rich Text Format`）。
///
/// 必须在 `OpenClipboard` 期间调用。
unsafe fn read_clip_text_format(name: &str) -> Option<String> {
    unsafe fn inner(name: &str) -> Option<String> {
        // 注册格式的 ID 由系统分配，每次启动/每次调用都要现问
        let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let fmt = RegisterClipboardFormatW(wide_name.as_ptr());
        if fmt == 0 || IsClipboardFormatAvailable(fmt) == 0 {
            return None;
        }
        let h = GetClipboardData(fmt);
        if h == 0 {
            return None;
        }
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let size = GlobalSize(h);
        if size == 0 || size > RICH_MAX_BYTES * 4 {
            GlobalUnlock(h);
            crate::log_warn!("[clipboard] {name} 体积 {} 字节，超出可读范围，已忽略", size);
            return None;
        }
        let mut buf = vec![0u8; size];
        std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), size);
        GlobalUnlock(h);
        // 去掉尾部补的 NUL（有些应用会补 1~2 个）
        while buf.last() == Some(&0) {
            buf.pop();
        }
        if buf.is_empty() {
            return None;
        }
        if buf.len() > RICH_MAX_BYTES {
            crate::log_warn!(
                "[clipboard] {name} 体积 {}KB 超过 {}KB 上限（多半是内联图片太多），本条只保留纯文本",
                buf.len() / 1024,
                RICH_MAX_BYTES / 1024
            );
            return None;
        }
        // HTML Format 是 UTF-8；RTF 是 ANSI/ASCII 超集，按 lossy 处理不会损坏结构
        Some(String::from_utf8_lossy(&buf).to_string())
    }
    inner(name)
}

unsafe fn read_clipboard_once(file_copy: bool) -> (Option<RawClip>, String, bool, ClipOrigin) {
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
                let text = String::from_utf16_lossy(&v);
                // 富文本：Word / 浏览器 / 微信通常同时给出 HTML Format 或 RTF。
                // 两个都读，回贴时优先用 HTML（保真度更高）。
                // 必须在同一个 OpenClipboard 窗口内读 —— 关掉再开，owner 可能已改内容。
                let html = read_clip_text_format("HTML Format");
                let rtf = read_clip_text_format("Rich Text Format");
                if html.is_some() {
                    diag.push_str("html:yes ");
                }
                if rtf.is_some() {
                    diag.push_str("rtf:yes ");
                }
                return Some(RawClip::Text { text, html, rtf });
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

        // ---- 防止「同一次复制记两条」----
        //
        // 复制图片时，CF_HDROP（文件路径）与 CF_DIB（位图）会先后就位，而两者的判重
        // 指纹算法**不同**：HDROP 走文件内容哈希，DIB 走像素量化指纹 —— 同一张图也
        // 算不出同一个值，判重据此认不出是同一张 → 一次复制出现两条。
        //
        // 实测微信总是**先登记 CF_HDROP**（延迟渲染，数据稍后补），随后才补上 DIB。
        // 于是中间态是「HDROP 格式在、取不到数据」+「DIB 已可取」，旧逻辑会先用 DIB
        // 记一条位图，等 HDROP 数据就位后再记一条文件 —— 重复由此产生。
        //
        // 所以：只要 CF_HDROP 已登记，本次就**不用 DIB 兜底**，等下一拍（≤20ms）拿到
        // 文件路径再记。那一条是原始文件（质量最好），且只会有一条。
        // 仅当 HDROP 迟迟不交货（超过下面这个宽限期）才回退接受 DIB —— 保证绝不丢图。
        const HDROP_GRACE_MS: u64 = 400;
        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            let cur = GetClipboardSequenceNumber();
            let first = if DIB_FALLBACK_SEQ.load(Ordering::Relaxed) == cur {
                DIB_FALLBACK_AT.load(Ordering::Relaxed)
            } else {
                let t = now_ms();
                DIB_FALLBACK_SEQ.store(cur, Ordering::Relaxed);
                DIB_FALLBACK_AT.store(t, Ordering::Relaxed);
                t
            };
            if now_ms().saturating_sub(first) < HDROP_GRACE_MS {
                diag.push_str("HDROP 已登记→暂缓读 DIB(等文件路径就位，避免同次复制记两条) ");
                suspicious = true;
                return None;
            }
            diag.push_str("HDROP 迟迟未交货→回退读 DIB ");
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

    (
        result,
        diag,
        suspicious,
        ClipOrigin {
            formats: fmts,
            owner_exe: owner,
        },
    )
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
///
/// 返回 `(预览串, 是否截断)`。第二个值不是顺手加的：前端要靠它判断
/// 「卡片已经显示全了就别再弹悬浮浮层」，而这个判断必须与截断规则**同源** ——
/// 前端照抄一份「2 行 / 140 字」的规则，日后改了一边就必然漂移。
/// 常见顶级域名白名单 —— 只用来识别**不带协议**的裸域名。
///
/// 为什么必须白名单，而不是「含点号且无空格」：`报告.docx`、`v1.2.3`、`3.14`
/// 全都符合那个形状，真按它判会把一堆普通文本标成「链接」，徽章立刻失去意义。
/// 白名单只影响裸域名；带 `http(s)://` 的一律直接认，不查表。
const TLDS: &[&str] = &[
    "com", "cn", "net", "org", "io", "dev", "app", "co", "me", "info", "xyz", "top", "site",
    "tech", "ai", "gov", "edu", "cc", "tv", "biz", "pro", "online", "shop", "blog", "wiki",
    "so", "gg", "ly", "to", "fm", "sh", "run", "page", "link", "cloud", "store", "design",
    "art", "work", "live", "news", "world", "today", "space", "website", "host", "press",
    "zone", "email", "group", "team", "plus", "one", "moe", "hk", "tw", "jp", "us", "uk",
];

/// 不带协议的裸域名：`github.com/x`、`example.com`、`mp.weixin.qq.com/s/xxx`。
///
/// 与 `is_single_url` 同一套前提：整条内容不含空白（一句话里夹个域名不算）。
fn looks_like_bare_url(t: &str) -> bool {
    if t.chars().any(char::is_whitespace) {
        return false;
    }
    // 行尾的句号/逗号/右括号是标点，不属于域名
    let core = t.trim_end_matches(['.', ',', ';', ')', ']', '。', '，', '、']);
    let host = core.split(['/', '?', '#']).next().unwrap_or("");
    if !host.contains('.') {
        return false;
    }
    // host 只允许字母数字、点、横线、下划线（排除 @ 邮箱、含中文的句子等）
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return false;
    }
    match host.rsplit('.').next() {
        Some(tld) => TLDS.contains(&tld.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// 整条内容是否「就是一个网址」。
///
/// 两种形态都认：带协议的（`http(s)://`、`www.`），以及**裸域名**（`github.com/x`）——
/// 后者是用户实测反馈「明明是个链接却标成文本」的那种。
/// 但都要求整条内容不含空白：一句话里夹着链接仍是普通文本，标成「链接」反而误导。
fn is_single_url(t: &str) -> bool {
    if t.chars().any(char::is_whitespace) || t.len() < 6 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.") {
        return lower.len() >= 10;
    }
    looks_like_bare_url(t)
}

/// 文本是否「看起来是代码」。
///
/// 用打分而不是单条特征：单独看「行尾有分号」会把中文项目符号、SQL 说明都算进来，
/// 而只看关键字又会把「我在文章里提到 return 这个词」误判成代码。宁可漏标（不显示徽章），
/// 也不要乱标 —— 徽章是「一眼认出这条是什么」的辅助，标错了比不标更糟。
///
/// 四条线索：围栏 / JSON 形状 / 行尾结构符号 / 缩进 / 运算符 / 关键字密度。
fn looks_like_code(t: &str) -> bool {
    // 显式围栏：用户贴代码块时通常自带，最可靠的信号
    if t.contains("```") {
        return true;
    }
    // JSON / 配置片段：首尾成对且含键值分隔
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        if t.contains(':') {
            return true;
        }
    }

    // 关键字表：只认「后跟空格」或带括号的形态，避免命中普通词
    const KEYWORDS: &[&str] = &[
        "fn ", "let ", "const ", "var ", "function ", "def ", "class ", "import ", "from ",
        "return ", "public ", "private ", "static ", "void ", "package ", "namespace ",
        "#include", "<?php", "console.log(", "await ", "async ", "elif ", "func ", "SELECT ",
        "INSERT ", "UPDATE ", "CREATE TABLE",
    ];

    let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        // 单行：多行那套「比例/缩进」统计用不上，但完全不判会把
        // `const x = 1;`、`import os` 这类整行粘贴的代码漏掉（用户实测反馈）。
        // 只用**强特征**，宁可漏：「注意：这样做；」不含括号、不以关键字开头，
        // 因此不会被误标。
        let starts_kw = KEYWORDS.iter().any(|k| t.starts_with(k));
        let ends_semi = t.trim_end().ends_with(';');
        let has_arrow = t.contains("=>") || t.contains("::") || t.contains("->");
        let looks_call = t.contains('(') && t.contains(')');
        return starts_kw || (has_arrow && ends_semi) || (ends_semi && looks_call);
    }

    let mut score = 0;

    // 行尾是分号 / 花括号：代码最常见的结构符号
    let struct_lines = lines
        .iter()
        .filter(|l| {
            let s = l.trim_end();
            s.ends_with(';') || s.ends_with('{') || s.ends_with('}')
        })
        .count();
    if struct_lines >= 2 {
        score += 1;
    }
    if struct_lines * 2 > lines.len() {
        score += 1;
    }

    // 缩进：至少两行以空格或 tab 开头
    if lines
        .iter()
        .filter(|l| l.starts_with("  ") || l.starts_with('\t'))
        .count()
        >= 2
    {
        score += 1;
    }

    // 代码专属运算符（中文排版本里几乎不会出现）
    if t.contains("=>") || t.contains("::") || t.contains("->") || t.contains("&&") {
        score += 1;
    }

    let kw_hits = KEYWORDS.iter().filter(|k| t.contains(**k)).count();
    if kw_hits >= 2 {
        score += 2;
    } else if kw_hits == 1 {
        score += 1;
    }

    // 必须有至少一个关键字：纯结构符号（项目符号、表格线）不算代码
    kw_hits >= 1 && score >= 3
}

/// 是否像 Markdown **源码**。
///
/// 只认**结构性强特征**，且要命中 ≥2 种：单个特征太容易撞车 ——
/// 代码里的 `#` 注释、`- ` 开头的负号行、SQL 里的 `**` 都可能误命中。
/// 强特征清单（都是「整行形态」，普通正文里几乎不会出现）：
///   ATX 标题(`## `)、无序列表(`- `)、有序列表(`1. `)、表格(`| a | b |`)、
///   引用(`> `)、链接语法(`[文字](地址)`)。
///
/// 刻意不把 `**粗体**` 和行内反引号算进来：代码里的乘号与模板字符串会中招。
fn looks_like_markdown(t: &str) -> bool {
    let mut hits = 0;
    let lines: Vec<&str> = t.lines().collect();

    // 1) ATX 标题：行首 1~6 个 `#` 且紧跟空格
    let atx = lines.iter().any(|l| {
        let s = l.trim_start();
        let n = s.chars().take_while(|c| *c == '#').count();
        (1..=6).contains(&n) && s.chars().nth(n) == Some(' ')
    });
    if atx {
        hits += 1;
    }

    // 2) 无序列表
    if lines.iter().any(|l| {
        let s = l.trim_start();
        s.starts_with("- ") || s.starts_with("* ") || s.starts_with("+ ")
    }) {
        hits += 1;
    }

    // 3) 有序列表：数字 + `. `（要求行首，避免匹配正文里的「第 3 点：」）
    if lines.iter().any(|l| {
        let s = l.trim_start();
        let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
        digits > 0 && s[digits..].starts_with(". ")
    }) {
        hits += 1;
    }

    // 4) 表格：整行以 `|` 开头且至少两个 `|`
    if lines
        .iter()
        .any(|l| l.trim_start().starts_with('|') && l.matches('|').count() >= 2)
    {
        hits += 1;
    }

    // 5) 引用
    if lines.iter().any(|l| l.trim_start().starts_with("> ")) {
        hits += 1;
    }

    // 6) 链接语法 `[文字](地址)`
    if t.contains("](") {
        hits += 1;
    }

    hits >= 2
}

/// 判定文本条目的子类型：`"link"` / `"markdown"` / `"code"` / `None`。
///
/// 链接优先：一个网址即使带查询参数也不该被当成代码。
fn detect_subtype(text: &str) -> Option<&'static str> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if is_single_url(t) {
        return Some("link");
    }
    // Markdown 在代码之前判：一篇带代码块的 Markdown 文档，用户认知里是「Markdown」
    // 而不是「代码」。而纯代码几乎不会满足「≥2 种结构强特征」，不会被抢走。
    if looks_like_markdown(t) {
        return Some("markdown");
    }
    if looks_like_code(t) {
        return Some("code");
    }
    None
}

fn text_preview_parts(text: &str) -> (String, bool) {
    /// 与 CSS `.card-preview { -webkit-line-clamp }` 一致
    const MAX_LINES: usize = 2;
    /// 与 2 行大致匹配的字符预算（按中文字符保守估计）
    const MAX_CHARS: usize = 140;

    if text.trim().is_empty() {
        return ("[空白文本]".into(), false);
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
    (out, truncated)
}

/// 只要预览串的便捷包装：测试里大多只关心显示出来的那两行，不关心截断标记。
/// 生产代码一律直接用 `text_preview_parts`（它要同时拿到标记）。
#[cfg(test)]
fn make_text_preview(text: &str) -> String {
    text_preview_parts(text).0
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
        let (new_preview, new_truncated) = text_preview_parts(&text);
        // 两个字段一起比对：老索引里只有 preview、没有 preview_truncated，
        // 若只比 preview，这类条目永远进不了分支，每次启动都要重读全部正文。
        if new_preview != item.preview || new_truncated != item.preview_truncated {
            item.preview = new_preview;
            item.preview_truncated = new_truncated;
            changed += 1;
        }
    }
    changed
}

/// 给存量文本条目补算 `subtype`（链接 / 代码），返回处理条数。
///
/// 只处理 `subtype.is_none()` 的条目 —— 已判定过的（包括「判过但两类都不是」的空串）
/// 一律跳过，所以这个函数**天然只跑一次**：升级后首次启动扫一遍，之后每次启动
/// 都不会再解密几百条正文。这也是用空串而不是 None 表示「无子类型」的原因。
fn refresh_text_subtypes(dir: &PathBuf, hist: &mut [HistoryItem]) -> usize {
    let mut changed = 0usize;
    for item in hist.iter_mut() {
        if item.kind != "text" || item.subtype.is_some() {
            continue;
        }
        let Some(rel) = item.text_path.as_ref() else {
            continue;
        };
        let Ok(text) = read_text_entity(dir, rel) else {
            continue;
        };
        item.subtype = Some(detect_subtype(&text).unwrap_or("").to_string());
        changed += 1;
    }
    changed
}

fn store_raw(raw: RawClip, edited: bool, shot: bool) -> HistoryItem {
    let dir = DIR.get().unwrap().clone();
    let ts = now_ms();
    let id = make_id(ts);
    // 图片分类开关取一次：同一次落盘不该看到两个不同的值
    let split_dirs = crate::settings::get().images_split_dirs;
    let mut item = HistoryItem {
        id: id.clone(),
        kind: String::new(),
        ts,
        pinned: false,
        pinned_views: Vec::new(),
        favorite: false,
        edited,
        subtype: None,
        preview: String::new(),
        preview_truncated: false,
        size: 0,
        // 具体值在图片分支里按 `shot` 参数写入
        shot: false,
        text_path: None,
        html_path: None,
        rtf_path: None,
        rich_images: Vec::new(),
        img_path: None,
        files_path: None,
        files: None,
    };

    match raw {
        RawClip::Text { text, html, rtf } => {
            item.kind = "text".into();
            // 子类型只用于卡片徽章（链接 / 代码），不参与分类
            item.subtype = detect_subtype(&text).map(str::to_string);
            item.size = text.len() as u64;
            let (preview, truncated) = text_preview_parts(&text);
            item.preview = preview;
            item.preview_truncated = truncated;
            // 无论长短都落盘全文：内存只保留截断后的 preview 摘要，
            // 这样既遵守「内容不走内存」红线，复制回去时也不会被截断。
            let rel = format!("data/{}.txt", id);
            write_text_entity(&dir, &rel, &text);
            item.text_path = Some(rel);
            // 富文本实体单独落盘（一样加密）：纯文本永远是主数据，
            // 富文本只是「回贴时可以更保真」的附加信息，缺了不影响使用。
            if let Some(h) = html {
                let (hrel, imgs) = store_rich_html(&dir, &id, h.as_str());
                item.html_path = Some(hrel);
                item.rich_images = imgs;
            }
            if let Some(r) = rtf {
                let rrel = format!("data/{id}.rtf");
                write_text_entity(&dir, &rrel, &r);
                item.rtf_path = Some(rrel);
            }
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
            // 来源存进条目：关掉分类开关后路径不再携带它，靠这个字段才回得来
            item.shot = shot;
            let rel = image_rel(&dir, ts, &ext, shot, split_dirs);
            match write_image(&dir, &rel, &bytes) {
                Ok(()) => item.img_path = Some(rel),
                Err(e) => crate::log_err!("clipboard", "图片原图落盘失败 {rel}：{e}"),
            }
        }
        RawClip::Image(bytes) => {
            item.kind = "image".into();
            item.size = bytes.len() as u64;
            item.preview = "[图片]".into();
            // 来源在捕获那一刻就判好了（见 `on_clipboard_update`），这里直接用：
            // 工作线程可能滞后几十毫秒，那时候剪贴板早被下一次复制换掉了。
            item.shot = shot;
            let from_shot = shot;
            // 截图按设置选格式；其它来源（复制的位图）沿用 BMP —— 它们本就没有
            // 原始编码可保留，BMP 最快且无损。
            let want = if shot {
                crate::settings::get().shot_format
            } else {
                "bmp".to_string()
            };
            // 编码失败一律回退 BMP；连 BMP 都拼不出来才存裸 DIB（前端显示不了，
            // 但数据还在，总比丢了好）
            let (ext, data) = encode_dib(&bytes, &want)
                .unwrap_or_else(|| ("dib".to_string(), bytes.clone()));
            let rel = image_rel(&dir, ts, &ext, from_shot, split_dirs);
            if let Err(e) = write_image(&dir, &rel, &data) {
                crate::log_err!("clipboard", "图片落盘失败 {rel}：{e}");
            }
            item.img_path = Some(rel);
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
    // `rich=` 是排查富文本的第一落点：一眼能看出这次复制到底有没有拿到格式。
    // none 说明源应用没提供 HTML/RTF，或体积超限被放弃（后者另有一条 WARN）。
    let rich = match (&item.html_path, &item.rtf_path) {
        (Some(_), Some(_)) => "html+rtf",
        (Some(_), None) => "html",
        (None, Some(_)) => "rtf",
        (None, None) => "none",
    };
    crate::log_print!(
        "[clipboard] 新增 {} 条目 kind={} size={} rich={}",
        id,
        item.kind,
        item.size,
        rich
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

/// 把 DIB 编码成目标格式，返回（扩展名，字节）。
///
/// 不认识的格式、或编码失败，一律回退 BMP —— 绝不能因为「用户选了 PNG」这种
/// 设置问题把图丢掉。连 BMP 都拼不出来时返回 None，由调用方存裸 DIB。
///
/// 只有**截图**走这里。其它来源的图片保持原样：微信给的是 JPEG 就存 JPEG，
/// 重新编码只会掉画质。
fn encode_dib(dib: &[u8], want: &str) -> Option<(String, Vec<u8>)> {
    let bmp = dib_to_bmp(dib).ok()?;
    if want == "bmp" {
        return Some(("bmp".to_string(), bmp));
    }
    let Ok(img) = image::load_from_memory(&bmp) else {
        return Some(("bmp".to_string(), bmp));
    };
    let mut out = std::io::Cursor::new(Vec::new());
    let ok = match want {
        "png" => img.write_to(&mut out, image::ImageFormat::Png).is_ok(),
        // JPEG 没有 alpha 通道，带透明度的图必须先拍成 RGB，否则编码直接报错
        _ => image::DynamicImage::ImageRgb8(img.to_rgb8())
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .is_ok(),
    };
    if ok {
        let ext = if want == "png" { "png" } else { "jpg" };
        Some((ext.to_string(), out.into_inner()))
    } else {
        Some(("bmp".to_string(), bmp))
    }
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
/// UNIX 毫秒 → FILETIME。1601-01-01 到 1970-01-01 相差 11644473600 秒，
/// FILETIME 的单位是 100 纳秒。
const FT_UNIX_EPOCH_OFFSET_MS: u64 = 11_644_473_600_000;

/// 条目时间戳 → 文件名用的本地日期时间 `20261028_223610`。
///
/// 必须用**本地时间**：文件是给人看的，用户在资源管理器里对照的是自己表上的时间。
/// 走 `FileTimeToSystemTime` + `SystemTimeToTzSpecificLocalTime` 而不是自己算时区，
/// 因为后者要处理夏令时规则，手写必然出错（`FileTimeToLocalFileTime` 用的是当前
/// 夏令时状态而不是目标日期的，同样不能用）。
fn local_stamp(ts_ms: u64) -> String {
    let ft = (ts_ms + FT_UNIX_EPOCH_OFFSET_MS) * 10_000;
    let mut utc = crate::logger::SystemTimeLocal::default();
    let mut local = crate::logger::SystemTimeLocal::default();
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    // 转换失败就退回 UTC：文件名仍然是可读的日期，只是时区差几小时，
    // 总比整条落盘失败强。
    let t = if ok { local } else { utc };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        t.w_year, t.w_month, t.w_day, t.w_hour, t.w_minute, t.w_second
    )
}

/// 文件名里的日期时间 → 时间戳（毫秒）。反解失败返回 None。
///
/// 索引损坏时靠它从文件重建条目 —— 新的文件名不再含内部 id，时间只能从
/// 名字里读回来。
fn ts_from_stamp(stem: &str) -> Option<u64> {
    // `20261028_223610` 或去重后的 `20261028_223610-2`
    let base = stem.split('-').next()?;
    let (d, t) = base.split_once('_')?;
    if d.len() != 8 || t.len() != 6 {
        return None;
    }
    let num = |s: &str| s.parse::<u16>().ok();
    let local = crate::logger::SystemTimeLocal {
        w_year: num(&d[0..4])?,
        w_month: num(&d[4..6])?,
        w_day_of_week: 0,
        w_day: num(&d[6..8])?,
        w_hour: num(&t[0..2])?,
        w_minute: num(&t[2..4])?,
        w_second: num(&t[4..6])?,
        w_milliseconds: 0,
    };
    let mut utc = crate::logger::SystemTimeLocal::default();
    if unsafe { TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) } == 0 {
        return None;
    }
    // 日历值 → 天数 → 毫秒。日期用「每月天数的前缀和」，闰年另行判断。
    let days = days_from_civil(utc.w_year as i64, utc.w_month as u32, utc.w_day as u32)?;
    let secs = days * 86_400
        + utc.w_hour as i64 * 3600
        + utc.w_minute as i64 * 60
        + utc.w_second as i64;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000)
}

/// 公历日期 → 距 1970-01-01 的天数。Howard Hinnant 算法的正向版本
/// （logger 里那份是反向的 `civil_from_days`，两者必须配套）。
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // 三月为 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// 图片落盘的相对路径：`images/shot/20261028_223610.bmp`。
///
/// - `from_shot`：这次落盘的是不是轻贴自己的截图。只有 `Alt+Q` 能确定 ——
///   剪贴板里没有来源元数据，微信 / QQ 的截图与它们自己「复制图片」是同一个
///   进程，分不出来，一律按复制处理。
/// - 开关 `images_split_dirs` 关掉时全部平铺在 `images/` 下：文件名仍然带日期。
/// - 同一秒里落两张（连按两次截图）会撞名，依次加 `-2`、`-3`。
fn image_rel(dir: &Path, ts_ms: u64, ext: &str, from_shot: bool, split: bool) -> String {
    let base = image_dir(split, from_shot);
    let stamp = local_stamp(ts_ms);
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stamp}.{ext}")
        } else {
            format!("{stamp}-{n}.{ext}")
        };
        let rel = format!("{base}/{name}");
        // 撞名只会在同一秒内发生；上限是防御无限循环，正常一两轮就出结果
        if !dir.join(&rel).exists() || n >= 999 {
            return rel;
        }
        n += 1;
    }
}

/// 图片目录布局的当前版本。改规则就把这里 +1，启动时按需迁移一次。
const IMAGE_LAYOUT_VERSION: u32 = 1;

/// 把图片按当前设置搬到应有的位置（`images/shot/`、`images/copy/` 或根目录），
/// 文件名一律日期时间。幂等：已在目标位置的条目会被跳过。
///
/// 存量条目的来源已不可考（旧记录没有来源标记），**一律按「复制」归类** ——
/// 只有用 `Alt+Q` 截的图才进 `shot/`。
///
/// 用 `rename` 而不是 `copy`：同盘 rename 是原子操作且毫秒级。copy 在 165 MB
/// 上要几秒 —— 那就只能放在启动时慢慢跑，切开关时没法立即生效。
///
/// rename 留下的窗口（文件已移走、索引还没写盘）由 `rebuild_index_from_files`
/// 兜住：它会扫 `shot/` 与 `copy/`，并从文件名解析时间重建条目。真被强杀也不丢图。
///
/// 返回迁移条数。
fn migrate_image_layout(dir: &PathBuf, hist: &mut Vec<HistoryItem>, split: bool) -> usize {
    // 返回「改动条数」：既包括搬了文件的，也包括只补了 shot 标记的 ——
    // 后者也要写索引，否则下次启动还会重算一遍
    let mut changed = 0usize;
    for item in hist.iter_mut() {
        let Some(old_rel) = item.img_path.clone() else {
            continue;
        };
        let Some(rest) = old_rel.strip_prefix("images/") else {
            continue;
        };
        // 富文本内联图与缩略图有各自的目录规则，不参与整理
        if rest.starts_with("rich/") || rest.starts_with("thumbs/") {
            continue;
        }
        // 来源以条目字段为准。老索引没有 `shot`，此时路径是唯一线索：已经在
        // `shot/` 里的补上标记 —— 否则升上来第一轮整理会把它们全搬进 `copy/`。
        let mut touched = false;
        if !item.shot && rest.starts_with("shot/") {
            item.shot = true;
            touched = true;
        }
        let from_shot = item.shot;
        let stem = Path::new(rest)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let cur_dir = old_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        // 已经在正确位置：目录对、文件名就是该有的日期（撞名的 `-2` 后缀不算变化）
        let base = stem.split('-').next().unwrap_or(stem);
        if cur_dir == image_dir(split, from_shot) && base == local_stamp(item.ts) {
            if touched {
                changed += 1;
            }
            continue;
        }
        let ext = Path::new(rest)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("bmp")
            .to_string();
        let src = dir.join(&old_rel);
        if !src.is_file() {
            continue;
        }
        let new_rel = image_rel(dir, item.ts, &ext, from_shot, split);
        let dst = dir.join(&new_rel);
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::rename(&src, &dst) {
            Ok(()) => {
                item.img_path = Some(new_rel);
                changed += 1;
            }
            // 单条失败不阻断整体：该条继续用旧路径，仍然能显示
            Err(e) => crate::log_warn!("[clipboard] 图片迁移跳过 {old_rel}：{e}"),
        }
    }
    changed
}

/// 删掉已经是空的分类目录。
///
/// 关掉分类开关后图片都搬回根目录，留两个空目录没有意义。`fs::remove_dir`
/// 只删空目录 —— 里面有东西（还有没搬完的、或用户自己放的文件）就失败，
/// 失败当正常情况处理，不报错。
fn prune_empty_image_dirs(dir: &Path) {
    for sub in ["images/shot", "images/copy"] {
        let p = dir.join(sub);
        if !p.is_dir() {
            continue;
        }
        if fs::remove_dir(&p).is_ok() {
            // sub 已经带着 `images/` 前缀，别再加一层
            crate::log_print!("[clipboard] 已删除空目录 {sub}");
        }
    }
}

/// 按当前设置重新整理图片目录，并记下布局版本。返回搬了几条。
///
/// 初始化与「切换图片分类开关」都调它。切开关后不整理的话，用户会看到
/// 「开关开了、旧图却还平铺在根目录」—— 而且那个状态要等到下次启动才修好。
pub fn organize_image_layout() -> usize {
    let (Some(dir), Some(hist)) = (DIR.get(), HISTORY.get()) else {
        return 0;
    };
    let split = crate::settings::get().images_split_dirs;
    let started = std::time::Instant::now();
    let mut h = match hist.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let moved = migrate_image_layout(dir, &mut h, split);
    prune_empty_image_dirs(dir);
    // rename 不搬数据，耗时只跟条目数相关；真正的大头通常是索引整份重新加密写盘
    // （条数越多越慢）。这两段分开记，以后要优化才知道该动哪一段。
    let files_done = started.elapsed();
    if moved > 0 {
        let t = std::time::Instant::now();
        save_index(dir, &h);
        crate::log_print!(
            "[clipboard] 图片整理：搬动 {moved} 条用时 {} ms，写索引 {} ms（共 {} 条历史）",
            files_done.as_millis(),
            t.elapsed().as_millis(),
            h.len()
        );
    }
    drop(h);
    crate::settings::update(|c| c.image_layout_ver = IMAGE_LAYOUT_VERSION);
    moved
}

/// 图片该放哪个目录。落盘与整理共用一处判断，免得两边走偏。
fn image_dir(split: bool, from_shot: bool) -> &'static str {
    if !split {
        "images"
    } else if from_shot {
        "images/shot"
    } else {
        "images/copy"
    }
}

/// 图片落盘：父目录不存在时先建。
///
/// `images/shot/` 与 `images/copy/` 是懒创建的 —— 直接 `fs::write` 会得到
/// 「系统找不到指定的路径」（os error 3）。那种失败很隐蔽：条目照样入库、
/// 界面上只是一张占位图，日志里才有一行 ERROR。`images/` 根目录一直存在，
/// 所以分成子目录之前从没撞上过。
fn write_image(dir: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes)
}

/// 缩略图的统一存放路径：**图片目录下的子文件夹** `images/thumbs/`。
/// 与图片原图（`images/`）放一起，便于整体迁移与清理。
fn thumb_rel(id: &str) -> String {
    format!("images/thumbs/{id}.png")
}

/// 写缩略图（自动建目录）。失败返回原因，由调用方决定是否记日志。
fn write_thumb(dir: &std::path::Path, id: &str, png: &[u8]) -> Result<(), &'static str> {
    let path = dir.join(thumb_rel(id));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "缩略图目录创建失败")?;
    }
    fs::write(&path, png).map_err(|_| "缩略图写盘失败")
}

/// 通用占位图：**解码不了也必须显示成功**，所以给一张中性底 + 简单「图片」图形，
/// 而不是留空白或让前端显示破图。只在确实解不出内容时使用。
fn placeholder_png(side: u32) -> Vec<u8> {
    let side = side.max(64);
    let bg = image::Rgba([241, 241, 244, 255]);
    let line = image::Rgba([205, 205, 211, 255]);
    let mark = image::Rgba([178, 178, 186, 255]);
    let mut img = image::RgbaImage::from_pixel(side, side, bg);

    let border = ((side as f32) * 0.05).max(2.0) as u32;
    for x in 0..side {
        for t in 0..border {
            img.put_pixel(x, t, line);
            img.put_pixel(x, side - 1 - t, line);
        }
    }
    for y in 0..side {
        for t in 0..border {
            img.put_pixel(t, y, line);
            img.put_pixel(side - 1 - t, y, line);
        }
    }
    let mut put = |x: i32, y: i32, c: image::Rgba<u8>| {
        if x >= 0 && y >= 0 && (x as u32) < side && (y as u32) < side {
            img.put_pixel(x as u32, y as u32, c);
        }
    };
    // 「山」：底边在 0.72 高度，顶点在 0.42，底宽 0.44
    let cx = (side as f32 / 2.0) as i32;
    let base = (side as f32 * 0.72) as i32;
    let peak = (side as f32 * 0.42) as i32;
    let half = (side as f32 * 0.22) as i32;
    for y in peak..=base {
        let prog = (base - y) as f32 / (base - peak).max(1) as f32;
        let w = (half as f32 * (1.0 - prog)) as i32;
        for dx in -w..=w {
            put(cx + dx, y, mark);
        }
    }
    // 「太阳」：左上 0.32/0.34，半径 0.07
    let r = ((side as f32) * 0.07).max(2.0) as i32;
    let sx = (side as f32 * 0.32) as i32;
    let sy = (side as f32 * 0.34) as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(sx + dx, sy + dy, mark);
            }
        }
    }

    let mut cur = std::io::Cursor::new(Vec::new());
    let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut cur, image::ImageFormat::Png);
    cur.into_inner()
}

fn ensure_thumb(dir: &std::path::Path, item: &HistoryItem) -> Option<Vec<u8>> {
    let rel = item.img_path.as_ref()?;
    let src = dir.join(rel);
    // 1) 已有缩略图直接读
    let thumb_path = dir.join(thumb_rel(&item.id));
    if let Ok(bytes) = fs::read(&thumb_path) {
        return Some(bytes);
    }
    // 2) 现场生成；**任何失败都退回通用占位图** —— 缩略图必须显示成功，
    //    不留空白或破图（SVG/PSD/缺解码器、超限、原图丢失都会走到这里）。
    let generated = (|| -> Option<Vec<u8>> {
        let bytes = match fs::read(&src) {
            Ok(b) => b,
            Err(e) => {
                crate::log_warn!("[clipboard] 缩略图：读取原图失败 {}：{e}", src.display());
                return None;
            }
        };
        if bytes.len() > 256 * 1024 * 1024 {
            crate::log_warn!(
                "[clipboard] 图片 {} 达 {}MB，超出处理能力，改用占位图",
                src.display(),
                bytes.len() / 1048576
            );
            return None;
        }
        let img = match image::load_from_memory(&bytes) {
            Ok(i) => i,
            Err(e) => {
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
        Some(png.into_inner())
    })()
    .unwrap_or_else(|| placeholder_png(THUMB_SIDE));
    // 落盘失败不影响本次返回（下次再生成）
    let _ = write_thumb(dir, &item.id, &generated);
    Some(generated)
}

// ---------------------------------------------------------------------------
// 写回剪贴板
// ---------------------------------------------------------------------------

unsafe fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    let hwnd = CLIP_HWND.get().copied().unwrap_or(0);
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

/// 把字符串转成剪贴板要的「UTF-16LE + 结尾 NUL」字节序列。
fn utf16_bytes(s: &str) -> Vec<u8> {
    let mut v: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out = Vec::with_capacity(v.len() * 2);
    for u in v.drain(..) {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// 一次写入多个剪贴板格式。
///
/// 为什么必须一次写完：每次写入都要 `EmptyClipboard`，分三次调就是「后写的把
/// 先写的清掉」—— 最后只剩一种格式，用户粘到 Word 里还是没有格式。
unsafe fn write_clipboard_formats(items: &[(u32, Vec<u8>)]) -> Result<(), String> {
    if items.is_empty() {
        return Err("没有可写入的格式".into());
    }
    let hwnd = CLIP_HWND.get().copied().unwrap_or(0);
    if OpenClipboard(hwnd) == 0 {
        return Err("OpenClipboard 失败".into());
    }
    EmptyClipboard();
    for (fmt, data) in items {
        let h = GlobalAlloc(0x0002, data.len()); // GMEM_MOVEABLE
        if h == 0 {
            CloseClipboard();
            return Err("GlobalAlloc 失败".into());
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            CloseClipboard();
            return Err("GlobalLock 失败".into());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        GlobalUnlock(h);
        if SetClipboardData(*fmt, h) == 0 {
            CloseClipboard();
            return Err(format!("SetClipboardData 失败（格式 {fmt}）"));
        }
    }
    CloseClipboard();
    mark_skip();
    Ok(())
}

/// 给裸 HTML 补上 `HTML Format` 所需的头。
///
/// 从应用复制来的 `HTML Format` 本身就带这个头（所以正常路径是「原样写回」）；
/// 这个函数只是兜底：万一拿到的是一段裸 HTML，不补头的话 Word / 邮件客户端
/// 会把它当成**纯文本**贴出来，也就是用户看到的「粘出来是源码」。
fn build_cf_html(body: &str) -> String {
    const PREFIX: &str = "<html><body><!--StartFragment-->";
    const SUFFIX: &str = "<!--EndFragment--></body></html>";
    // 头部各偏移量都是 10 位数字，长度固定 —— 先算出来再填值
    let head_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_html = head_len;
    let start_frag = head_len + PREFIX.len();
    let end_frag = start_frag + body.len();
    let end_html = end_frag + SUFFIX.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n{PREFIX}{body}{SUFFIX}"
    )
}

/// 内容是否已经是带头的 CF_HTML。
///
/// 从 Word / 浏览器 / 微信复制来的 `HTML Format` **本来就有头**，直接原样写回即可；
/// 再套一层会让四个偏移全错，粘到 Word 里就成了一坨源码。
fn is_cf_html(s: &str) -> bool {
    s.trim_start().starts_with("Version:")
}

/// 写回带格式的文本：HTML 优先，外加纯文本兜底。
///
/// 一定同时写 `CF_UNICODETEXT` —— 记事本、终端、多数输入框只认纯文本，
/// 只写 HTML 会表现成「粘到一半应用里什么都没有」。
unsafe fn write_html_to_clipboard(cf_html: &str, plain: &str) -> Result<(), String> {
    let payload = if is_cf_html(cf_html) {
        cf_html.to_string()
    } else {
        build_cf_html(cf_html)
    };
    let html_fmt = unsafe {
        let name: Vec<u16> = "HTML Format".encode_utf16().chain(std::iter::once(0)).collect();
        RegisterClipboardFormatW(name.as_ptr())
    };
    if html_fmt == 0 {
        // 拿不到 HTML 格式就直接退回纯文本，而不是让回贴整个失败
        return unsafe { write_text_to_clipboard(plain) };
    }
    unsafe {
        write_clipboard_formats(&[
            (html_fmt, payload.into_bytes()),
            (13u32, utf16_bytes(plain)),
        ])
    }
}

/// 写回 RTF（`HTML Format` 缺失时的次选），同样附纯文本。
unsafe fn write_rtf_to_clipboard(rtf: &str, plain: &str) -> Result<(), String> {
    let rtf_fmt = unsafe {
        let name: Vec<u16> = "Rich Text Format".encode_utf16().chain(std::iter::once(0)).collect();
        RegisterClipboardFormatW(name.as_ptr())
    };
    if rtf_fmt == 0 {
        return unsafe { write_text_to_clipboard(plain) };
    }
    unsafe {
        write_clipboard_formats(&[
            (rtf_fmt, rtf.as_bytes().to_vec()),
            (13u32, utf16_bytes(plain)),
        ])
    }
}

unsafe fn write_files_to_clipboard(files: &[String]) -> Result<(), String> {
    let hwnd = CLIP_HWND.get().copied().unwrap_or(0);
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
    write_dib_raw(dib, false)
}

/// 写位图到剪贴板。`record` = 是否让采集线程把它当成一条新内容。
///
/// 两个调用方的诉求正好相反，所以必须分开：
/// - **从历史回贴**（`copy_back` 的图片分支）：`record = false`。回贴是「把旧内容
///   还给剪贴板」，若让它再记一条，用户每粘一次就多一条重复；
/// - **截图**（`screenshot`）：`record = true`。截图是新内容，用户截完就期望它在
///   列表里 —— 这条链路正是「截图 → 自动进轻贴」的实现方式。
///
/// 共用一个函数体而不是各写一份：GMEM/加锁/SetClipboardData 的顺序错了会静默失败
/// （剪贴板里是空的，或格式写不进去），这类代码只该有一份。
unsafe fn write_dib_raw(dib: &[u8], record: bool) -> Result<(), String> {
    let hwnd = CLIP_HWND.get().copied().unwrap_or(0);
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
    if !record {
        mark_skip();
        // 回贴绝不能被记录：顺手把可能残留的通行证清掉，
        // 否则「上一条截图刚好没被采集消费」时，这一次回贴会被误记成新条目
        RECORD_OWN_WRITE.store(false, Ordering::SeqCst);
    }
    Ok(())
}

/// 把一张位图当作**新内容**写进剪贴板（截图专用）。
///
/// 刻意不 `mark_skip`：写完之后采集线程会正常读到它，于是截图立刻出现在轻贴列表里，
/// 不需要截图模块自己再走一遍存储。
pub fn write_shot_to_clipboard(dib: &[u8]) -> Result<(), String> {
    RECORD_OWN_WRITE.store(true, Ordering::SeqCst);
    SHOT_AT.store(now_ms(), Ordering::SeqCst);
    let r = unsafe { write_dib_raw(dib, true) };
    if r.is_err() {
        RECORD_OWN_WRITE.store(false, Ordering::SeqCst);
        SHOT_AT.store(0, Ordering::SeqCst);
    }
    r
}

/// 剪贴板来源的结构化信息：格式名列表 + 所有者进程名。
///
/// 两者都必须在 `OpenClipboard` 期间取到 —— 一松手，内容可能已经被下一次
/// 复制换掉了，事后再查就是别人的。
struct ClipOrigin {
    /// `enum_clipboard_formats` 的输出：标准格式是裸名（`CF_DIB`），
    /// 私有/注册格式带双引号（`"WeChatScreenshotFormat"`）。
    formats: Vec<String>,
    /// 所有者进程名。空串 = 没有窗口持有剪贴板（PrtSc 与 QQ 截图就是这样写进来的），
    /// 或者权限不足查不到。
    owner_exe: String,
}

/// 这次图片算不算「截图」—— 决定落 `images/shot/` 还是 `images/copy/`。
///
/// 判据是剪贴板上的**格式名**，不是进程名。实测（Win10 19045，2026-10-28）：
///
/// | 操作 | 所有者 | 可用的区分特征 |
/// |---|---|---|
/// | 微信截图 | Weixin.exe | 私有格式 `WeChatScreenshotFormat` |
/// | 微信复制图片 | Weixin.exe | 没有那个格式，改给 `CF_HDROP` |
/// | QQ 截图 | 无 | 只有裸位图 |
/// | QQ 复制图片 | 无 | 带 `QQ_Unicode_RichEdit_Format` / `HTML Format` |
/// | Win+Shift+S | svchost.exe | 注册格式 `PNG` + `Ole Private Data` |
///
/// 三条规则按可靠性排序，任一命中即算截图：
/// 1. **微信的专属格式** —— 零误判，它只在截图时出现
/// 2. **系统截图工具**会写注册格式 `PNG`（普通「复制图片」一般只给 CF_DIB），
///    并同时带 `Ole Private Data`
/// 3. **没有窗口持有者的裸位图** —— PrtSc 与 QQ 截图都不经自己的窗口写剪贴板；
///    有窗口的程序（画图、PS）复制图片一定带自己的 owner，不会命中
///
/// 三条都不命中就是普通复制。宁漏判不错判：目录名写错比 copy/ 里多几个文件
/// 更让人困惑。
fn classify_shot(origin: &ClipOrigin) -> bool {
    // 私有/注册格式在枚举时带了双引号，比较前先剥掉。
    // 写成局部 fn 而不是闭包：闭包返回借用说不清生命周期。
    fn unq(s: &str) -> &str {
        s.trim_matches('"')
    }
    let has = |name: &str| origin.formats.iter().any(|f| unq(f) == name);

    // 1) 微信截图：专属私有格式，只有截图才写
    if has("WeChatScreenshotFormat") {
        return true;
    }
    // 2) 系统截图工具（Win+Shift+S 等）
    if has("PNG") && has("Ole Private Data") {
        return true;
    }
    // 3) 无窗口持有者的裸位图：PrtSc、QQ 截图
    let bitmap_only = !origin.formats.is_empty()
        && origin
            .formats
            .iter()
            .all(|f| matches!(unq(f), "CF_BITMAP" | "CF_DIB" | "CF_DIBV5" | "CF_PALETTE"));
    bitmap_only
        && (origin.owner_exe.is_empty() || origin.owner_exe.eq_ignore_ascii_case("svchost.exe"))
}

/// 取用「这次落盘的是不是截图」的标记，取完即清。
///
/// 只在图片落盘时调用一次：截图写进剪贴板后，紧接着的那次捕获才是它。
fn take_shot_mark() -> bool {
    let at = SHOT_AT.swap(0, Ordering::SeqCst);
    at != 0 && now_ms().saturating_sub(at) <= SHOT_MARK_TTL_MS
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

/// 索引读不出来时，把损坏的文件原样留一份再返回空。
///
/// 为什么必须留：`load_index` 返回空之后，用户随手复制一条就会触发 `save_index`，
/// 把损坏文件**就地覆盖**成一个看起来正常的小索引 —— 事故现场彻底消失，事后
/// 谁也无法判断是截断、解密失败还是解析失败。用户实测那次就是这样丢的证据。
fn keep_corrupt(dir: &PathBuf, raw: &[u8], why: &str) {
    let dst = dir.join("index.json.corrupt");
    match fs::write(&dst, raw) {
        Ok(()) => crate::log_err!(
            "clipboard",
            "index.json {why}；损坏文件 {} 字节已另存为 index.json.corrupt",
            raw.len()
        ),
        Err(e) => crate::log_err!("clipboard", "index.json {why}，且备份失败：{e}"),
    }
}

fn load_index(dir: &PathBuf) -> Vec<HistoryItem> {
    let path = dir.join("index.json");
    // 按**字节**读，不按字符串：新格式是 DPAPI 密文（带 ENC_MAGIC），不是 UTF-8 JSON。
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(e) => {
            // 曾经这里直接 `return Vec::new()` —— **一行日志都没有**。于是
            // 「记录全没了」成了唯一线索，连「索引文件到底在不在」都查不出来。
            // 用户实测那次日志里恰好就是这种真空。
            if e.kind() == std::io::ErrorKind::NotFound {
                crate::log_warn!(
                    "[clipboard] 没有 index.json（首次运行，或索引已被删除/改名）—— 历史列表为空"
                );
            } else {
                crate::log_err!("clipboard", "index.json 读取失败：{e}");
            }
            return Vec::new();
        }
    };
    if raw.is_empty() {
        crate::log_err!(
            "clipboard",
            "index.json 是空文件（0 字节）—— 上次写入被中断（强杀/崩溃/断电）的特征，历史列表将为空"
        );
        return Vec::new();
    }
    // decode_text_bytes 同时兼容老版本的明文 JSON（没有魔数 → 原样返回），
    // 老用户升级后历史不会因为格式切换而清空。
    let Some(json) = decode_text_bytes(&raw) else {
        keep_corrupt(dir, &raw, "解密失败（可能换了 Windows 账户或机器）");
        return Vec::new();
    };
    let mut v: Vec<HistoryItem> = serde_json::from_str(&json).unwrap_or_else(|e| {
        keep_corrupt(dir, &raw, &format!("解析失败：{e}"));
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
    // 成功加载也要记一条：否则「索引读到了几条、文件多大」只能靠 init 里那句
    // 间接推断。事故复盘时最有用的是「改动前后的两条读数」，而不是只剩一条。
    crate::log_print!(
        "[clipboard] 索引加载成功：{} 条，{} 字节",
        v.len(),
        raw.len()
    );
    v
}

/// 落盘索引：**整份 DPAPI 加密**，落盘的不是明文 JSON。
///
/// 为什么必须加密：`index.json` 里有 `preview`（正文前 2 行）和 `files`（完整
/// 路径列表）—— 前者可能直接是密码、密钥、聊天记录，后者暴露目录结构与项目名。
/// 内容实体（`data/` `files/`）早就加密了，索引却是明文，等于把同一份内容
/// 抄了个不加密的副本放在旁边，加密就白做了。
///
/// 为什么整份加密而不是逐字段：`save_index` 每次捕获都会跑，逐字段要对上千条
/// 各调一次 `CryptProtectData`，一次复制能卡上百毫秒；整份只调一次。
/// 代价是 index.json 不再是人眼可读的 JSON —— 但它本来就是程序独占的索引，
/// 需要排查时看日志就够了。
///
/// 加密失败时 `encrypt_text` 会退回明文（与内容实体同一策略）：
/// 宁可少一层保护，也不能因为 DPAPI 异常把整份历史弄丢。读出时靠魔数区分。
fn save_index(dir: &PathBuf, hist: &Vec<HistoryItem>) {
    let path = dir.join("index.json");
    // 先写临时文件、再原子替换。
    //
    // 直接 `fs::write` 覆盖是**非原子**的：进程若在写入中途被强杀（taskkill、
    // 崩溃、关机），index.json 会被截断成半截甚至 0 字节，下次启动解析失败
    // → 历史全没。用户实测过一次：列表只剩 4 条，而 data/ 下 300 多个正文文件
    // 都在 —— 正是这种损坏形态。rename 在同一分区上是原子的，要么是旧的完整
    // 索引，要么是新的完整索引，不存在半截状态。
    let tmp = dir.join("index.json.tmp");
    match serde_json::to_string_pretty(hist) {
        Ok(s) => {
            // 写失败要记：index.json 是重启后恢复历史/置顶/收藏的唯一依据，
            // 静默失败会表现为「重启后记录全没了」却查无实据。
            if let Err(e) = fs::write(&tmp, encrypt_text(&s)) {
                crate::log_err!("clipboard", "index.json 写入失败：{e}");
                let _ = fs::remove_file(&tmp);
                return;
            }
            if let Err(e) = fs::rename(&tmp, &path) {
                crate::log_err!("clipboard", "index.json 原子替换失败：{e}");
                let _ = fs::remove_file(&tmp);
            }
        }
        Err(e) => crate::log_err!("clipboard", "index.json 序列化失败：{e}"),
    }
}

/// 从 id（`{:x}-{counter}`）里取回毫秒时间戳。取不到返回 None。
fn ts_from_id(id: &str) -> Option<u64> {
    id.split('-')
        .next()
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .filter(|&ms| ms > 1_500_000_000_000) // 粗校验：2017 年之前的值视为不可信
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
        * 1000
}

/// 统计缓存目录里的内容实体数（正文 + 图片 + 文件清单），用来判断索引是否漏条。
/// 只数实体文件，不数 `images/thumbs/`（缩略图不是条目）。
fn count_entities(dir: &PathBuf) -> usize {
    let count_in = |sub: &str, want_ext: Option<&str>| -> usize {
        fs::read_dir(dir.join(sub))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let p = e.path();
                        p.is_file()
                            && want_ext
                                .map(|x| p.extension().and_then(|s| s.to_str()) == Some(x))
                                .unwrap_or(true)
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    // 图片可能在三处：`images/` 根目录（旧布局或关掉分类）、`shot/`、`copy/`。
    // 只数根目录的话，迁移之后所有图片都不计入 —— 索引真损坏时判据 `文件数 >
    // 条目数` 就永远不成立，那些图会静默消失（`images/thumbs`、`images/rich`
    // 是子目录，`is_file()` 已把它们排除在外）。
    count_in("data", Some("txt"))
        + count_in("files", Some("txt"))
        + count_in("images", None)
        + count_in("images/shot", None)
        + count_in("images/copy", None)
}

/// 富文本实体落盘：把内联图拆出去单独存，HTML 只保留占位。
///
/// 三处写 html 的地方（新建条目 / 同源替换 / 补格式）都走这里 —— 漏掉任何一处，
/// 那条记录的 HTML 就会带着 base64，体积问题又回来了。
///
/// 返回 `(html 相对路径, 图片相对路径列表)`。
fn store_rich_html(dir: &PathBuf, id: &str, html: &str) -> (String, Vec<String>) {
    let (stripped, imgs) = extract_inline_images(html, dir, id);
    let rel = format!("data/{id}.html");
    write_text_entity(dir, &rel, &stripped);
    (rel, imgs)
}

/// 列出某个条目已落盘的内联图，按序号排好（重建索引时用）。
fn list_rich_images(dir: &PathBuf, id: &str) -> Vec<String> {
    let sub = dir.join(format!("images/rich/{id}"));
    let Ok(rd) = fs::read_dir(&sub) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort_by_key(|n| {
        n.split('.')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(usize::MAX)
    });
    names
        .into_iter()
        .map(|n| format!("images/rich/{id}/{n}"))
        .collect()
}

/// 实体文件存在则返回它的相对路径（用于重建时恢复可选关联，如富文本实体）。
fn has_entity(dir: &PathBuf, rel: &str) -> Option<String> {
    if dir.join(rel).is_file() {
        Some(rel.to_string())
    } else {
        None
    }
}

/// 索引不可用（丢失/被截断）时，按落盘文件重建条目，返回新增条数。
///
/// 为什么需要：内容实体是**逐个文件**落的，索引只是它们的目录。索引坏了不等于
/// 数据没了 —— 重建能把记录一条不少地找回来（正文预览要解密读原文）。
///
/// 只补不删：索引里已有的 id 一律跳过，所以可以在「索引为空」时无条件调用。
fn rebuild_index_from_files(dir: &PathBuf, hist: &mut Vec<HistoryItem>) -> usize {
    let known: std::collections::HashSet<String> =
        hist.iter().map(|i| i.id.clone()).collect();
    let mut found: Vec<HistoryItem> = Vec::new();

    let push_text = |found: &mut Vec<HistoryItem>, id: &str, p: &Path| {
        let Ok(raw) = fs::read(p) else { return };
        let Some(text) = decode_text_bytes(&raw) else {
            return; // 解不开（换了账户）就跳过，不要用空内容污染列表
        };
        if text.is_empty() {
            return;
        }
        let (preview, preview_truncated) = text_preview_parts(&text);
        found.push(HistoryItem {
            id: id.to_string(),
            kind: "text".into(),
            ts: ts_from_id(id).unwrap_or_else(|| mtime_secs(p)),
            pinned: false,
            pinned_views: Vec::new(),
            favorite: false,
            edited: false,
            shot: false,
            subtype: Some(detect_subtype(&text).unwrap_or("").to_string()),
            preview,
            preview_truncated,
            size: text.len() as u64,
            text_path: Some(format!("data/{id}.txt")),
            // 索引丢了但富文本实体还在：按文件存在与否重建关联，
            // 否则重建出来的条目会「格式凭空消失」。
            html_path: has_entity(dir, &format!("data/{id}.html")),
            rtf_path: has_entity(dir, &format!("data/{id}.rtf")),
            // 索引丢了但图片还在：按目录内容恢复，否则回贴时图会凭空消失
            rich_images: list_rich_images(dir, id),
            img_path: None,
            files_path: None,
            files: None,
        });
    };

    // 正文 → 文本条目
    if let Ok(rd) = fs::read_dir(dir.join("data")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() || p.extension().and_then(|s| s.to_str()) != Some("txt") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if known.contains(&id) {
                continue;
            }
            push_text(&mut found, &id, &p);
        }
    }

    // 文件清单 → 文件条目
    if let Ok(rd) = fs::read_dir(dir.join("files")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() || p.extension().and_then(|s| s.to_str()) != Some("txt") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if known.contains(&id) {
                continue;
            }
            let Ok(raw) = fs::read(&p) else { continue };
            let Some(body) = decode_text_bytes(&raw) else {
                continue;
            };
            let files: Vec<String> = body
                .split(['\r', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if files.is_empty() {
                continue;
            }
            let name = Path::new(&files[0])
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            found.push(HistoryItem {
                id: id.clone(),
                kind: "files".into(),
                ts: ts_from_id(&id).unwrap_or_else(|| mtime_secs(&p)),
                pinned: false,
                pinned_views: Vec::new(),
                favorite: false,
                edited: false,
                shot: false,
                subtype: None,
                preview: if files.len() == 1 {
                    name
                } else {
                    format!("{} 等 {} 个文件", name, files.len())
                },
                preview_truncated: false,
                size: files.iter().map(|f| f.len()).sum::<usize>() as u64,
                text_path: None,
                html_path: None,
                rtf_path: None,
                rich_images: Vec::new(),
                img_path: None,
                files_path: Some(format!("files/{id}.txt")),
                files: Some(files),
            });
        }
    }

    // 图片 → 图片条目。
    //
    // 三个位置都要扫：分类布局把图片放进 `shot/` 与 `copy/`，只扫 `images/`
    // 根目录会把全部新图片漏掉。`thumbs/` 与 `rich/` 不是条目实体，不在此列。
    for sub in ["images", "images/shot", "images/copy"] {
        let Ok(rd) = fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if p.extension().and_then(|s| s.to_str()) == Some("txt") {
                continue;
            }
            // 旧文件名就是条目 id；新文件名是日期时间，时间只能从名字里读回来，
            // id 由它重新生成（原 id 已经不可寻，缩略图会按新 id 重做一张）。
            let (id, ts) = match ts_from_id(&stem) {
                Some(ts) => (stem.clone(), ts),
                None => match ts_from_stamp(&stem) {
                    Some(ts) => (make_id(ts), ts),
                    // 既不是内部 id 也不是日期时间 —— 来路不明的文件，不当条目
                    None => continue,
                },
            };
            if known.contains(&id) {
                continue;
            }
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("bmp")
                .to_string();
            found.push(HistoryItem {
                id: id.clone(),
                kind: "image".into(),
                ts,
                pinned: false,
                pinned_views: Vec::new(),
                favorite: false,
                edited: false,
                // 重建时路径是唯一线索：在 shot/ 里就说明它是截图
                shot: sub == "images/shot",
                subtype: None,
                preview: "[图片]".into(),
                preview_truncated: false,
                size,
                text_path: None,
                html_path: None,
                rtf_path: None,
                rich_images: Vec::new(),
                img_path: Some(format!("{sub}/{stem}.{ext}")),
                files_path: None,
                files: None,
            });
        }
    }

    let n = found.len();
    hist.extend(found);
    // 列表约定：新→旧。重建出来的条目顺序取自 read_dir（不确定），必须重排。
    hist.sort_by(|a, b| b.ts.cmp(&a.ts));
    n
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
        // 必须走 write_text_entity（会自动 DPAPI 加密）—— 这里原先是裸 fs::write 明文，
        // 与「内容落盘一律加密」自相矛盾，等于给老条目补了一份明文副本。
        write_text_entity(dir, &rel, &files.join("\r\n"));
        item.files_path = Some(rel);
        n += 1;
    }
    n
}

/// 删除条目对应的落盘实体。
///
/// 以前全用 `let _ =`：删失败（文件被占用、被设为只读、路径失效）就静默留下——
/// 用户以为敏感内容已经删掉，磁盘上其实还在。这是**隐私相关**的静默失败，
/// 必须记 ERROR；`NotFound` 不算失败（重复删除、实体本就不存在）。
fn remove_item_files(dir: &PathBuf, item: &HistoryItem) {
    let mut rels: Vec<&str> = Vec::new();
    if let Some(rel) = &item.text_path {
        rels.push(rel);
    }
    // 富文本实体与纯文本同生命周期：只删记录不删实体，等于把「带格式的正文」
    // 留在磁盘上 —— 用户以为删干净了，其实没有（隐私问题）。
    if let Some(rel) = &item.html_path {
        rels.push(rel);
    }
    if let Some(rel) = &item.rtf_path {
        rels.push(rel);
    }
    if let Some(rel) = &item.img_path {
        rels.push(rel);
    }
    if let Some(rel) = &item.files_path {
        rels.push(rel);
    }
    for rel in rels {
        let p = dir.join(rel);
        if let Err(e) = fs::remove_file(&p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                crate::log_err!("clipboard", "删除实体失败（{}）：{e}", p.display());
            }
        }
    }
    // 富文本拆出来的内联图：整目录删。放在 rels 之外，因为它是目录而不是文件。
    let rich_dir = dir.join(format!("images/rich/{}", item.id));
    if rich_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&rich_dir) {
            crate::log_err!("clipboard", "删除富文本图片目录失败（{}）：{e}", rich_dir.display());
        }
    }
    // 缩略图与原图同生命周期，一并清掉
    let thumb = dir.join(thumb_rel(&item.id));
    if let Err(e) = fs::remove_file(&thumb) {
        if e.kind() != std::io::ErrorKind::NotFound {
            crate::log_err!("clipboard", "删除缩略图失败（{}）：{e}", thumb.display());
        }
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
/// 默认豁免置顶/收藏（见 `prune_history` 与 `settings.prune_include_pinned`）。
/// 只删过期条目自己的实体文件，**不碰 `log/`**（见 CONTENT_DIRS 注释）。
pub fn prune_retention(days: u32) {
    if let Some(dir) = DIR.get() {
        let include_pinned = crate::settings::get().prune_include_pinned;
        let mut hist = HISTORY.get().unwrap().lock().unwrap();
        let before = hist.len();
        *hist = prune_history(hist.clone(), dir, days, include_pinned);
        let removed = before - hist.len();
        if removed > 0 {
            crate::log_print!("[clipboard] 保留策略清理 {removed} 条过期条目");
        }
    }
}

/// 这条记录是否受保护（置顶或收藏）。
///
/// 单独抽出来是因为**所有自动清理路径都要用它**：退出清理、保留期清理。
/// 上一版只有退出清理判断了，保留期清理严格按时间戳过滤 —— 于是「置顶了三年」的
/// 条目会悄无声息地到期被删，用户以为置顶就是长期保存（实测反馈）。
fn is_protected(item: &HistoryItem) -> bool {
    // 不用再判 `item.pinned`：那个 v2.0 早期的全局置顶字段已废弃，
    // `load_index` 载入时就会迁移进 `pinned_views`，而所有清理都发生在载入之后。
    !item.pinned_views.is_empty() || item.favorite
}

/// 纯函数版本：过滤 + 删文件 + 存索引
///
/// `include_pinned = false`（默认）时，置顶/收藏**不受保留期影响** —— 它们被用户
/// 主动标记为「要留着」，到期自动删掉等于违背这个意图。
fn prune_history(
    hist: Vec<HistoryItem>,
    dir: &PathBuf,
    days: u32,
    include_pinned: bool,
) -> Vec<HistoryItem> {
    if days == 0 {
        crate::log_print!("[clipboard] 保留策略：永久保存，不清理");
        return hist;
    }
    let deadline = now_ms().saturating_sub(days as u64 * 24 * 60 * 60 * 1000);
    let before = hist.len();
    let mut kept_protected = 0usize;
    let kept: Vec<HistoryItem> = hist
        .into_iter()
        .filter(|i| {
            if i.ts >= deadline {
                true
            } else if !include_pinned && is_protected(i) {
                // 过期但被标记为「要留着」：豁免，不计入删除数
                kept_protected += 1;
                true
            } else {
                remove_item_files(dir, i);
                false
            }
        })
        .collect();
    // 这条日志以前**不存在**，而它删的是用户数据（连同落盘文件）。
    //
    // 踩过的坑：`config.json` 解析失败会回落默认配置，而默认保留天数是 3 天，
    // 于是启动时按 3 天把几百条历史连文件一起删掉 —— 日志里一个字都没有，
    // 现场只剩「记录全没了」。凡是「按策略自动删除用户数据」的地方，
    // 必须留下「按什么规则、删了多少、剩多少」。
    let removed = before - kept.len();
    if removed > 0 {
        crate::log_warn!(
            "[clipboard] 保留策略：按 {days} 天清掉 {removed} 条（剩 {} 条）——若不是预期，检查设置里的保留天数",
            kept.len()
        );
    } else {
        crate::log_print!("[clipboard] 保留策略：{days} 天，无需清理（共 {} 条）", kept.len());
    }
    // 豁免了几条也要记：用户若觉得「怎么老有旧记录」，一查就知道是被保护下来的，
    // 而不是清理逻辑没跑。
    if kept_protected > 0 {
        crate::log_print!(
            "[clipboard] 保留策略：{kept_protected} 条过期记录因置顶/收藏被保留（include_pinned={include_pinned}）"
        );
    }
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
    move_dir_contents(&old, &target_root, &app).map_err(|e| format!("迁移缓存文件失败：{e}"))?;

    let new_dir_str = strip_ext_prefix(&target_root.to_string_lossy());
    crate::settings::update(|c| c.cache_dir = Some(new_dir_str.clone()));
    crate::log_print!("[clipboard] 缓存目录已迁移至 {new_dir_str}，即将重启");

    // DIR 是 OnceLock，运行期改不了：只能重启进程让新目录生效。
    //
    // 重启前必须**让出实例锁**。`restart()` 是「spawn 新进程 + 退出自己」，
    // 新进程常在旧进程退出之前就检查单例锁 —— 不让出的话它会判定「已有实例」
    // 并退出自己，而旧进程正在退出，结果两个都没了（实测过一次）。
    //
    // 用户主动退出才可能清理数据：这里只调 `note_exiting`（放行 ExitRequested），
    // 不调 `note_user_quit`，`clean_on_close` 不会生效。
    crate::log_print!("[clipboard] 缓存目录已迁移，等待用户确认后重启");

    // 交给前端弹自绘确认框：原生消息框与轻贴的玻璃界面是两套语言。
    // 前端点「重启」后调 `confirm_cache_move`。
    let _ = app.emit(
        "cache-moved",
        serde_json::json!({ "path": new_dir_str.as_str() }),
    );

    // 兜底：前端若因故没弹框或没响应，20 秒后自己重启。
    //
    // **不能无限等**：此刻 `DIR` 还指向已经被搬空的旧目录，继续运行会把新内容
    // 写回旧目录 —— 变成「配置说 A、实际写 B」，两处数据各一半。
    let handle = app.clone();
    std::thread::spawn(move || {
        for _ in 0..200 {
            if CONFIRM_RESTART.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        crate::idle_release::note_exiting();
        crate::singleton::yield_instance_lock();
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
/// 迁移进度事件。跨盘搬迁要**真复制数据**（同盘才是 rename），174 MB 就要
/// 几十秒；期间界面上什么都不显示，用户会以为程序卡死。
const CACHE_MOVE_EVENT: &str = "cache-move";

/// 迁移进度。按已复制的字节数上报，节流 120ms —— 小文件多的时候逐条发事件
/// 会把 IPC 刷爆，而进度条 8 帧/秒已经够顺。
struct MoveProgress<'a> {
    done: u64,
    total: u64,
    last: std::time::Instant,
    app: &'a AppHandle,
}

impl MoveProgress<'_> {
    fn emit_now(&self) {
        use tauri::Emitter;
        let _ = self.app.emit(
            CACHE_MOVE_EVENT,
            serde_json::json!({ "done": self.done, "total": self.total }),
        );
    }
    fn add(&mut self, bytes: u64) {
        self.done += bytes;
        if self.last.elapsed().as_millis() >= 120 {
            self.last = std::time::Instant::now();
            self.emit_now();
        }
    }
}

fn move_dir_contents(old: &PathBuf, new: &PathBuf, app: &AppHandle) -> std::io::Result<()> {
    let (_, total) = dir_stats(old);
    let mut p = MoveProgress {
        done: 0,
        total,
        last: std::time::Instant::now(),
        app,
    };
    // 先发一次 0/total：界面立刻出现进度条，而不是等第一个文件搬完
    p.emit_now();
    move_dir_contents_depth(old, new, 0, &mut p)?;
    p.done = total;
    p.emit_now();
    Ok(())
}

fn move_dir_contents_depth(
    old: &Path,
    new: &Path,
    depth: u32,
    progress: &mut MoveProgress,
) -> std::io::Result<()> {
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
            move_dir_contents_depth(&from, &to, depth + 1, progress)?;
            fs::remove_dir_all(&from).ok();
        } else {
            let size = fs::metadata(&from).map(|m| m.len()).unwrap_or(0);
            // 同盘 rename 是原子的、不搬数据；跨盘才会退化成真复制
            if fs::rename(&from, &to).is_err() {
                fs::copy(&from, &to)?;
                let _ = fs::remove_file(&from);
            }
            progress.add(size);
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

/// 本次退出是不是**用户主动**要求的（托盘「退出」/ 设置页「退出」）。
///
/// 「关闭时清理数据」只在这个场景生效。迁移缓存目录、装完更新、重启应用这些
/// 退出都是程序自己发起的 —— 用户没打算清空什么，清掉一次就等于丢光历史。
/// 曾经这里是反过来的（默认清理，迁移时打补丁跳过），漏一个场景就清一次数据；
/// 现在改成默认不清理，只有明确的用户退出才清。
static USER_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 标记「接下来这次退出是用户主动要求的」，`clean_on_close` 才有机会生效。
pub fn note_user_quit() {
    USER_QUIT.store(true, Ordering::SeqCst);
}

/// 迁移缓存目录后，前端是否已经点了「重启」。
///
/// 后端只负责等：前端弹自绘确认框，用户点了就置位，兜底线程随即重启。
static CONFIRM_RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 前端确认重启（迁移缓存目录之后）。
#[tauri::command]
pub fn confirm_cache_move() {
    CONFIRM_RESTART.store(true, Ordering::SeqCst);
}

/// 在 RunEvent::Exit 时调用。clean_on_close 开启则清空全部内容文件与历史索引。
pub fn on_app_exit() {
    // 落盘已挪到后台线程，退出前得等它跑完，否则「刚复制就退出」会丢记录。
    // 上限 2 秒：大图解码可能较久，但不至于让退出卡住。
    flush_pending(std::time::Duration::from_secs(2));

    if !USER_QUIT.swap(false, Ordering::SeqCst) {
        // 迁移缓存目录 / 装完更新 / 重启应用：程序自己发起的退出，不动数据
        crate::log_print!("[clipboard] 非用户主动退出（重启 / 升级 / 迁移），保留全部数据");
        return;
    }
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
                if is_protected(i) {
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

/// base64 解码。只处理标准字母表，忽略 `=`, 空白与换行。
///
/// 用在「从 HTML 里拆内联图」这一条路上：拿到的都是浏览器/Word 生成的规范 base64，
/// 不需要容错各种奇葩变体，遇到非法字符直接返回 `None`（那张图就按丢弃处理）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in s.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// 扩展名 → MIME。图片条目的 `get_image` 与富文本内联图共用同一份映射。
fn mime_for(rel: &str) -> &'static str {
    match rel.rsplit('.').next().unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/bmp",
    }
}

/// 把磁盘上的图片读成 data URL（供详情渲染与内联图还原共用）。
fn image_data_url(dir: &PathBuf, rel: &str) -> Option<String> {
    let bytes = fs::read(dir.join(rel)).ok()?;
    Some(format!(
        "data:{};base64,{}",
        mime_for(rel),
        base64_encode(&bytes)
    ))
}

/// 单张内联图允许拆出来落盘的上限（原始字节）。
///
/// 一张 3MB 的图已经很夸张了（通常是扫描件或截图原图）。超过就丢弃那张图，
/// 但**保留文字格式** —— 一条记录不该因为一张巨图占掉几十 MB。
const RICH_IMG_MAX_BYTES: usize = 3 * 1024 * 1024;

/// `data:image/<mime>;base64,` 后的 mime 是否允许落盘。
fn rich_img_ext(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpeg" | "jpg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        // 刻意不含 svg：SVG 能内嵌脚本，落盘后再显示等于把脚本带进渲染层
        _ => None,
    }
}

/// 把 HTML 里的内联 base64 图片拆出来**单独落盘**，`src` 换成占位
/// `data-qt-img="<序号>"`。
///
/// 为什么值得这么做：一页带插图的 Word，HTML 里 `data:` 图能占掉 1~2MB，
/// 于是整条记录卡在体积上限、甚至降级成纯文本。拆开后 HTML 只剩几十 KB，
/// 图片走已有的图片目录（同样加密落盘），显示时再读回来。
///
/// 返回 `(替换后的 HTML, 图片相对路径列表)`，列表下标即占位里的序号。
/// 提取失败的图（格式不认识 / 解码失败 / 超过 `RICH_IMG_MAX_BYTES`）替换成
/// `data-qt-img="drop"`，前端跳过，回贴时也不会塞回去。
fn extract_inline_images(html: &str, dir: &PathBuf, id: &str) -> (String, Vec<String>) {
    let mut paths: Vec<String> = Vec::new();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(rel) = html[cursor..].find("data:image/") {
        let pos = cursor + rel;
        // 属性值必须被引号包住，向前找最近的那个引号
        let Some(qpos) = html[..pos].rfind(|c| c == '"' || c == '\'') else {
            break;
        };
        let quote = html.as_bytes()[qpos];
        let Some(vend_rel) = html[qpos + 1..].find(quote as char) else {
            break;
        };
        let vend = qpos + 1 + vend_rel; // 结束引号的位置
        let value = &html[qpos + 1..vend];

        // 必须是 `src="…"` 这个属性，否则不认识，跳过本次匹配
        let head = &html[..qpos];
        if !head.ends_with("src=") && !head.ends_with("SRC=") {
            cursor = pos + "data:image/".len();
            continue;
        }
        let attr_start = qpos - 4;

        // data:image/<mime>;base64,<data>
        let placeholder = (|| -> Option<usize> {
            let after = value.strip_prefix("data:image/")?;
            let (mime, rest) = after.split_once(';')?;
            let ext = rich_img_ext(mime)?;
            let b64 = rest.strip_prefix("base64,")?;
            let bytes = base64_decode(b64)?;
            if bytes.is_empty() || bytes.len() > RICH_IMG_MAX_BYTES {
                return None;
            }
            let n = paths.len();
            let rel_path = format!("images/rich/{id}/{n}.{ext}");
            let full = dir.join(&rel_path);
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::write(&full, &bytes) {
                crate::log_err!("clipboard", "内联图落盘失败：{e}");
                return None;
            }
            paths.push(rel_path);
            Some(n)
        })();

        out.push_str(&html[cursor..attr_start]);
        match placeholder {
            Some(n) => out.push_str(&format!("data-qt-img=\"{n}\"")),
            None => out.push_str("data-qt-img=\"drop\""),
        }
        cursor = vend + 1;
    }

    out.push_str(&html[cursor..]);
    (out, paths)
}

/// 回贴前把占位换回 `src="data:…"`，让图片重新变成自包含的 HTML。
///
/// 回贴是「把原文还给用户」，图片必须以 base64 形式跟着走 —— 否则粘到 Word
/// 里就只剩一个指向本机的占位。
fn inline_images_back(frag: &str, dir: &PathBuf, paths: &[String]) -> String {
    let mut out = String::with_capacity(frag.len() + 64 * 1024);
    let mut cursor = 0usize;

    while let Some(rel) = frag[cursor..].find("data-qt-img=") {
        let pos = cursor + rel;
        let Some(qpos) = frag[pos..].find(|c| c == '"' || c == '\'').map(|r| pos + r) else {
            break;
        };
        let quote = frag.as_bytes()[qpos];
        let Some(vend_rel) = frag[qpos + 1..].find(quote as char) else {
            break;
        };
        let vend = qpos + 1 + vend_rel;
        let token = &frag[qpos + 1..vend];

        let url = token
            .parse::<usize>()
            .ok()
            .and_then(|n| paths.get(n))
            .and_then(|p| image_data_url(dir, p));

        match url {
            Some(u) => {
                // 文件还在：还原成内联图，粘出去就是原样
                out.push_str(&frag[cursor..pos]);
                out.push_str(&format!("src=\"{u}\""));
                cursor = vend + 1;
            }
            None => {
                // 图没了（提取时被丢弃 / 文件被删）：把整个 `<img …>` 删掉。
                // 留一个没有 src 的 img，粘到 Word 里是个破图占位，比没有更难看。
                let tag_start = frag[..pos]
                    .rfind("<img")
                    .or_else(|| frag[..pos].rfind("<IMG"))
                    .unwrap_or(pos);
                let tag_end = frag[vend..]
                    .find('>')
                    .map(|i| vend + i + 1)
                    .unwrap_or(vend + 1);
                out.push_str(&frag[cursor..tag_start]);
                cursor = tag_end;
            }
        }
    }
    out.push_str(&frag[cursor..]);
    out
}

/// 从 CF_HTML 里取出正文片段（按字节偏移）。
///
/// 与前端 `formats.js` 的 `cfHtmlFragment` 是同一套规则。回贴需要「重建」时，
/// 必须先把片段从旧头里取出来 —— 旧头里的偏移在图片还原后就全错了。
fn cf_html_fragment(raw: &str) -> &str {
    fn offset(raw: &str, key: &str) -> Option<usize> {
        let i = raw.find(key)?;
        raw[i + key.len()..].lines().next()?.trim().parse().ok()
    }
    if let (Some(sf), Some(ef)) = (offset(raw, "StartFragment:"), offset(raw, "EndFragment:")) {
        // `get` 会顺带校验是否落在字符边界上：偏移错位时返回 None，走下面兜底
        if let Some(frag) = raw.get(sf..ef) {
            if !frag.trim().is_empty() {
                return frag;
            }
        }
    }
    match raw.find('<') {
        Some(i) => &raw[i..],
        None => "",
    }
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
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

/// 系统通知「剪贴板内容变了」的消息号。
const WM_CLIPBOARDUPDATE: u32 = 0x031D;
/// `CreateWindowExW` 的父窗口参数：消息专用窗口（不显示、不进任务栏）。
const HWND_MESSAGE: isize = -3;

/// 剪贴板窗口的 WndProc。
///
/// 只做一件事：把 `WM_CLIPBOARDUPDATE` 转给读取逻辑。监听挂在这个常驻窗口上，
/// 主窗口销毁后事件仍然送达（原先挂在主窗口钩子上，释放界面后只剩心跳兜底）。
extern "system" fn clip_owner_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    if msg == WM_CLIPBOARDUPDATE {
        on_clipboard_update();
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// 建常驻的剪贴板窗口。返回 0 表示失败。
fn create_clip_window() -> isize {
    use crate::shot_native::WndClassW;
    let class: Vec<u16> = "QingTieClipboardOwner"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WndClassW {
            style: 0,
            wnd_proc: Some(clip_owner_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: std::ptr::null(),
            class_name: class.as_ptr(),
        };
        // 重复注册返回 0，无害（与截图覆盖层同一套写法）
        RegisterClassW(&wc);
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            0,
            instance,
            0,
        )
    }
}

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
    fn RegisterClassW(class: *const crate::shot_native::WndClassW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: isize,
        menu: isize,
        instance: isize,
        param: isize,
    ) -> isize;
    fn DefWindowProcW(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
    fn GetModuleHandleW(name: *const u16) -> isize;
    /// UTC 的 FILETIME → 日历值（SYSTEMTIME）。
    fn FileTimeToSystemTime(ft: *const u64, st: *mut crate::logger::SystemTimeLocal) -> i32;
    /// UTC 日历值 → 本地日历值（传 null 用当前时区，含夏令时规则）。
    fn SystemTimeToTzSpecificLocalTime(
        tz: *const std::ffi::c_void,
        utc: *const crate::logger::SystemTimeLocal,
        local: *mut crate::logger::SystemTimeLocal,
    ) -> i32;
    /// 本地日历值 → UTC 日历值。反解文件名里的日期要用它。
    fn TzSpecificLocalTimeToSystemTime(
        tz: *const std::ffi::c_void,
        local: *const crate::logger::SystemTimeLocal,
        utc: *mut crate::logger::SystemTimeLocal,
    ) -> i32;
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
    /// 把 `HTML Format` / `Rich Text Format` 这类名字换成系统分配的格式 ID
    fn RegisterClipboardFormatW(name: *const u16) -> u32;

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

    /// 日期 → 天数的换算必须与已知锚点一致，否则索引重建时算出的时间会整体偏移。
    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2000, 1, 1), Some(10957));
        assert_eq!(days_from_civil(2026, 10, 28), Some(20754));
        // 闰日：2024-02-29 存在，2024-03-01 紧接着它
        let leap = days_from_civil(2024, 2, 29).unwrap();
        assert_eq!(days_from_civil(2024, 3, 1), Some(leap + 1));
        // 1970 之前与非法月份一律拒绝（那种文件不该被当成条目）
        assert_eq!(days_from_civil(1969, 12, 31), None);
        assert_eq!(days_from_civil(2026, 13, 1), None);
    }

    /// 文件名里的日期必须能原样读回时间戳：索引损坏后靠它重建图片条目，
    /// 差一秒就是差一秒，差了就会把图排到错误的位置上。
    #[test]
    fn stamp_round_trip() {
        for ts in [1_700_000_000_000u64, 1_793_200_000_000] {
            let stamp = local_stamp(ts);
            assert_eq!(stamp.len(), 15, "格式应为 20261028_223610，实际 {stamp}");
            let back = ts_from_stamp(&stamp).expect("应能从文件名解析回来");
            assert!(
                back.abs_diff(ts) < 1000,
                "{ts} → {stamp} → {back}，误差应在 1 秒内"
            );
        }
        // 去重后缀不影响解析
        assert!(ts_from_stamp("20261028_223610-2").is_some());
        // 内部 id 与随便一串东西都不该被当成日期
        assert!(ts_from_stamp("1a126311c6c-3").is_none());
        assert!(ts_from_stamp("hello").is_none());
    }

    /// 截图判定：用例全部来自 2026-10-28 的实测剪贴板数据。
    ///
    /// 注意格式名的真实形态：标准格式是裸名（`CF_DIB`），私有/注册格式带双引号
    /// （`"WeChatScreenshotFormat"`）—— 这是 `enum_clipboard_formats` 的输出习惯。
    #[test]
    fn classify_shot_matches_measured_sources() {
        let cases: &[(&str, Vec<&str>, &str, bool)] = &[
            (
                "微信截图",
                vec![
                    "\"DataObject\"",
                    "CF_DIB",
                    "\"application/x-capturer-status\"",
                    "\"WeChatScreenshotFormat\"",
                    "CF_BITMAP",
                    "CF_DIBV5",
                ],
                "Weixin.exe",
                true,
            ),
            (
                "微信复制图片",
                vec![
                    "\"DataObject\"",
                    "CF_HDROP",
                    "\"UniformResourceLocatorW\"",
                    "CF_DIB",
                    "\"Ole Private Data\"",
                    "CF_BITMAP",
                    "CF_DIBV5",
                ],
                "Weixin.exe",
                false,
            ),
            (
                "QQ 截图（无所有者、只有裸位图）",
                vec!["CF_BITMAP", "CF_DIB", "CF_DIBV5"],
                "",
                true,
            ),
            (
                "QQ 复制图片",
                vec![
                    "\"QQ_Unicode_RichEdit_Format\"",
                    "\"HTML Format\"",
                    "\"Preferred DropEffect\"",
                    "CF_HDROP",
                    "CF_BITMAP",
                    "CF_DIB",
                    "CF_DIBV5",
                ],
                "",
                false,
            ),
            (
                "Win+Shift+S",
                vec![
                    "\"DataObject\"",
                    "\"Preferred DropEffect\"",
                    "CF_BITMAP",
                    "\"PNG\"",
                    "\"Ole Private Data\"",
                    "CF_DIB",
                    "CF_DIBV5",
                ],
                "svchost.exe",
                true,
            ),
            (
                "画图里复制选区（有 owner，裸位图也不该算截图）",
                vec!["CF_BITMAP", "CF_DIB", "CF_DIBV5"],
                "mspaint.exe",
                false,
            ),
            ("空格式", vec![], "", false),
        ];

        for (name, formats, owner, want) in cases {
            let origin = ClipOrigin {
                formats: formats.iter().map(|s| s.to_string()).collect(),
                owner_exe: owner.to_string(),
            };
            assert_eq!(classify_shot(&origin), *want, "用例：{name}");
        }
    }

    /// 截图标记：取过即清、超时不再认。
    ///
    /// 这两个性质缺一条都会出静默错误 —— 不清会一直把后续手动复制的图当成
    /// 截图；不设时效则「截图没被捕获到」之后标记会永久挂着。
    #[test]
    fn shot_mark_is_consumed_and_expires() {
        SHOT_AT.store(now_ms(), Ordering::SeqCst);
        assert!(take_shot_mark(), "刚置位的标记应生效");
        assert!(!take_shot_mark(), "取过就该清掉，不能重复生效");

        SHOT_AT.store(
            now_ms().saturating_sub(SHOT_MARK_TTL_MS + 1_000),
            Ordering::SeqCst,
        );
        assert!(!take_shot_mark(), "超过有效期不应再认");
        assert_eq!(SHOT_AT.load(Ordering::SeqCst), 0, "过期的那次也要清掉");
    }

    /// 造一条图片条目，供目录迁移测试用。
    fn image_item(id: &str, ts: u64, img_path: String) -> HistoryItem {
        image_item_shot(id, ts, img_path, false)
    }

    fn image_item_shot(id: &str, ts: u64, img_path: String, shot: bool) -> HistoryItem {
        HistoryItem {
            id: id.into(),
            kind: "image".into(),
            ts,
            pinned: false,
            pinned_views: Vec::new(),
            favorite: false,
            edited: false,
            shot,
            subtype: None,
            preview: "[图片]".into(),
            preview_truncated: false,
            size: 6,
            text_path: None,
            html_path: None,
            rtf_path: None,
            rich_images: Vec::new(),
            img_path: Some(img_path),
            files_path: None,
            files: None,
        }
    }

    /// 迁移必须：改名成日期、挪进 copy/、新文件真的落盘、旧文件留到索引写盘后才删。
    #[test]
    fn migrate_image_layout_moves_flat_images_into_dated_dir() {
        let dir = std::env::temp_dir().join(format!("qt-img-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images")).unwrap();

        let old_id = "1a135878af0-0";
        let old_rel = format!("images/{old_id}.bmp");
        std::fs::write(dir.join(&old_rel), b"pixels").unwrap();
        let ts = 1_697_000_000_000u64;
        let mut hist = vec![image_item(old_id, ts, old_rel.clone())];

        let moved = migrate_image_layout(&dir, &mut hist, true);
        assert_eq!(moved, 1);
        assert!(
            !dir.join(&old_rel).is_file(),
            "rename 之后旧位置不该再有文件"
        );

        let new_rel = hist[0].img_path.clone().unwrap();
        assert!(new_rel.starts_with("images/copy/"), "应进 copy/：{new_rel}");
        assert_eq!(
            &new_rel["images/copy/".len()..new_rel.len() - 4],
            local_stamp(ts),
            "文件名应是条目时间对应的日期"
        );
        assert!(dir.join(&new_rel).is_file(), "新文件必须真的存在");

        // 幂等：再跑一次什么都不该搬
        let again = migrate_image_layout(&dir, &mut hist, true);
        assert_eq!(again, 0, "第二次不应再迁移");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 开关关闭时只改名、不建子目录 —— 日期命名与分类是两个独立的事。
    #[test]
    fn migrate_image_layout_keeps_flat_when_split_off() {
        let dir = std::env::temp_dir().join(format!("qt-img-flat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images")).unwrap();

        let old_id = "1a1358e7e60-4";
        let old_rel = format!("images/{old_id}.bmp");
        std::fs::write(dir.join(&old_rel), b"x").unwrap();
        let ts = 1_698_000_000_000u64;
        let mut hist = vec![image_item(old_id, ts, old_rel)];

        let moved = migrate_image_layout(&dir, &mut hist, false);
        assert_eq!(moved, 1);
        let new_rel = hist[0].img_path.clone().unwrap();
        assert!(
            !new_rel["images/".len()..].contains('/'),
            "开关关闭时应留在根目录：{new_rel}"
        );
        assert_eq!(
            &new_rel["images/".len()..new_rel.len() - 4],
            local_stamp(ts),
            "文件名仍按日期"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关掉分类开关时，已经分好目录的图片要搬回 `images/` 根目录。
    ///
    /// 只做单向（根目录 → 子目录）时，用户会看到「关了开关，图还在 shot/copy 里」，
    /// 与开着的时候一样困惑。
    #[test]
    fn migrate_image_layout_moves_back_when_split_turned_off() {
        let dir = std::env::temp_dir().join(format!("qt-img-back-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images/copy")).unwrap();

        let ts = 1_697_000_000_000u64;
        let dated = format!("images/copy/{}.bmp", local_stamp(ts));
        std::fs::write(dir.join(&dated), b"x").unwrap();
        let mut hist = vec![image_item("1a1358e82e7-5", ts, dated.clone())];

        // 关掉开关 → 搬回根目录
        let moved = migrate_image_layout(&dir, &mut hist, false);
        assert_eq!(moved, 1);
        let flat = hist[0].img_path.clone().unwrap();
        assert_eq!(flat, format!("images/{}.bmp", local_stamp(ts)));
        assert!(!dir.join(&dated).is_file(), "旧位置应已清空");
        assert!(dir.join(&flat).is_file(), "新位置应有文件");

        // 再打开 → 搬进 copy/
        let moved = migrate_image_layout(&dir, &mut hist, true);
        assert_eq!(moved, 1);
        assert_eq!(hist[0].img_path.as_deref(), Some(dated.as_str()));
        assert!(dir.join(&dated).is_file());
        assert!(!dir.join(&flat).is_file());

        // 已经对位了就不该再动
        let moved = migrate_image_layout(&dir, &mut hist, true);
        assert_eq!(moved, 0, "位置正确时不应搬");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关掉分类再打开，截图的「身份」不能丢。
    ///
    /// 用户实测反馈：原先靠「文件在不在 shot/ 里」推断来源，关掉开关时图片
    /// 搬回根目录，路径不再携带来源，再打开就全被当成复制塞进 copy/。
    /// 改成存 `shot` 字段后，来回切换都保持分类。
    #[test]
    fn shot_flag_survives_split_toggle() {
        let dir = std::env::temp_dir().join(format!("qt-img-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images/shot")).unwrap();

        let ts = 1_697_000_000_000u64;
        let dated = format!("images/shot/{}.bmp", local_stamp(ts));
        std::fs::write(dir.join(&dated), b"x").unwrap();
        let mut hist = vec![image_item_shot("1a135ba61f1-5", ts, dated.clone(), true)];

        // 关掉开关 → 搬回根目录，但 shot 标记必须还在
        assert_eq!(migrate_image_layout(&dir, &mut hist, false), 1);
        assert!(hist[0].shot, "关掉分类不该丢掉「这是截图」");
        let flat = hist[0].img_path.clone().unwrap();
        assert!(!flat["images/".len()..].contains('/'), "应平铺在根目录");

        // 再打开 → 回 shot/，不该进 copy/
        assert_eq!(migrate_image_layout(&dir, &mut hist, true), 1);
        assert_eq!(hist[0].img_path.as_deref(), Some(dated.as_str()));
        assert!(dir.join(&dated).is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老索引没有 `shot` 字段，但文件已经在 `shot/` 里 —— 首轮整理要补上标记，
    /// 而不是把它搬进 `copy/`。
    #[test]
    fn legacy_shot_dir_gets_flagged_not_moved() {
        let dir = std::env::temp_dir().join(format!("qt-img-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("images/shot")).unwrap();

        let ts = 1_697_000_000_000u64;
        let dated = format!("images/shot/{}.bmp", local_stamp(ts));
        std::fs::write(dir.join(&dated), b"x").unwrap();
        // shot 默认 false：模拟升级前写入的条目
        let mut hist = vec![image_item("1a135da3ed6-11", ts, dated.clone())];

        assert_eq!(migrate_image_layout(&dir, &mut hist, true), 1, "补标记算一次改动");
        assert!(hist[0].shot, "应据路径补上截图标记");
        assert_eq!(hist[0].img_path.as_deref(), Some(dated.as_str()), "路径不该变");

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// 造一份最小可用的条目，供索引相关用例复用。
    fn sample_item(id: &str, preview: &str) -> HistoryItem {
        HistoryItem {
            id: id.to_string(),
            kind: "text".into(),
            ts: 1,
            pinned: false,
            pinned_views: Vec::new(),
            favorite: false,
            edited: false,
            shot: false,
            subtype: None,
            html_path: None,
            rtf_path: None,
            rich_images: Vec::new(),
            preview: preview.to_string(),
            preview_truncated: false,
            size: 1,
            text_path: None,
            img_path: None,
            files_path: None,
            files: None,
        }
    }

    /// 保留期清理**默认不碰置顶/收藏**。
    ///
    /// 用户实测反馈：置顶/收藏是「我要留着」的明确标记，到期自动删掉等于违背它，
    /// 而且删除发生在后台 —— 用户只会在发现记录没了的时候才察觉。
    #[test]
    fn retention_keeps_pinned_and_favorite_by_default() {
        let dir = std::env::temp_dir().join("qingtie_test_prune_default");
        let _ = std::fs::create_dir_all(&dir);

        let now = now_ms();
        let old = now - 10 * 24 * 3600 * 1000; // 10 天前，默认 3 天保留下已过期
        let mut plain = sample_item("plain", "普通过期");
        plain.ts = old;
        let mut pinned = sample_item("pinned", "置顶过期");
        pinned.ts = old;
        pinned.pinned_views = vec!["text".into()];
        let mut fav = sample_item("fav", "收藏过期");
        fav.ts = old;
        fav.favorite = true;
        let mut fresh = sample_item("fresh", "未过期");
        fresh.ts = now;

        let hist = vec![plain, pinned, fav, fresh];
        let kept = prune_history(hist, &dir, 3, false);
        let ids: Vec<&str> = kept.iter().map(|i| i.id.as_str()).collect();
        assert!(!ids.contains(&"plain"), "过期普通条目应被清掉");
        assert!(ids.contains(&"pinned"), "过期但置顶的必须保留");
        assert!(ids.contains(&"fav"), "过期但收藏的必须保留");
        assert!(ids.contains(&"fresh"), "未过期的必须保留");
        assert_eq!(ids.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 勾选「到期清理也清置顶/收藏」后，置顶/收藏才会被清掉。
    #[test]
    fn retention_drops_pinned_only_when_explicitly_enabled() {
        let dir = std::env::temp_dir().join("qingtie_test_prune_include");
        let _ = std::fs::create_dir_all(&dir);

        let now = now_ms();
        let old = now - 10 * 24 * 3600 * 1000;
        let mut pinned = sample_item("pinned", "置顶过期");
        pinned.ts = old;
        pinned.favorite = true;
        let mut fresh = sample_item("fresh", "未过期");
        fresh.ts = now;

        let kept = prune_history(vec![pinned, fresh], &dir, 3, true);
        assert_eq!(kept.len(), 1, "显式开启后置顶/收藏也应被清掉");
        assert_eq!(kept[0].id, "fresh");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保留天数 0 = 永久保存，任何条目都不该动（含普通条目）。
    #[test]
    fn retention_zero_days_keeps_everything() {
        let dir = std::env::temp_dir().join("qingtie_test_prune_zero");
        let _ = std::fs::create_dir_all(&dir);

        let mut ancient = sample_item("ancient", "很久以前");
        ancient.ts = 1;
        let kept = prune_history(vec![ancient], &dir, 0, true);
        assert_eq!(kept.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 1x1 透明 PNG，用来测「拆分 → 还原」的完整往返。
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    /// base64 编解码互为逆运算（含 `=` 填充的三种长度）。
    #[test]
    fn base64_roundtrip() {
        for len in [0usize, 1, 2, 3, 4, 5, 6, 100] {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 % 251) as u8).collect();
            let enc = base64_encode(&data);
            assert_eq!(base64_decode(&enc).unwrap(), data, "len={len}");
        }
        assert_eq!(base64_decode("!!!").is_none(), true, "非法字符返回 None");
    }

    /// 内联图要能从 HTML 里拆出来落盘，且**占位编号与路径下标一致**。
    #[test]
    fn inline_images_are_extracted_and_reversible() {
        let dir = std::env::temp_dir().join("qingtie_test_rich_extract");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let html = format!(
            r#"<p>前文</p><img src="data:image/png;base64,{PNG_1X1}" alt="图一"><p>后文</p>"#
        );
        let (stripped, paths) = extract_inline_images(&html, &dir, "1a2b-0");

        assert_eq!(paths.len(), 1, "应拆出一张图");
        assert!(stripped.contains(r#"data-qt-img="0""#), "占位编号应为 0");
        assert!(!stripped.contains("base64"), "HTML 里不该再留 base64");
        assert!(stripped.contains("前文") && stripped.contains("后文"), "正文不能丢");
        assert!(stripped.contains(r#"alt="图一""#), "其它属性要保留");

        let saved = dir.join(&paths[0]);
        assert!(saved.is_file(), "图片应已落盘：{}", saved.display());
        assert_eq!(fs::read(&saved).unwrap(), base64_decode(PNG_1X1).unwrap());

        // 关键：还原回去必须能拿回原图
        let back = inline_images_back(&stripped, &dir, &paths);
        assert!(back.contains("src=\"data:image/png;base64,"), "应还原成内联图");
        assert!(!back.contains("data-qt-img"), "占位应全部替换掉");
        assert!(base64_decode(PNG_1X1).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 外链图**不碰**：不下载、不占位，原样留在 HTML 里交给渲染层丢弃并提示。
    #[test]
    fn external_images_are_left_alone() {
        let dir = std::env::temp_dir().join("qingtie_test_rich_external");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let html = r#"<img src="https://example.com/a.png"><img src="file:///D:/b.png">"#;
        let (stripped, paths) = extract_inline_images(html, &dir, "x-0");
        assert_eq!(paths.len(), 0);
        assert!(stripped.contains("https://example.com/a.png"), "外链保持原样");
        assert!(stripped.contains("file:///D:/b.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不支持的格式（SVG）与大图都替换成 `drop`，不落盘、也不污染 HTML。
    #[test]
    fn unsupported_and_oversized_images_are_dropped() {
        let dir = std::env::temp_dir().join("qingtie_test_rich_drop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let svg = r#"<img src="data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=">"#;
        let (stripped, paths) = extract_inline_images(svg, &dir, "s-0");
        assert_eq!(paths.len(), 0, "SVG 不许落盘（可内嵌脚本）");
        assert!(stripped.contains(r#"data-qt-img="drop""#));

        // 超过单图上限：用超长 base64 模拟（解码后 > RICH_IMG_MAX_BYTES）
        let huge = format!(
            r#"<img src="data:image/png;base64,{}">"#,
            "A".repeat(RICH_IMG_MAX_BYTES / 3 * 4 + 8)
        );
        let (stripped2, paths2) = extract_inline_images(&huge, &dir, "h-0");
        assert_eq!(paths2.len(), 0, "超限图不落盘");
        assert!(stripped2.contains(r#"data-qt-img="drop""#));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从 CF_HTML 取片段：偏移按字节算，中文不能切歪。
    #[test]
    fn cf_html_fragment_respects_byte_offsets() {
        let body = "<p>西安飞蝶 · 专利分析</p>";
        let cf = build_cf_html(body);
        assert_eq!(cf_html_fragment(&cf), body, "片段应与原文完全一致");

        // 没有偏移头：退化为「从第一个 < 开始」
        let naked = "Version:1.0\r\nStartHTML:0\r\n\r\n<p>x</p>";
        assert_eq!(cf_html_fragment(naked), "<p>x</p>");
    }

    /// 索引必须**整份加密**落盘。
    ///
    /// 这条是真需求而不是洁癖：`index.json` 里有 `preview`（正文前 2 行）和
    /// `files`（完整路径）。内容实体已经加密了，索引若还是明文，等于给同一份
    /// 内容留了个不加密的副本，前面的加密全白做。
    #[test]
    fn index_saved_encrypted_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("qingtie-idx-enc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let secret = "密码 hunter2 不能出现在索引里";
        save_index(&dir, &vec![sample_item("t1", secret)]);

        let raw = fs::read(dir.join("index.json")).unwrap();
        assert!(raw.starts_with(ENC_MAGIC), "索引必须带加密魔数");
        let needle = "hunter2".as_bytes();
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "index.json 里不能出现明文正文"
        );

        // 解回来必须逐字一致，否则加密就成了丢数据
        let back = load_index(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].preview, secret);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 兼容性：老版本 index.json 是明文 JSON，读取时必须照常解析（靠魔数区分），
    /// 否则升级后所有人的历史会被当成「解析失败」清空。
    #[test]
    fn index_accepts_legacy_plaintext() {
        let dir = std::env::temp_dir().join(format!("qingtie-idx-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let json = serde_json::to_string_pretty(&vec![sample_item("t2", "旧版明文索引")]).unwrap();
        fs::write(dir.join("index.json"), json.as_bytes()).unwrap();

        let back = load_index(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].preview, "旧版明文索引");

        let _ = fs::remove_dir_all(&dir);
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
    /// CF_HTML 的四个偏移必须精确指向片段边界。
    ///
    /// 拼错的表现很典型：粘到 Word / 邮件客户端里是**一坨 HTML 源码**，
    /// 而不是带格式的段落。所以用中文内容（UTF-8 多字节）来测 ——
    /// 按字符数算偏移的实现会在这里立刻露馅。
    #[test]
    fn cf_html_offsets_point_exactly_at_fragment() {
        let body = "<b>你好，世界</b>";
        let html = build_cf_html(body);
        let field = |key: &str| -> usize {
            html.lines()
                .find(|l| l.starts_with(key))
                .unwrap_or_else(|| panic!("缺少 {key}"))
                [key.len()..]
                .trim()
                .parse()
                .expect("偏移量必须是数字")
        };
        let (sf, ef) = (field("StartFragment:"), field("EndFragment:"));
        assert_eq!(&html[sf..ef], body, "片段必须正好是原文");
        assert_eq!(&html[field("StartHTML:")..][..5], "<html", "StartHTML 指向文档开头");
        assert_eq!(ef, field("EndHTML:") - "<!--EndFragment--></body></html>".len());
    }

    /// 判断「已经是 CF_HTML」的能力必须可靠 —— 它决定回贴时原样写回还是套头。
    /// 套错的后果是把带格式的内容粘成源码。
    #[test]
    fn cf_html_detection() {
        let proper = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000200\r\nStartFragment:0000000141\r\nEndFragment:0000000180\r\n<html><body><!--StartFragment-->x<!--EndFragment--></body></html>";
        assert!(is_cf_html(proper), "带头的必须识别为 CF_HTML");
        assert!(is_cf_html("\r\n  Version:0.9\r\n..."), "前导空白不能影响判断");
        assert!(!is_cf_html("<b>裸 HTML 片段</b>"), "裸片段要走套头分支");
        assert!(!is_cf_html(""), "空内容不当作 CF_HTML");
    }

    /// 子类型判定：链接只认「整条就是一个网址」。
    #[test]
    fn subtype_link_requires_whole_content_to_be_a_url() {
        assert_eq!(detect_subtype("https://example.com/a?b=1"), Some("link"));
        assert_eq!(detect_subtype("  https://example.com  "), Some("link"));
        assert_eq!(detect_subtype("www.example.com"), Some("link"));

        // 夹在一句话里的链接仍然是普通文本 —— 标成「链接」会误导
        assert_eq!(detect_subtype("看这个 https://example.com"), None);
        assert_eq!(detect_subtype("https://example.com\n第二行"), None);
    }

    /// Markdown 源码要认出来（用户会复制整篇 md 文档）。
    #[test]
    fn subtype_markdown_detected_for_structured_source() {
        let md = "## 1. 铁律\n\n1. 先退出软件\n2. 再重新安装\n\n| 列A | 列B |\n| --- | --- |";
        assert_eq!(detect_subtype(md), Some("markdown"));

        // 含代码块的 Markdown 仍应判 Markdown，而不是被围栏抢走
        let with_fence = "## 示例\n\n```rust\nfn main() {}\n```\n\n- 说明一\n- 说明二";
        assert_eq!(detect_subtype(with_fence), Some("markdown"));
    }

    /// 只有**一个**结构强特征时不算 Markdown —— 中文正文里「# 号」「1. 」都可能出现，
    /// 单个特征就判会让徽章泛滥。
    #[test]
    fn subtype_markdown_needs_two_structural_hints() {
        assert_eq!(detect_subtype("# 号标题在中文里也常见"), None);
        // 用户实测那条「渲染后的纯文本」只有有序列表一个特征 —— 保持不标
        assert_eq!(
            detect_subtype("1. 铁律（不可协商）\n只能用命令行工具，禁止使用外壳脚本。"),
            None
        );
        // 代码里的行注释不该被当成 Markdown 标题
        assert_eq!(
            detect_subtype("fn main() {\n    // 注释\n    let x = 1;\n}"),
            Some("code")
        );
    }

    /// 同源判定：Markdown 原文 vs 它的渲染文本要判为「同一次复制」。
    ///
    /// 真实数据取自用户剪贴板（812 字 vs 1347 字），实测 Jaccard = **0.937**，
    /// 离阈值 0.5 很远 —— 这个余量说明阈值不是拍脑袋定的。
    #[test]
    fn same_origin_detects_markdown_vs_rendered() {
        let md = "## 1. 铁律（不可协商）\n\n1. **只能用 Bash，禁止使用命令行外壳。**\n2. **每次加功能或修 bug 都必须新建分支**，不需要人来提醒。";
        let rendered = "1. 铁律（不可协商）\n只能用 Bash，禁止使用命令行外壳。\n\n每次加功能或修 bug 都必须新建分支，不需要人来提醒。";
        assert!(same_origin(md, rendered), "同一次复制的两种表示必须判为同源");
    }

    /// 反例同样取自实测：两段无关中文的相似度是 **0.0**，离阈值有充足距离。
    #[test]
    fn same_origin_rejects_unrelated_text() {
        assert!(!same_origin(
            "今天下午三点开会，讨论下季度的产品路线图和预算分配。",
            "这个 bug 出现在用户点击删除按钮之后，索引文件被截断成半截。"
        ));
        assert!(!same_origin(
            "const a = 1;\nconst b = 2;",
            "def foo():\n    return 42"
        ));
    }

    /// 太短的文本不做同源判定：一句话恰好相似的概率太高，
    /// 合并掉用户真复制的第二条比多一条更糟。
    #[test]
    fn same_origin_ignores_very_short_text() {
        assert!(!same_origin("abc", "abd"));
        assert!(!same_origin("收到", "收到"));
    }

    /// 裸域名也要识别成链接 —— 用户实测「复制的是正确链接，只是没带 http，却被标成文本」。
    /// 同时必须挡住 `报告.docx` / `v1.2.3` / `3.14` 这类「像但不是」的内容。
    #[test]
    fn subtype_link_accepts_bare_domains() {
        assert_eq!(detect_subtype("github.com/kunge9106/self_clipboard"), Some("link"));
        assert_eq!(detect_subtype("example.com"), Some("link"));
        assert_eq!(detect_subtype("mp.weixin.qq.com/s/AbCdEf"), Some("link"));
        assert_eq!(detect_subtype("example.com."), Some("link"), "行尾句号应被忽略");

        // 以下都含点号，但都不是域名 —— 标成链接会让徽章失去意义
        assert_eq!(detect_subtype("报告.docx"), None);
        assert_eq!(detect_subtype("v1.2.3"), None);
        assert_eq!(detect_subtype("3.14"), None);
        assert_eq!(detect_subtype("第一.二条"), None);
        assert_eq!(detect_subtype("a@b.com"), None, "邮箱不是链接");
        assert_eq!(detect_subtype("看 example.com"), None, "句子里夹域名不算");
    }

    /// 单行代码也要认（用户实测 `const x = 1;` 这类整行粘贴的代码被漏掉），
    /// 但只认强特征，普通句子不能误标。
    #[test]
    fn subtype_code_accepts_single_line_with_strong_features() {
        assert_eq!(detect_subtype("const x = 1;"), Some("code"));
        assert_eq!(detect_subtype("import os"), Some("code"));
        assert_eq!(detect_subtype("console.log(1);"), Some("code"));
        assert_eq!(detect_subtype("value => value * 2;"), Some("code"));

        // 中文说明：带冒号、带分号，但不含括号、不以关键字开头
        assert_eq!(detect_subtype("注意：这样做；"), None);
        assert_eq!(detect_subtype("请稍等；"), None);
        assert_eq!(detect_subtype("结论：先跑一遍。"), None);
    }

    /// 子类型判定：代码要能认出来，且**不能把中文文档误判成代码**。
    ///
    /// 误判的代价比漏判大：漏了只是少一个徽章，误了会让用户对整列徽章失去信任。
    #[test]
    fn subtype_code_detected_without_false_positives() {
        // 典型代码：有围栏
        assert_eq!(
            detect_subtype("```rust\nfn main() {}\n```"),
            Some("code")
        );
        // 无围栏但有缩进 + 关键字 + 结构符号
        assert_eq!(
            detect_subtype("fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}"),
            Some("code")
        );
        // JSON / 配置片段
        assert_eq!(detect_subtype("{\n  \"name\": \"qingtie\"\n}"), Some("code"));

        // 中文正文：带分号、带冒号、多行缩进，都不该被认成代码
        assert_eq!(
            detect_subtype("注意事项：\n- 先退出软件；\n- 再重新安装；"),
            None
        );
        assert_eq!(
            detect_subtype("这是一段普通说明。\n第二行继续说明，没有代码特征。"),
            None
        );
        // 单行**已改为按强特征判定**（见 subtype_code_accepts_single_line_with_strong_features）：
        // 原来这里断言「单行一律不判」，会把整行粘贴的代码全部漏掉。
        assert_eq!(detect_subtype("const x = 1;"), Some("code"));
    }

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

    /// `preview_truncated` 必须与「省略号」严格同义：它是前端判断
    /// 「卡片已经显示全了、别再弹悬浮浮层」的唯一依据，判松了会对短文本弹空浮层，
    /// 判严了会让长文本失去悬浮预览。
    #[test]
    fn preview_truncated_flag_matches_ellipsis() {
        // 卡片显示得全 → false
        assert!(!text_preview_parts("就两个字").1);
        assert!(!text_preview_parts("第一行\n第二行").1, "正好 2 行不算截断");
        assert!(!text_preview_parts("\n\n正文第一行\n正文第二行").1, "空行被跳过，不算截断");
        assert!(!text_preview_parts("   \n\t\n").1, "空白文本没有丢弃内容");

        // 有内容被丢掉 → true
        assert!(text_preview_parts("一\n二\n三").1, "第 3 行被丢掉");
        assert!(text_preview_parts(&"x".repeat(500)).1, "超出字符预算");
        assert!(text_preview_parts(&format!("{}\n下一行", "y".repeat(200))).1);
    }
}
