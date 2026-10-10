//! 剪贴板监听 / 读取 / 存储 / 写回。
//!
//! 设计红线（DESIGN.md 6.1 / 6.3.1）：
//!   - 事件驱动：仅响应 `WM_CLIPBOARDUPDATE`，绝不轮询
//!   - 廉价去重：`GetClipboardSequenceNumber` 判重 + 上一条指纹
//!   - 内容永不常驻内存：index 只存元数据，长文本/图片按 id 落盘分片

use std::fs;
use std::path::PathBuf;
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

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: String,
    pub kind: String, // "text" | "image" | "files"
    pub ts: u64,
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

#[tauri::command]
pub fn toggle_pin(id: String) -> Result<(), String> {
    patch_item(&id, |i| i.pinned = !i.pinned)
}

#[tauri::command]
pub fn toggle_favorite(id: String) -> Result<(), String> {
    patch_item(&id, |i| i.favorite = !i.favorite)
}

#[tauri::command]
pub fn clear_all() -> Result<(), String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    for item in hist.iter() {
        remove_item_files(&dir, item);
    }
    hist.clear();
    save_index(&dir, &hist);
    Ok(())
}

#[tauri::command]
pub fn get_image(id: String) -> Result<String, String> {
    let dir = DIR.get().cloned().ok_or("存储目录未初始化")?;
    let hist = HISTORY.get().unwrap().lock().unwrap();
    let item = hist.iter().find(|i| i.id == id).ok_or("条目不存在")?;
    let rel = item.img_path.as_ref().ok_or("该条目不是图片")?;
    let path = dir.join(rel);
    drop(hist);
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let b64 = base64_encode(&bytes);
    Ok(format!("data:image/bmp;base64,{}", b64))
}

/// 把历史条目内容写回系统剪贴板（文本 + 文件列表；图片暂仅展示）
#[tauri::command]
pub fn copy_back(id: String) -> Result<(), String> {
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
            unsafe { write_text_to_clipboard(&text) }
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
            unsafe { write_files_to_clipboard(&files) }
        }
        "image" => {
            // BMP 文件去掉 14 字节文件头即为 DIB，直接放回 CF_DIB
            let rel = item.img_path.ok_or("图片文件缺失")?;
            let bmp = fs::read(dir.join(rel)).map_err(|e| e.to_string())?;
            if bmp.len() < 54 || &bmp[0..2] != b"BM" {
                return Err("图片数据损坏".into());
            }
            unsafe { write_dib_to_clipboard(&bmp[14..]) }
        }
        _ => Err("未知类型".into()),
    }
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
    println!("[clipboard] 批量删除 {removed} 条");
    Ok(())
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

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
        println!("[clipboard] 已为 {backfilled} 条历史文件条目补写 files/ 清单");
        save_index(&dir, &hist);
    }

    // 保留策略：按配置天数清理过期条目（0 = 永久保留）
    let days = crate::settings::get().retention_days;
    let hist = prune_history(hist, &dir, days);

    let _ = HISTORY.set(Mutex::new(hist));

    // 订阅剪贴板格式监听：此后系统剪贴板变化会向我们窗口发 WM_CLIPBOARDUPDATE
    unsafe {
        if AddClipboardFormatListener(MAIN_HWND.get().copied().unwrap_or(0)) == 0 {
            eprintln!("[clipboard] AddClipboardFormatListener 失败");
        } else {
            println!("[clipboard] 已订阅 WM_CLIPBOARDUPDATE（事件驱动监听）");
        }
    }

    println!(
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
    if let Some(raw) = raw {
        let item = store_raw(raw);
        if let Some(app) = APP.get() {
            let _ = app.emit("clipboard-update", item.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// 读取剪贴板
// ---------------------------------------------------------------------------

enum RawClip {
    Text(String),
    Image(Vec<u8>), // DIB 字节
    Files(Vec<String>),
}

unsafe fn read_clipboard() -> Option<RawClip> {
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_HDROP: u32 = 15;

    // 「文件复制」开关（设置页，默认关闭）：关掉后完全不看 CF_HDROP。
    // 在 OpenClipboard 之前读取，避免持有剪贴板锁时再去碰 settings 的互斥锁。
    let file_copy = crate::settings::get().file_copy;

    if OpenClipboard(0) == 0 {
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

        if file_copy && IsClipboardFormatAvailable(CF_HDROP) != 0 {
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
                return Some(RawClip::Files(files));
            }
        }

        if IsClipboardFormatAvailable(CF_DIB) != 0 {
            if let Some(h) = non_null(GetClipboardData(CF_DIB)) {
                let size = GlobalSize(h);
                let p = GlobalLock(h);
                if !p.is_null() && size > 0 {
                    let bytes = std::slice::from_raw_parts(p as *const u8, size as usize).to_vec();
                    GlobalUnlock(h);
                    return Some(RawClip::Image(bytes));
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

fn store_raw(raw: RawClip) -> HistoryItem {
    let dir = DIR.get().unwrap().clone();
    let ts = now_ms();
    let id = make_id(ts);
    let mut item = HistoryItem {
        id: id.clone(),
        kind: String::new(),
        ts,
        pinned: false,
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
    }

    let mut hist = HISTORY.get().unwrap().lock().unwrap();
    hist.insert(0, item.clone());
    save_index(&dir, &hist);
    println!(
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
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("[clipboard] index.json 解析失败：{e}，返回空");
            Vec::new()
        }),
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
pub fn prune_retention(days: u32) {
    if let Some(dir) = DIR.get() {
        let mut hist = HISTORY.get().unwrap().lock().unwrap();
        let before = hist.len();
        *hist = prune_history(hist.clone(), dir, days);
        let removed = before - hist.len();
        if removed > 0 {
            println!("[clipboard] 保留策略清理 {removed} 条过期条目");
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

#[tauri::command]
pub fn cache_info() -> CacheInfo {
    let dir = DIR.get().cloned().unwrap_or_default();
    let (count, size) = dir_stats(&dir);
    CacheInfo {
        dir: strip_ext_prefix(&dir.to_string_lossy()),
        count,
        size,
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
pub async fn pick_cache_dir(app: AppHandle) -> Result<Option<String>, String> {
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
    // 不做 canonicalize（会引入 \\?\ 前缀），仅清理可能存在的扩展前缀
    let new_dir_str = strip_ext_prefix(&new_dir.to_string_lossy());

    if let Some(old) = DIR.get() {
        if old.to_string_lossy() == new_dir_str {
            return Ok(Some(new_dir_str));
        }
        move_dir_contents(old, &new_dir).map_err(|e| format!("迁移缓存文件失败：{e}"))?;
    }

    crate::settings::update(|c| c.cache_dir = Some(new_dir_str.clone()));
    println!("[clipboard] 缓存目录已迁移至 {new_dir_str}，即将重启");

    // DIR 是 OnceLock，运行期不可变：迁移完成后重启进程，让新目录在下次启动生效
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        handle.restart();
    });
    Ok(Some(new_dir_str))
}

/// 递归把 old 内所有内容移动到 new（跨卷也适用：rename 失败退回 copy+delete）
fn move_dir_contents(old: &PathBuf, new: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(new)?;
    for entry in fs::read_dir(old)?.flatten() {
        let from = entry.path();
        let to = new.join(entry.file_name());
        if from.is_dir() {
            move_dir_contents(&from, &to)?;
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

// ---------------------------------------------------------------------------
// 退出清理（迁移自 v1 cleanImagesOnClose：清空内容缓存 + 历史）
// ---------------------------------------------------------------------------

/// 在 RunEvent::Exit 时调用。clean_on_close 开启则清空全部内容文件与历史索引。
pub fn on_app_exit() {
    let cfg = crate::settings::get();
    if !cfg.clean_on_close {
        return;
    }
    let include_pinned = cfg.clean_include_pinned;
    println!(
        "[clipboard] 关闭时清理缓存（include_pinned={}）",
        include_pinned
    );
    if let Some(dir) = DIR.get() {
        if include_pinned {
            // 全清：直接清空三个内容目录 + 历史
            for sub in ["data", "images", "files"] {
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
                if i.pinned || i.favorite {
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
