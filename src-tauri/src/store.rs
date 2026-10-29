//! 索引存储层：v2 快照（AES-256-GCM）+ 旧格式一次性迁移。
//!
//! 为什么改格式：`index.json` 原来是「整份 DPAPI 加密的 JSON」。DPAPI 的耗时不像
//! 随体积线性（实测 4.7 MB 207 ms、38 MB 226 ms），更像有一笔 200 ms 量级的固定开销，
//! 于是条目过万之后每一次复制、置顶、删除都要卡 200 ms 以上。改成「密钥由 DPAPI 保护、
//! 数据由 AES-256-GCM 保护」之后，写盘路径上不再有系统调用级的加密开销，
//! 38 MB 从 344 ms 降到 100 ms 量级。
//!
//! 三个文件名各司其职：
//! - `index.bin`：v2 快照，**唯一**的当前索引。写它永远是 tmp + `sync_all` + rename。
//! - `index.v1.json`：迁移前的旧索引，改名保留不删（回滚到旧版本时它是唯一可读的索引）。
//! - `index.bin.corrupt` / `index.nokey.bin`：读不出来时把现场原样留下再重建。
//!   「就地覆盖成一份看起来正常的小索引」会让事故现场彻底消失 —— 用户踩过这个坑。
//!
//! 迁移的可中断性靠**顺序**保证：先写成功 v2 快照，再改旧文件名。任一刻磁盘上
//! 至少有一份完整索引；写到一半被杀时 `index.bin` 根本不存在（tmp 还没 rename），
//! 下次启动照旧从旧文件重新迁移一遍。反过来，如果 `index.bin` 读不出来而旧文件还在，
//! 也是回退到旧文件重迁，而不是去重建（重建会丢置顶与收藏）。
//!
//! 本模块不做 WAL、不做条数上限，那两块是下一步。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::clipboard::{decode_text_bytes, HistoryItem};
use crate::crypto::{self, CryptoError, Format, Key};

/// v2 快照。
pub const INDEX_BIN: &str = "index.bin";
const INDEX_BIN_TMP: &str = "index.bin.tmp";
/// 旧格式索引（v1 = 整份 DPAPI，v0 = 明文 JSON）。迁移后不再写它。
pub const INDEX_LEGACY: &str = "index.json";
/// 迁移完成时旧索引的去处。
pub const INDEX_LEGACY_KEPT: &str = "index.v1.json";
/// v2 快照校验失败时的现场。
pub const INDEX_CORRUPT: &str = "index.bin.corrupt";
/// 密钥对不上时（key.bin 丢了 / 换了账户）v2 快照的去处。
pub const INDEX_NOKEY: &str = "index.nokey.bin";
/// 旧索引读不出来时的现场（沿用旧文件名，方便用户认得）。
const LEGACY_CORRUPT: &str = "index.json.corrupt";

/// 索引存储格式版本，写进 `settings.storage_version`。
pub const STORAGE_VERSION: u32 = 2;

/// 单条 `files` 在内存里的路径上限。
///
/// 到 T02b（把常驻列表换成 `Vec<Meta>`）才真正生效；现在只有单测与 `Meta` 用它。
///
/// 一次复制上千个文件时，把全部路径常驻内存会让「元数据」变成「内容」；搜索与卡片
/// 预览只覆盖前 500 个已经够用，完整清单始终在 `files/<id>.txt` 里，回贴时读它。
#[cfg_attr(not(test), allow(dead_code))]
pub const FILES_RESIDENT_MAX: usize = 500;

// ---------------------------------------------------------------------------
// 载入结果
// ---------------------------------------------------------------------------

/// 这次载入走的是哪条路。调用方据此决定「要不要补写一份 v2 快照」与「要不要重建」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStatus {
    /// 正常读到 v2 快照。
    V2,
    /// 没有任何索引文件（首次运行，或用户删过）。历史为空是正常状态。
    Missing,
    /// 从旧格式（整份 DPAPI）读出，内存里的那份是对的，但磁盘上还没有 v2 快照。
    MigratedV1,
    /// 从旧格式（明文 JSON）读出。
    MigratedV0,
    /// 索引读不出来（密钥不对 / 校验失败 / 结构损坏），文件已改名保留，
    /// 调用方应从内容实体重建。
    NeedsRebuild,
}

impl LoadStatus {
    /// 是否需要在本次启动结束时补写一份 v2 快照。
    ///
    /// `Missing` 也要写：首次运行就该把 `index.bin` 建起来，否则下一次启动仍然
    /// 走「没有索引」的分支，永远等不到第一份快照。
    pub fn needs_snapshot(self) -> bool {
        !matches!(self, LoadStatus::V2)
    }
}

/// `load_index` 的返回：条目 + 走的哪条路。
pub struct Loaded {
    pub items: Vec<Meta>,
    pub status: LoadStatus,
    /// 日志里有需要固化的东西：有未整理的帧、尾帧被截断、或中间帧损坏。
    /// 调用方应在**后台**做一次 compaction（不要卡启动）。
    pub needs_compaction: bool,
}

// ---------------------------------------------------------------------------
// Meta：内存形态
// ---------------------------------------------------------------------------

/// 「路径按 id 推导」的三种状态。
///
/// 为什么不是 `Option<String>`：`None` 无法区分「这条本来就没有实体」和「有实体、
/// 路径是标准形态、所以不用存」。前者（老索引里短文本只有 preview）如果被当成后者，
/// 回贴会去读一个不存在的 `data/<id>.txt`，正文就变成「读取失败」。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RelPath {
    /// 与 `data/<id>.txt` 这类标准形态逐字相同 → 不占内存，用的时候算出来。
    Canonical,
    /// 没有这个实体。
    Absent,
    /// 存量里存在非常规路径（早期版本/手工迁移/用户改过目录名）→ 原样保留。
    Custom(String),
}

#[cfg_attr(not(test), allow(dead_code))]
impl RelPath {
    fn of(actual: &Option<String>, canon: String) -> RelPath {
        match actual {
            None => RelPath::Absent,
            Some(p) if *p == canon => RelPath::Canonical,
            Some(p) => RelPath::Custom(p.clone()),
        }
    }

    fn resolve(&self, canon: impl FnOnce() -> String) -> Option<String> {
        match self {
            RelPath::Canonical => Some(canon()),
            RelPath::Absent => None,
            RelPath::Custom(p) => Some(p.clone()),
        }
    }

    fn is_custom(&self) -> bool {
        matches!(self, RelPath::Custom(_))
    }
}

/// 一条记录的**元数据**（内存形态）。
///
/// 只保留渲染与搜索需要的字段。五个路径里四个由 id 推导（`data/<id>.txt` /
/// `data/<id>.html` / `data/<id>.rtf` / `files/<id>.txt`），剩下两个推不出来：
/// `img_path` 带日期目录与来源目录（`images/shot/20261028_223610.bmp`），
/// `rich_images` 的文件名由 HTML 里的图片顺序决定，推导要 `read_dir`。
///
/// 与磁盘形态（`HistoryItem`）之间只允许存在 `from_item` / `to_item` 这一对转换，
/// **磁盘格式不做破坏性变更**：仍然是完整的 `HistoryItem` JSON，旧版本仍能读。
// `Meta` 是 T02b 的常驻形态。T02a 里它只有单测在用（生产路径仍是 HistoryItem 直写，
// 免得每次落盘多做一次全量深拷贝），所以这里显式允许暂时没人调用。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub id: String,
    pub ts: u64,
    pub kind: String,
    pub preview: String,
    pub preview_truncated: bool,
    pub size: u64,
    pub pinned_views: Vec<String>,
    pub favorite: bool,
    pub edited: bool,
    pub subtype: Option<String>,
    pub shot: bool,
    /// 图片路径带日期/来源目录，推不出来 → 常驻。
    pub img_path: Option<String>,
    /// 富文本内联图（`images/rich/<id>/<n>.<ext>`），文件名不定 → 常驻。
    pub rich_images: Vec<String>,
    /// 文件路径列表，最多 [`FILES_RESIDENT_MAX`] 条。
    pub files: Option<Vec<String>>,
    /// `files` 是否被截断过。截断时完整清单只在 `files/<id>.txt` 里。
    pub files_overflow: bool,
    text_path: RelPath,
    html_path: RelPath,
    rtf_path: RelPath,
    files_path: RelPath,
}

/// 正文实体路径。
pub fn text_rel(id: &str) -> String {
    format!("data/{id}.txt")
}
/// 富文本（CF_HTML）实体路径。
pub fn html_rel(id: &str) -> String {
    format!("data/{id}.html")
}
/// 富文本（RTF）实体路径。
pub fn rtf_rel(id: &str) -> String {
    format!("data/{id}.rtf")
}
/// 文件清单实体路径。
pub fn files_rel(id: &str) -> String {
    format!("files/{id}.txt")
}
/// 内联图目录（相对路径，带末尾分隔符）。
pub fn rich_dir_rel(id: &str) -> String {
    format!("images/rich/{id}/")
}

#[cfg_attr(not(test), allow(dead_code))]
impl Meta {
    pub fn from_item(item: &HistoryItem) -> Meta {
        let (files, files_overflow) = match &item.files {
            Some(v) if v.len() > FILES_RESIDENT_MAX => {
                (Some(v[..FILES_RESIDENT_MAX].to_vec()), true)
            }
            other => (other.clone(), false),
        };
        Meta {
            id: item.id.clone(),
            ts: item.ts,
            kind: item.kind.clone(),
            preview: item.preview.clone(),
            preview_truncated: item.preview_truncated,
            size: item.size,
            pinned_views: item.pinned_views.clone(),
            favorite: item.favorite,
            edited: item.edited,
            subtype: item.subtype.clone(),
            shot: item.shot,
            img_path: item.img_path.clone(),
            rich_images: item.rich_images.clone(),
            files,
            files_overflow,
            text_path: RelPath::of(&item.text_path, text_rel(&item.id)),
            html_path: RelPath::of(&item.html_path, html_rel(&item.id)),
            rtf_path: RelPath::of(&item.rtf_path, rtf_rel(&item.id)),
            files_path: RelPath::of(&item.files_path, files_rel(&item.id)),
        }
    }

    pub fn to_item(&self) -> HistoryItem {
        HistoryItem {
            id: self.id.clone(),
            kind: self.kind.clone(),
            ts: self.ts,
            // 已废弃的兼容字段：载入时已迁移进 pinned_views，写出去恒 false
            pinned: false,
            pinned_views: self.pinned_views.clone(),
            favorite: self.favorite,
            edited: self.edited,
            subtype: self.subtype.clone(),
            preview: self.preview.clone(),
            preview_truncated: self.preview_truncated,
            size: self.size,
            shot: self.shot,
            text_path: self.text_rel(),
            html_path: self.html_rel(),
            rtf_path: self.rtf_rel(),
            rich_images: self.rich_images.clone(),
            img_path: self.img_path.clone(),
            files_path: self.files_rel(),
            // 截断过的列表**不往磁盘写**：写出去就是把用户的后 500+ 条路径抹掉，
            // 而完整清单本来就在 files/<id>.txt 里（读者以它为准）。
            files: if self.files_overflow {
                None
            } else {
                self.files.clone()
            },
        }
    }

    pub fn text_rel(&self) -> Option<String> {
        self.text_path.resolve(|| text_rel(&self.id))
    }

    pub fn html_rel(&self) -> Option<String> {
        self.html_path.resolve(|| html_rel(&self.id))
    }

    pub fn rtf_rel(&self) -> Option<String> {
        self.rtf_path.resolve(|| rtf_rel(&self.id))
    }

    pub fn files_rel(&self) -> Option<String> {
        self.files_path.resolve(|| files_rel(&self.id))
    }

    pub fn set_text_path(&mut self, v: Option<String>) {
        self.text_path = RelPath::of(&v, text_rel(&self.id));
    }

    pub fn set_html_path(&mut self, v: Option<String>) {
        self.html_path = RelPath::of(&v, html_rel(&self.id));
    }

    pub fn set_rtf_path(&mut self, v: Option<String>) {
        self.rtf_path = RelPath::of(&v, rtf_rel(&self.id));
    }

    pub fn set_files_path(&mut self, v: Option<String>) {
        self.files_path = RelPath::of(&v, files_rel(&self.id));
    }

    /// 是否保留了非常规路径（诊断用：正常数据应当恒为 false）。
    pub fn has_custom_path(&self) -> bool {
        self.text_path.is_custom()
            || self.html_path.is_custom()
            || self.rtf_path.is_custom()
            || self.files_path.is_custom()
    }
}

/// 按 `ts` 倒序把一条记录插进列表。
///
/// 老代码是 `insert(0, item)`，隐含「新条目的 ts 一定最大」。系统时钟回拨（NTP 校时、
/// 用户改时间）时这个假设不成立，列表顺序就会与真实时间不符；后面的分页依赖
/// 「ts 严格降序」这条不变量做切片，顺序一乱就会出现重复或漏条。
/// 二分插入在 99.99% 的情况下命中下标 0（与老行为一致），只有回拨时才插到中间。
///
/// 同 `ts` 的条目插在最前面：id 是 `hex(ts)-counter`，同时在毫秒内 counter 递增，
/// 所以「新的在前」就是「同 ts 时新的先插入」。
pub fn insert_ordered(items: &mut Vec<Meta>, item: Meta) -> usize {
    let at = items.partition_point(|i| i.ts > item.ts);
    items.insert(at, item);
    at
}

// ---------------------------------------------------------------------------
// 密钥与写序号
// ---------------------------------------------------------------------------

/// 进程内只解一次密钥。值是 `(目录, 密钥)`：目录变了（用户迁移缓存目录后重启）
/// 就重新解一次，避免把 A 目录的密钥拿去解 B 目录的数据。
static KEY_CACHE: Mutex<Option<(PathBuf, Option<Key>)>> = Mutex::new(None);

/// 拿这个目录的密钥。`None` 表示密钥当前不可用（DPAPI 解不开 / 生成失败）。
///
/// **不在这里重建密钥**：`crypto::load_or_create_key` 只在文件缺失时生成，
/// 存在但解不开时返回错误；把「出错就换一把」写进来等于把用户数据变成永久乱码。
pub fn key_for(dir: &Path) -> Option<Key> {
    let mut guard = match KEY_CACHE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((cached_dir, key)) = guard.as_ref() {
        if cached_dir.as_path() == dir {
            return key.clone();
        }
    }
    let key = match crypto::load_or_create_key(dir) {
        Ok(k) => {
            crate::log_print!(
                "[store] 密钥就绪：key_id={:016x} created={}",
                k.key_id(),
                k.created_ms()
            );
            Some(k)
        }
        Err(e) => {
            crate::log_err!(
                "store",
                "密钥不可用（{}）：本次运行无法读写加密索引，历史仍可读取但不会落盘",
                e
            );
            None
        }
    };
    *guard = Some((dir.to_path_buf(), key.clone()));
    key
}

// ---------------------------------------------------------------------------
// 保存
// ---------------------------------------------------------------------------

/// 保存失败的两类原因。**分开**是因为调用方对它们的反应完全不同：
/// 密钥不可用只能记 ERROR（换不了钥匙），IO 错误要按老口径重试/告警。
#[derive(Debug)]
pub enum SaveError {
    /// 密钥不可用（`key.bin` 解不开、DPAPI 异常）。
    NoKey,
    /// 序列化 / 写盘 / 改名失败。
    Io(io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::NoKey => f.write_str("密钥不可用"),
            SaveError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        SaveError::Io(e)
    }
}

/// 把元数据写成 v2 快照。
///
/// 与 [`save_items`] 共用同一套序列化与落盘逻辑，区别只在于要不要先做一次
/// `Meta → HistoryItem` 投影。T02b 把常驻列表换成 `Vec<Meta>` 之后走这条。
#[cfg_attr(not(test), allow(dead_code))]
pub fn save_snapshot(dir: &Path, metas: &[Meta], key: &Key, write_seq: u64) -> io::Result<()> {
    let items: Vec<HistoryItem> = metas.iter().map(Meta::to_item).collect();
    write_snapshot(dir, &items, key, write_seq)
}

fn write_snapshot(dir: &Path, items: &[HistoryItem], key: &Key, write_seq: u64) -> io::Result<()> {
    // 紧凑 JSON：`to_string_pretty` 多 22% 体积、多 7 ms，而这份东西加密之后
    // 没人会去人眼阅读 —— 唯一的读者是 serde。留明文副本用于排障的诉求由
    // `index.json.corrupt` / `index.nokey.bin` 这类「现场文件」满足。
    let json = serde_json::to_string(items)
        .map_err(|e| io::Error::other(format!("索引序列化失败：{e}")))?;
    let sealed = crypto::seal(key, crypto::DOMAIN_INDEX, None, json.as_bytes(), write_seq);

    let tmp = dir.join(INDEX_BIN_TMP);
    // 临时文件 → sync_all → rename。直接覆盖写是非原子的：写入途中被强杀就会留下
    // 半截索引，而半截索引等于丢历史（用户实测过一次：列表只剩 4 条）。
    let write = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&sealed)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, dir.join(INDEX_BIN)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 写一份整份快照，并把日志里已被它覆盖的部分截掉。
///
/// 顺序不可颠倒：
///   ① 取检查点（当前日志长度 + 内存已应用到哪个序号）；
///   ② 写快照，头里的 `write_seq` = 检查点序号；
///   ③ 只截掉日志的 `0..检查点长度`。
///
/// 第②步与第③步之间被强杀是安全的：那些帧的序号 ≤ 快照头里的序号，重放时会被跳过
/// （否则一个已经进了快照的 `clear` 会被再放一遍，把整份历史抹掉）。
/// 第③步用「重写尾部 + rename」而不是原地截断：整理期间新追加的帧在 `0..` 之后，
/// 原样留着。
pub fn persist(dir: &Path, metas: &[Meta]) -> Result<(), SaveError> {
    let key = key_for(dir).ok_or(SaveError::NoKey)?;
    let cp = checkpoint(dir)?;
    // 常驻是 Meta，磁盘形态仍是完整 HistoryItem：投影在这里发生一次。
    save_snapshot(dir, metas, &key, cp.seq)?;
    truncate_wal_prefix(dir, cp.wal_len)?;
    finish_migration(dir, metas.len(), cp.seq);
    Ok(())
}

/// 后台整理：语义与 [`persist`] 完全一致，只是个说得清用途的名字。
pub fn compact(dir: &Path, metas: &[Meta]) -> Result<(), SaveError> {
    persist(dir, metas)
}

/// 快照已经写到磁盘之后才做的事：把旧索引改名保留、记下 `storage_version`。
///
/// 顺序不可颠倒 —— 旧文件必须在新快照完整落盘之后才改名，否则迁移中途被杀
/// 就两份都没有了。
fn finish_migration(dir: &Path, count: usize, write_seq: u64) {
    let legacy = dir.join(INDEX_LEGACY);
    if !legacy.is_file() {
        if crate::settings::get().storage_version < STORAGE_VERSION {
            crate::settings::update(|c| c.storage_version = STORAGE_VERSION);
        }
        return;
    }
    let kept = dir.join(INDEX_LEGACY_KEPT);
    // 只留一份旧索引：上一份 `index.v1.json` 与当前 `index.json` 是同一次迁移的
    // 两个世代，后者才是「迁移前那一刻」，前者已经是过期副本。
    if kept.exists() {
        let _ = fs::remove_file(&kept);
    }
    match fs::rename(&legacy, &kept) {
        Ok(()) => {
            let bytes = fs::metadata(&kept).map(|m| m.len()).unwrap_or(0);
            crate::settings::update(|c| c.storage_version = STORAGE_VERSION);
            crate::log_print!(
                "[store] 索引格式 v1 → v2 迁移完成：{count} 条，{bytes} 字节，旧索引保留为 {INDEX_LEGACY_KEPT}，write_seq={write_seq}"
            );
        }
        // 改名失败不算致命：新快照已经落地，下次启动还会再试一次。
        Err(e) => crate::log_err!(
            "store",
            "旧索引改名失败（{INDEX_LEGACY} → {INDEX_LEGACY_KEPT}）：{e}；不影响新快照使用"
        ),
    }
}

// ---------------------------------------------------------------------------
// 载入
// ---------------------------------------------------------------------------

/// 读索引。按「v2 快照 → 旧格式 → 没有」的顺序，然后重放日志。
///
/// 任何一种失败都不 panic，也不静默生成新密钥。
pub fn load_index(dir: &Path) -> Loaded {
    // 上一次写到一半被杀的残留：`index.bin` 还没被 rename 出来，只有这个临时文件。
    // 它永远不该被当成数据源，先清掉再继续（清不掉也不影响后面的判断）。
    let _ = fs::remove_file(dir.join(INDEX_BIN_TMP));
    let base = load_base(dir);
    // 磁盘形态（HistoryItem）→ 常驻形态（Meta）：整条加载链只在这里投影一次，
    // 之后的重放、修复、IPC 投影都基于 Meta。
    let mut items: Vec<Meta> = base.items.iter().map(Meta::from_item).collect();
    let snapshot_seq = base.seq;
    // 重放：只应用「快照还没覆盖」的帧。日志里共几帧、有没有坏帧由它一并回报。
    let force_compact = replay_wal(dir, &mut items, snapshot_seq);
    let pending_frames = wal_len(dir) > 0;
    Loaded {
        items,
        status: base.status,
        needs_compaction: force_compact || pending_frames,
    }
}

/// 载入的基础状态（不碰日志）。
struct Base {
    items: Vec<HistoryItem>,
    status: LoadStatus,
    /// 快照已覆盖到的 op 序号；旧格式/没有索引时是 0（= 日志全部要重放）。
    seq: u64,
}

fn load_base(dir: &Path) -> Base {
    match fs::read(dir.join(INDEX_BIN)) {
        Ok(raw) => match read_snapshot(dir, &raw) {
            Ok((items, seq)) => {
                crate::log_print!(
                    "[store] 索引加载成功（v2）：{} 条，{} 字节，已覆盖 op#{seq}",
                    items.len(),
                    raw.len()
                );
                Base {
                    items: pin_migrate(items),
                    status: LoadStatus::V2,
                    seq,
                }
            }
            Err(err) => {
                // v2 读不出来时先看旧索引还在不在：在的话回退到旧索引重新迁移，
                // 那里面有置顶与收藏；从内容实体重建会把它们丢掉。
                let dst = if err.is_key() {
                    INDEX_NOKEY
                } else {
                    INDEX_CORRUPT
                };
                keep(dir, INDEX_BIN, &raw, err.why(), dst);
                if dir.join(INDEX_LEGACY).is_file() {
                    crate::log_warn!(
                        "[store] v2 快照不可用（{}），但旧索引还在 → 从旧索引重新迁移",
                        err.why()
                    );
                    return load_legacy(dir);
                }
                crate::log_err!(
                    "store",
                    "v2 快照不可用（{}）：已另存为 {dst}，索引将从内容实体重建",
                    err.why()
                );
                Base {
                    items: Vec::new(),
                    status: LoadStatus::NeedsRebuild,
                    seq: 0,
                }
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => load_legacy(dir),
        Err(e) => {
            crate::log_err!("store", "索引读取失败（{INDEX_BIN}）：{e}");
            Base {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
                seq: 0,
            }
        }
    }
}

/// 解一份 v2 快照失败的原因。**分开密钥类与损坏类**：前者要留 `index.nokey.bin`
/// （用户能据此判断「是钥匙丢了，不是数据坏了」），后者留 `index.bin.corrupt`。
enum SnapshotError {
    Key(String),
    Corrupt(String),
}

impl SnapshotError {
    fn why(&self) -> &str {
        match self {
            SnapshotError::Key(s) | SnapshotError::Corrupt(s) => s,
        }
    }

    fn is_key(&self) -> bool {
        matches!(self, SnapshotError::Key(_))
    }
}

/// 解一份 v2 快照，同时取出「本快照已覆盖到哪个 op 序号」。
fn read_snapshot(dir: &Path, raw: &[u8]) -> Result<(Vec<HistoryItem>, u64), SnapshotError> {
    if raw.is_empty() {
        // 空文件是「上次写入被中断」的特征（老版本直接覆盖写的产物）。
        return Err(SnapshotError::Corrupt("文件是 0 字节".to_string()));
    }
    let key = match key_for(dir) {
        Some(k) => k,
        // 密钥解不开（换账户/换机器）与「内容被改」是两回事。
        None => {
            return Err(SnapshotError::Key(
                "密钥不可用（换了 Windows 账户或机器？）".to_string(),
            ))
        }
    };
    match crypto::open(&key, crypto::DOMAIN_INDEX, None, raw) {
        Ok(plain) => {
            let json = String::from_utf8(plain).map_err(|_| {
                SnapshotError::Corrupt("解密结果不是 UTF-8（索引已损坏）".to_string())
            })?;
            let items = serde_json::from_str(&json)
                .map_err(|e| SnapshotError::Corrupt(format!("解析失败：{e}")))?;
            // 序号是 AAD 的一部分，能解到这里说明它没被改过。
            let seq = crypto::container_write_seq(raw).unwrap_or(0);
            Ok((items, seq))
        }
        Err(CryptoError::WrongKey) => Err(SnapshotError::Key(
            "容器用的是另一把密钥（key.bin 被删或换过？）".to_string(),
        )),
        Err(CryptoError::Tampered) => Err(SnapshotError::Corrupt(
            "校验失败（内容被改动或位翻转）".to_string(),
        )),
        Err(CryptoError::Malformed) => Err(SnapshotError::Corrupt(
            "容器结构损坏（长度或字段不合法）".to_string(),
        )),
        Err(CryptoError::NotV2) => Err(SnapshotError::Corrupt("不是 v2 容器".to_string())),
    }
}

/// 读旧格式索引（v1 = 整份 DPAPI，v0 = 明文 JSON）。都没有则返回空 + `Missing`。
fn load_legacy(dir: &Path) -> Base {
    let path = dir.join(INDEX_LEGACY);
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                crate::log_warn!(
                    "[store] 没有索引文件（首次运行，或索引已被删除/改名）—— 历史列表为空"
                );
            } else {
                crate::log_err!("store", "{INDEX_LEGACY} 读取失败：{e}");
            }
            return Base {
                items: Vec::new(),
                status: LoadStatus::Missing,
                seq: 0,
            };
        }
    };
    if raw.is_empty() {
        crate::log_err!(
            "store",
            "{INDEX_LEGACY} 是空文件（0 字节）—— 上次写入被中断（强杀/崩溃/断电）的特征，历史列表将为空"
        );
        keep(dir, INDEX_LEGACY, &raw, "空文件", LEGACY_CORRUPT);
        return Base {
            items: Vec::new(),
            status: LoadStatus::NeedsRebuild,
            seq: 0,
        };
    }

    let format = crypto::detect(&raw);
    let json = match format {
        Format::V1Dpapi => match decode_text_bytes(&raw) {
            Some(s) => s,
            None => {
                // 与今天 `clipboard.rs` 同一口径：换账户/换机器时 DPAPI 解不开。
                keep(
                    dir,
                    INDEX_LEGACY,
                    &raw,
                    "解密失败（可能换了 Windows 账户或机器）",
                    LEGACY_CORRUPT,
                );
                crate::log_err!(
                    "store",
                    "旧索引解密失败：换 Windows 账户或机器后旧数据不可恢复；本次从内容实体重建（置顶与收藏会丢）"
                );
                return Base {
                    items: Vec::new(),
                    status: LoadStatus::NeedsRebuild,
                    seq: 0,
                };
            }
        },
        Format::V0Plaintext => String::from_utf8_lossy(&raw).to_string(),
        // 认不出来：按损坏处理，留现场再重建。
        Format::V2 => {
            // 理论上不会发生（v2 分支在上面处理过）。真发生了说明 index.json 里
            // 装的是一份 v2 容器，按损坏处理即可，不用特殊兼容。
            keep(dir, INDEX_LEGACY, &raw, "是 v2 容器", LEGACY_CORRUPT);
            return Base {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
                seq: 0,
            };
        }
        Format::Unknown => {
            keep(dir, INDEX_LEGACY, &raw, "格式无法识别", LEGACY_CORRUPT);
            return Base {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
                seq: 0,
            };
        }
    };

    match serde_json::from_str::<Vec<HistoryItem>>(&json) {
        Ok(items) => {
            let status = if format == Format::V1Dpapi {
                LoadStatus::MigratedV1
            } else {
                LoadStatus::MigratedV0
            };
            crate::log_print!(
                "[store] 旧索引加载成功（{}）：{} 条，{} 字节 → 待迁移为 v2",
                if status == LoadStatus::MigratedV1 {
                    "v1 DPAPI"
                } else {
                    "v0 明文"
                },
                items.len(),
                raw.len()
            );
            Base {
                items: pin_migrate(items),
                status,
                // 旧格式没有 op 序号：日志里的帧全部要重放
                seq: 0,
            }
        }
        Err(e) => {
            keep(
                dir,
                INDEX_LEGACY,
                &raw,
                &format!("解析失败：{e}"),
                LEGACY_CORRUPT,
            );
            Base {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
                seq: 0,
            }
        }
    }
}

/// 旧版的**全局**置顶（`pinned: bool`）迁移成「历史」分类的置顶。
///
/// 历史是默认视图，用户当初置顶的内容在历史里仍然浮顶，行为不变。
/// 幂等：已经有 `"history"` 的条目不再重复加。
fn pin_migrate(mut items: Vec<HistoryItem>) -> Vec<HistoryItem> {
    let mut migrated = 0usize;
    for i in items.iter_mut() {
        if i.pinned && !i.pinned_views.iter().any(|x| x == "history") {
            i.pinned_views.push("history".to_string());
            migrated += 1;
        }
    }
    if migrated > 0 {
        crate::log_print!("[store] 迁移 {migrated} 条旧版全局置顶 → 历史分类置顶");
    }
    items
}

/// 把读不出来的索引原样留一份，再让调用方决定重建。
///
/// 为什么必须留：不清空、不覆盖，事后才查得出是截断、解密失败还是解析失败。
/// 用户实测那次「列表只剩 4 条」就是靠现场文件才定位的。
fn keep(dir: &Path, from: &str, raw: &[u8], why: &str, to: &str) {
    let dst = dir.join(to);
    match fs::write(&dst, raw) {
        Ok(()) => crate::log_err!(
            "store",
            "{from} {why}；损坏文件 {} 字节已另存为 {to}",
            raw.len()
        ),
        Err(e) => crate::log_err!("store", "{from} {why}，且备份失败：{e}"),
    }
}

// ---------------------------------------------------------------------------
// 追加日志（index.wal）
// ---------------------------------------------------------------------------
//
// 为什么还要 WAL：快照是**整份**写的 —— 复制一条就要序列化 + 加密 + 写盘整套重来，
// 10 万条 150 ms。而剪贴板工具最不能犯的错是「刚复制的那条没记住」。WAL 把一次变更
// 变成一次 ~500 B 的追加 + `sync_data`（实测 0.3–1 ms），同时保住「不丢」。
// 整份快照退到后台（compaction）与退出前做。
//
// 帧格式（36 B 头 + 密文）：
//   0  2  魔数 "W1"
//   2  2  保留 0
//   4  4  payload_len（u32 LE，密文字节数）
//   8  12 nonce
//   20 16 tag
//   36 N  密文（AES-256-GCM，AAD = 头[0..8] ‖ DOMAIN_WAL）
//
// AAD 覆盖魔数、保留位与长度字段：改长度字段就会校验失败，没法「把 payload_len
// 改小、让后面某帧被当成新帧」。nonce 与 tag **不进** AAD —— 前者是加密的入参
// （拼 AAD 时它还没生成），后者是加密的产物；改 nonce 本身就会让 tag 不过。
//
// 为什么每个 op 都带一个**单调序号**（seq，在密文里）：
// 快照头里的 `write_seq` 记的就是「本快照已覆盖到的最后 op 序号」。重放时只看
// `seq > snapshot.write_seq` 的帧。这样一来「快照写完了、WAL 还没截断就被强杀」
// 是安全的：那些已经进了快照的帧（包括 `clear`）会被跳过，不会被重放一遍。
// 没有序号时只能靠「截断一定成功」，而截断恰恰是崩溃窗口里最容易没做成的那一步。

/// 追加日志的文件名。
pub const WAL_FILE: &str = "index.wal";
const WAL_TMP: &str = "index.wal.tmp";
/// 中间帧坏掉时整个日志的去处（留现场，不静默跳过）。
pub const WAL_CORRUPT: &str = "index.wal.corrupt";

const WAL_MAGIC: &[u8] = b"W1";
/// 参与 AAD 的帧头前缀：魔数 2 + 保留 2 + 长度 4。
///
/// **不含 nonce 与 tag**：nonce 是加密的入参（由 `crypto::seal_detached` 现场生成，
/// 拼 AAD 时还不知道），tag 是加密的产物。nonce 不需要另外进 AAD：它是 IV，改它 tag 必然不过。
const WAL_AAD_LEN: usize = 2 + 2 + 4;
const WAL_HEADER_LEN: usize = WAL_AAD_LEN + 12 + 16; // 前缀 + nonce + tag

/// 帧数超过这个值就整理一次。
pub const WAL_COMPACT_FRAMES: u64 = 1000;
/// 日志字节数超过这个值就整理一次。
pub const WAL_COMPACT_BYTES: u64 = 2 * 1024 * 1024;

/// WAL 里的一条变更。
///
/// **每条都必须幂等**：compaction 中途被强杀后整段会被重放，非幂等的 op
/// （「在末尾追加一条」这种）重放一次就会多出一条。这里的六个 op 全部是
/// 「覆盖式」语义：insert 按 id 覆盖、patch 写绝对值、pins/fav 写绝对值、
/// delete 不存在即跳过、clear 清空。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Op {
    /// 新增或整条覆盖（按 id upsert）。
    Insert { item: HistoryItem },
    /// 只改内容相关的几个字段（编辑保存 / 富文本补进来）。
    Patch { id: String, set: PatchSet },
    /// 置顶状态（绝对值）。
    Pins { id: String, views: Vec<String> },
    /// 收藏状态（绝对值）。
    Fav { id: String, on: bool },
    /// 删除（不存在即跳过）。
    Delete { id: String },
    /// 清空。
    Clear,
}

/// `Op::Patch` 的载荷：每个字段 `None` = 不改。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PatchSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtf_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rich_images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<bool>,
}

impl PatchSet {
    fn apply(&self, meta: &mut Meta) {
        if let Some(v) = &self.preview {
            meta.preview = v.clone();
        }
        if let Some(v) = self.preview_truncated {
            meta.preview_truncated = v;
        }
        if let Some(v) = self.size {
            meta.size = v;
        }
        if let Some(v) = &self.subtype {
            meta.subtype = Some(v.clone());
        }
        // 路径走 setter：常驻形态里存的是「标准 / 没有 / 非常规」三态，
        // 直接写字符串会绕过推导规则
        if let Some(v) = &self.text_path {
            meta.set_text_path(Some(v.clone()));
        }
        if let Some(v) = &self.html_path {
            meta.set_html_path(Some(v.clone()));
        }
        if let Some(v) = &self.rtf_path {
            meta.set_rtf_path(Some(v.clone()));
        }
        if let Some(v) = &self.rich_images {
            meta.rich_images = v.clone();
        }
        if let Some(v) = self.edited {
            meta.edited = v;
        }
    }
}

/// 帧里的明文：`{"s":序号,"op":{...}}`。
#[derive(Serialize, Deserialize)]
struct Frame {
    s: u64,
    op: Op,
}

/// 把一条变更写进日志。`Ok(())` 意味着**已经过了 `sync_data`**，可以对外声称落盘了。
pub fn append(dir: &Path, op: Op) -> Result<(), SaveError> {
    append_many(dir, std::slice::from_ref(&op))
}

/// 一批变更一次写完、一次 `sync_data`。
///
/// 批量删除一次选中几百条时，逐条 `sync_data` 就是几百次 fsync（每次 ~2 ms）——
/// 那是秒级的界面卡顿。一次写完再 sync 一遍，语义不变（要么整批落盘、要么整批没有），
/// 而且「整批原子」本来就比「一半落盘」更符合用户预期。
pub fn append_many(dir: &Path, ops: &[Op]) -> Result<(), SaveError> {
    if ops.is_empty() {
        return Ok(());
    }
    let key = key_for(dir).ok_or(SaveError::NoKey)?;
    let mut guard = lock_wal();
    ensure_wal_dir(&mut guard, dir, &key)?;

    let mut buf: Vec<u8> = Vec::with_capacity(ops.len() * 512);
    let mut seq = guard.last_seq;
    for op in ops {
        seq += 1;
        buf.extend_from_slice(&encode_frame(&key, seq, op)?);
    }

    let file = guard
        .file
        .as_mut()
        .ok_or_else(|| SaveError::Io(io::Error::other("追加日志句柄不可用")))?;
    file.write_all(&buf)?;
    // WAL 的全部价值就在这里：不 sync 就只是「内存里多了一段字节」，
    // 断电/强杀后那条刚复制的记录会消失。
    file.sync_data()?;

    guard.len += buf.len() as u64;
    guard.frames += ops.len() as u64;
    guard.last_seq = seq;
    Ok(())
}

/// 日志是否已经该整理了（帧数或字节数到了上限）。
pub fn wal_needs_compact(dir: &Path) -> bool {
    let Some(key) = key_for(dir) else {
        return false;
    };
    let mut guard = lock_wal();
    if ensure_wal_dir(&mut guard, dir, &key).is_err() {
        return false;
    }
    guard.frames > WAL_COMPACT_FRAMES || guard.len > WAL_COMPACT_BYTES
}

/// 当前日志的字节数（诊断与基准用）。
pub fn wal_len(dir: &Path) -> u64 {
    fs::metadata(dir.join(WAL_FILE))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn encode_frame(key: &Key, seq: u64, op: &Op) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(&Frame {
        s: seq,
        op: op.clone(),
    })
    .map_err(|e| io::Error::other(format!("WAL 序列化失败：{e}")))?;
    let len =
        u32::try_from(payload.len()).map_err(|_| io::Error::other("单条 WAL 载荷超过 4 GB"))?;

    let mut header = [0u8; WAL_HEADER_LEN];
    header[0..2].copy_from_slice(WAL_MAGIC);
    header[2..4].copy_from_slice(&[0, 0]);
    header[4..8].copy_from_slice(&len.to_le_bytes());

    let mut aad = Vec::with_capacity(WAL_AAD_LEN + crypto::DOMAIN_WAL.len());
    aad.extend_from_slice(&header[..WAL_AAD_LEN]);

    let (nonce, tag, ciphertext) = crypto::seal_detached(key, &aad_with_domain(&aad), &payload);
    header[8..20].copy_from_slice(&nonce);
    header[20..36].copy_from_slice(&tag);

    let mut out = Vec::with_capacity(WAL_HEADER_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 帧的 AAD = 头[0..8] ‖ 域串。
fn aad_with_domain(header_prefix: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(header_prefix.len() + crypto::DOMAIN_WAL.len());
    aad.extend_from_slice(header_prefix);
    aad.extend_from_slice(crypto::DOMAIN_WAL);
    aad
}

/// 一次扫描的结果。
enum WalIssue {
    Clean,
    /// 尾部有半帧 / 残帧：`0..valid_len` 是完好的部分。
    Tail {
        valid_len: usize,
    },
    /// 中间帧坏了：整份日志不可信。
    Mid(String),
}

/// 扫一遍日志字节，对每个完整帧回调 `(seq, op)`。
///
/// 返回 `(issue, valid_len, max_seq, frames)`。**不修改任何状态** —— 是否应用、
/// 应用哪些（`seq > 快照序号`）由调用方决定，这样「重放」与「只数帧」共用一份解析。
fn scan_wal(raw: &[u8], key: &Key, mut on_op: impl FnMut(u64, Op)) -> (WalIssue, usize, u64, u64) {
    let mut off = 0usize;
    let mut max_seq = 0u64;
    let mut frames = 0u64;
    while off < raw.len() {
        let rest = raw.len() - off;
        // 不足一个头：可能是「上一帧写到一半就被杀」，也可能是文件尾巴上的垃圾。
        // 只要它是**最后**那个不完整块，就当尾帧处理（截断到 off）。
        if rest < WAL_HEADER_LEN {
            return (WalIssue::Tail { valid_len: off }, off, max_seq, frames);
        }
        let header = &raw[off..off + WAL_HEADER_LEN];
        if &header[0..2] != WAL_MAGIC {
            return (
                WalIssue::Mid(format!("偏移 {off} 处魔数不匹配")),
                off,
                max_seq,
                frames,
            );
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let body_start = off + WAL_HEADER_LEN;
        if raw.len() < body_start + len {
            // 头是完整的、密文不够长：写入被中断在密文中间。这是最常见的断电形态。
            return (WalIssue::Tail { valid_len: off }, off, max_seq, frames);
        }
        let nonce = &header[8..20];
        let tag = &header[20..36];
        let ciphertext = &raw[body_start..body_start + len];
        let aad = aad_with_domain(&header[..WAL_AAD_LEN]);
        let plain = match crypto::open_detached(key, &aad, nonce, tag, ciphertext) {
            Ok(v) => v,
            Err(_) => {
                return (
                    WalIssue::Mid(format!("偏移 {off} 处校验失败（tag 不过）")),
                    off,
                    max_seq,
                    frames,
                )
            }
        };
        let frame: Frame = match serde_json::from_slice(&plain) {
            Ok(f) => f,
            Err(e) => {
                return (
                    WalIssue::Mid(format!("偏移 {off} 处载荷解析失败：{e}")),
                    off,
                    max_seq,
                    frames,
                )
            }
        };
        max_seq = max_seq.max(frame.s);
        frames += 1;
        on_op(frame.s, frame.op);
        off = body_start + len;
    }
    (WalIssue::Clean, off, max_seq, frames)
}

/// 把所有 op 应用到内存列表。**幂等**：同样的 op 再来一次结果不变。
fn apply_op(items: &mut Vec<Meta>, op: Op) {
    match op {
        Op::Insert { item } => {
            // 帧里装的是完整 HistoryItem（磁盘与回滚兼容），落到内存时投影一次。
            // 投影只有 `Meta::from_item` 这一份实现，别处不重复搬字段。
            let meta = Meta::from_item(&item);
            // upsert：同 id 就地替换（保持顺序），否则按 ts 插入正确位置
            match items.iter_mut().find(|i| i.id == meta.id) {
                Some(slot) => *slot = meta,
                None => {
                    insert_ordered(items, meta);
                }
            }
        }
        Op::Patch { id, set } => {
            if let Some(i) = items.iter_mut().find(|i| i.id == id) {
                set.apply(i);
            }
        }
        Op::Pins { id, views } => {
            if let Some(i) = items.iter_mut().find(|i| i.id == id) {
                i.pinned_views = views;
            }
        }
        Op::Fav { id, on } => {
            if let Some(i) = items.iter_mut().find(|i| i.id == id) {
                i.favorite = on;
            }
        }
        Op::Delete { id } => items.retain(|i| i.id != id),
        Op::Clear => items.clear(),
    }
}

/// 进程内的日志状态：文件句柄、长度、帧数、已用到哪个序号。
///
/// 句柄常驻是刻意的：一次复制只做一次 `write + sync_data`，不重新 open/close。
/// 所有对日志文件的写、截断、改名都在这个锁里做 —— 否则 compaction 的
/// 「读尾部 → 写临时文件 → rename」会和正在进行的 append 撞车，把刚追加的帧写进
/// 一个已经被替换掉的旧句柄里，那条记录就悄悄丢了。
struct WalState {
    dir: Option<PathBuf>,
    file: Option<fs::File>,
    len: u64,
    frames: u64,
    last_seq: u64,
}

static WAL: Mutex<WalState> = Mutex::new(WalState {
    dir: None,
    file: None,
    len: 0,
    frames: 0,
    last_seq: 0,
});

fn lock_wal() -> std::sync::MutexGuard<'static, WalState> {
    match WAL.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 让日志状态指向 `dir`。换目录（配置迁移后重启、单测并行）时重新扫一遍，
/// 顺便把 `last_seq` 推到「日志里已有的最大序号」，保证新帧的序号严格递增。
fn ensure_wal_dir(guard: &mut WalState, dir: &Path, key: &Key) -> io::Result<()> {
    if guard.dir.as_deref() == Some(dir) {
        return Ok(());
    }
    let path = dir.join(WAL_FILE);
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let (issue, valid_len, max_seq, frames) = scan_wal(&raw, key, |_, _| {});
    // 尾帧残了（上次写到一半被杀）：先截掉，让文件始终「帧边界对齐」——
    // 否则新帧会接在残帧后面，下次重放会在残帧处停下，新帧永远读不到。
    let tail_broken = matches!(issue, WalIssue::Tail { .. }) && valid_len < raw.len();
    let len = if tail_broken { valid_len } else { raw.len() } as u64;
    if tail_broken {
        rewrite_wal(dir, &raw[..valid_len])?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    *guard = WalState {
        dir: Some(dir.to_path_buf()),
        file: Some(file),
        len,
        frames,
        last_seq: max_seq,
    };
    Ok(())
}

/// 把 `bytes` 整体写成新的日志文件（先临时文件再 rename，原子替换）。
fn rewrite_wal(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(WAL_TMP);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, dir.join(WAL_FILE))
}

/// compaction 开始时记下的日志位置。
pub struct Checkpoint {
    /// 日志当前长度：整理完只需要截掉 `0..wal_len`，之后追加的帧原样保留。
    wal_len: u64,
    /// 内存状态已经应用到哪个序号，写进快照头。
    seq: u64,
}

/// 取一个检查点（在写快照**之前**调用）。
pub fn checkpoint(dir: &Path) -> Result<Checkpoint, SaveError> {
    let key = key_for(dir).ok_or(SaveError::NoKey)?;
    let mut guard = lock_wal();
    ensure_wal_dir(&mut guard, dir, &key)?;
    Ok(Checkpoint {
        wal_len: guard.len,
        seq: guard.last_seq,
    })
}

/// 截掉日志的前 `n` 字节（`0..n` 已经被快照覆盖）。
///
/// 为什么是「重写尾部 + rename」而不是原地 `set_len`：原地截断只能砍尾巴，
/// 砍不掉前缀；而且 compaction 期间追加进来的新帧必须原样保留。这里在**锁内**
/// 重新读一遍文件，`n` 之后的内容就是这段时间新追加的帧。
fn truncate_wal_prefix(dir: &Path, n: u64) -> io::Result<()> {
    let mut guard = lock_wal();
    if n == 0 {
        return Ok(());
    }
    let path = dir.join(WAL_FILE);
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let from = (n as usize).min(raw.len());
    let tail = raw[from..].to_vec();
    if from == 0 {
        return Ok(());
    }
    rewrite_wal(dir, &tail)?;
    // 句柄必须重开：rename 之后旧句柄指向的是一个已经不在目录里的文件，
    // 继续往它写等于把记录写进虚空。
    guard.file = Some(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?,
    );
    guard.len = tail.len() as u64;
    guard.frames = count_frames(&tail);
    Ok(())
}

/// 数一段日志字节里有几帧（只按头走，不校验内容 —— 用于维护「帧数」这个判据，
/// 帧数偏大只会让整理提前触发，不会漏数据）。
fn count_frames(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut off = 0usize;
    let mut n = 0u64;
    while off + WAL_HEADER_LEN <= bytes.len() {
        let len = u32::from_le_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]) as usize;
        let next = off + WAL_HEADER_LEN + len;
        if next > bytes.len() {
            break;
        }
        n += 1;
        off = next;
    }
    n
}

/// 把日志里的 op 重放进来。
///
/// 只应用 `seq > snapshot_seq` 的帧：快照已经覆盖的那些（包括 `clear`）跳过，
/// 所以「快照写完、WAL 还没来得及截断就被强杀」不会让状态倒退。
///
/// 返回 `true` 表示建议立刻整理（尾帧被截断 / 中间帧坏掉）。
fn replay_wal(dir: &Path, items: &mut Vec<Meta>, snapshot_seq: u64) -> bool {
    let path = dir.join(WAL_FILE);
    let raw = match fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 没有日志是正常状态（刚整理完）。顺手把状态归零。
            let mut guard = lock_wal();
            guard.dir = Some(dir.to_path_buf());
            guard.file = None;
            guard.len = 0;
            guard.frames = 0;
            guard.last_seq = snapshot_seq;
            return false;
        }
        Err(e) => {
            crate::log_err!("store", "{WAL_FILE} 读取失败：{e}；本次只加载快照");
            return false;
        }
    };
    let Some(key) = key_for(dir) else {
        crate::log_err!(
            "store",
            "密钥不可用：{WAL_FILE} 无法重放（{} 字节的变更未生效）",
            raw.len()
        );
        return false;
    };

    let mut applied = 0usize;
    let (issue, valid_len, max_seq, frames) = scan_wal(&raw, &key, |seq, op| {
        if seq > snapshot_seq {
            apply_op(items, op);
            applied += 1;
        }
    });

    let mut need_compact = false;
    let mut final_len = raw.len() as u64;
    match issue {
        WalIssue::Clean => {}
        WalIssue::Tail { valid_len } => {
            crate::log_warn!(
                "[store] {WAL_FILE} 尾部有 {} 字节残帧（上次写入被强杀）→ 截断到最后一条完整帧，随后立即整理",
                raw.len() - valid_len
            );
            if let Err(e) = rewrite_wal(dir, &raw[..valid_len]) {
                crate::log_err!("store", "{WAL_FILE} 截断失败：{e}");
            } else {
                final_len = valid_len as u64;
            }
            need_compact = true;
        }
        WalIssue::Mid(why) => {
            // 中间帧坏了：不是「写到一半」，是真的坏。整份日志不可信 ——
            // 留现场、报 ERROR、丢掉日志，用「快照 + 坏帧之前的完好的部分」继续。
            // 不静默跳过：跳过的是一条用户的真实操作，用户有权在日志里看到。
            keep(dir, WAL_FILE, &raw, &why, WAL_CORRUPT);
            crate::log_err!(
                "store",
                "{WAL_FILE} 中间帧损坏（{why}）→ 已另存为 {WAL_CORRUPT}，索引按快照 + 完好前缀继续；本次会重写快照"
            );
            if let Err(e) = fs::remove_file(&path) {
                crate::log_err!("store", "{WAL_FILE} 清理失败：{e}");
            }
            final_len = 0;
            need_compact = true;
        }
    }
    let _ = valid_len;

    {
        let mut guard = lock_wal();
        guard.dir = Some(dir.to_path_buf());
        guard.file = None;
        guard.len = final_len;
        guard.frames = if final_len == 0 { 0 } else { frames };
        guard.last_seq = max_seq.max(snapshot_seq);
    }
    if applied > 0 {
        crate::log_print!(
            "[store] {WAL_FILE} 重放 {applied} 条变更（共 {frames} 帧，{} 字节）",
            raw.len()
        );
    }
    need_compact
}

/// 超出条数上限时「该删哪些」。
pub struct Overflow {
    /// 应删的 id，**从最旧开始**（列表按 ts 降序，所以是从尾部往前挑）。
    pub ids: Vec<String>,
    /// 保护条目全跳过之后仍缺的条数。`> 0` 说明清完还是超限，只能停手。
    pub shortfall: usize,
}

/// 挑出该删的条目：最旧的非置顶、非收藏。
///
/// 为什么从最旧挑：用户要找的几乎总是「最近的」；而置顶/收藏是用户明确说过
/// 「这条要留着」的 —— 自动清理碰它们等于背弃那个标记，所以宁可超限也不删。
/// `max = 0` 表示不限。
pub fn select_overflow(items: &[Meta], max: usize) -> Overflow {
    if max == 0 || items.len() <= max {
        return Overflow {
            ids: Vec::new(),
            shortfall: 0,
        };
    }
    let mut need = items.len() - max;
    let mut ids = Vec::with_capacity(need);
    for item in items.iter().rev() {
        if need == 0 {
            break;
        }
        if !item.pinned_views.is_empty() || item.favorite {
            continue;
        }
        ids.push(item.id.clone());
        need -= 1;
    }
    Overflow {
        ids,
        shortfall: need,
    }
}

/// 由一个 `HistoryItem` 的变化推出最小的 op：只改了收藏就写 `Fav`，只改了置顶就写
/// `Pins`，其余写整条（upsert）。返回 `None` 表示什么都没变，不必落盘。
pub fn diff_op(before: &Meta, after: &Meta) -> Option<Op> {
    if before == after {
        return None;
    }
    let mut probe = before.clone();
    probe.favorite = after.favorite;
    if probe == *after {
        return Some(Op::Fav {
            id: after.id.clone(),
            on: after.favorite,
        });
    }
    let mut probe = before.clone();
    probe.pinned_views = after.pinned_views.clone();
    if probe == *after {
        return Some(Op::Pins {
            id: after.id.clone(),
            views: after.pinned_views.clone(),
        });
    }
    // 其余变化（正文/预览/子类型/富文本/图片路径）写整条 upsert：帧里是完整
    // HistoryItem，这一处投影是唯一一次「Meta → 磁盘形态」的转换入口。
    Some(Op::Insert {
        item: after.to_item(),
    })
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::encrypt_text;

    /// 自建临时目录，`Drop` 时清掉。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "qingtie-store-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            fs::create_dir_all(&p).expect("创建临时目录");
            TempDir(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 模拟「重启进程」：清掉进程内的密钥缓存。
    ///
    /// 真实场景里密钥只在进程启动时解一次，删掉 `key.bin` 后必须重启才看得到影响；
    /// 单测跑在同一个进程里，只能手动把缓存清掉。
    fn forget_key_cache() {
        let mut g = KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }

    /// 磁盘形态 → 常驻形态（测试里只用一个入口投影，跟生产路径一致）。
    fn metas(items: &[HistoryItem]) -> Vec<Meta> {
        items.iter().map(Meta::from_item).collect()
    }

    fn item(id: &str, text: &str) -> HistoryItem {
        HistoryItem {
            id: id.to_string(),
            kind: "text".into(),
            ts: 1_700_000_000_000,
            pinned: false,
            pinned_views: Vec::new(),
            favorite: false,
            edited: false,
            subtype: None,
            preview: text.to_string(),
            preview_truncated: false,
            size: text.len() as u64,
            shot: false,
            text_path: Some(text_rel(id)),
            html_path: None,
            rtf_path: None,
            rich_images: Vec::new(),
            img_path: None,
            files_path: None,
            files: None,
        }
    }

    /// 迁移的样本：三种 kind + 置顶 + 收藏 + 子类型 + 富文本 + 内联图 + 文件清单。
    fn sample_items() -> Vec<HistoryItem> {
        let mut t = item("1a1b2c3d4e5-1", "一条带富文本的正文");
        t.subtype = Some("code".into());
        t.pinned_views = vec!["history".into(), "text".into()];
        t.favorite = true;
        t.edited = true;
        t.preview_truncated = true;
        t.html_path = Some(html_rel("1a1b2c3d4e5-1"));
        t.rtf_path = Some(rtf_rel("1a1b2c3d4e5-1"));
        t.rich_images = vec!["images/rich/1a1b2c3d4e5-1/0.png".into()];

        let mut f = item("1a1b2c3d4e6-2", "");
        f.kind = "files".into();
        f.text_path = None;
        f.preview = "a.txt 等 2 个文件".into();
        f.files_path = Some(files_rel("1a1b2c3d4e6-2"));
        f.files = Some(vec!["C:/a.txt".into(), "C:/b.txt".into()]);

        let mut i = item("1a1b2c3d4e7-3", "");
        i.kind = "image".into();
        i.text_path = None;
        i.preview = "[图片]".into();
        i.img_path = Some("images/shot/20261028_223610.bmp".into());
        i.shot = true;

        vec![t, f, i]
    }

    /// 迁移保语义（条数/置顶/收藏/子类型/preview 截断标记），且连跑 3 次幂等。
    #[test]
    fn migrate_keeps_semantics_and_is_idempotent() {
        let dir = TempDir::new("migrate");
        let sample = sample_items();
        let json = serde_json::to_string_pretty(&sample).unwrap();
        // v1 索引 = DPAPI 整份加密，带 ENC_MAGIC
        fs::write(dir.path().join(INDEX_LEGACY), encrypt_text(&json)).unwrap();

        // 第一次：从 v1 迁移
        let first = load_index(dir.path());
        assert_eq!(first.status, LoadStatus::MigratedV1);
        assert_eq!(first.items.len(), 3);
        assert_eq!(first.items[0].pinned_views, vec!["history", "text"]);
        assert!(first.items[0].favorite);
        assert!(first.items[0].edited);
        assert_eq!(first.items[0].subtype.as_deref(), Some("code"));
        assert!(first.items[0].preview_truncated);
        assert_eq!(first.items[0].rich_images.len(), 1);
        assert_eq!(first.items[1].files.as_ref().unwrap().len(), 2);
        assert_eq!(
            first.items[2].img_path.as_deref(),
            Some("images/shot/20261028_223610.bmp")
        );
        assert!(first.status.needs_snapshot());

        // 落盘：写 v2 快照 + 旧文件改名保留
        persist(dir.path(), &first.items).expect("迁移应能落盘");
        assert!(dir.path().join(INDEX_BIN).is_file());
        assert!(dir.path().join(INDEX_LEGACY_KEPT).is_file());
        assert!(!dir.path().join(INDEX_LEGACY).exists(), "旧索引应被改名");

        // 第二、三次：直接读 v2，不再迁移，内容逐字段一致
        for round in 0..2 {
            let again = load_index(dir.path());
            assert_eq!(again.status, LoadStatus::V2, "第 {round} 次应直接读 v2");
            assert!(!again.status.needs_snapshot());
            assert_eq!(again.items.len(), 3);
            for (a, b) in again.items.iter().zip(metas(&sample).iter()) {
                assert_eq!(a.id, b.id);
                assert_eq!(a.kind, b.kind);
                assert_eq!(a.ts, b.ts);
                assert_eq!(a.preview, b.preview);
                assert_eq!(a.preview_truncated, b.preview_truncated);
                assert_eq!(a.size, b.size);
                assert_eq!(a.pinned_views, b.pinned_views);
                assert_eq!(a.favorite, b.favorite);
                assert_eq!(a.edited, b.edited);
                assert_eq!(a.subtype, b.subtype);
                assert_eq!(a.shot, b.shot);
                assert_eq!(a.text_rel(), b.text_rel());
                assert_eq!(a.html_rel(), b.html_rel());
                assert_eq!(a.rtf_rel(), b.rtf_rel());
                assert_eq!(a.rich_images, b.rich_images);
                assert_eq!(a.img_path, b.img_path);
                assert_eq!(a.files_rel(), b.files_rel());
                assert_eq!(a.files, b.files);
            }
        }
    }

    /// v0（明文 JSON）索引也能迁移。
    #[test]
    fn migrate_v0_plaintext() {
        let dir = TempDir::new("v0");
        let sample = sample_items();
        fs::write(
            dir.path().join(INDEX_LEGACY),
            serde_json::to_string_pretty(&sample).unwrap(),
        )
        .unwrap();

        let loaded = load_index(dir.path());
        assert_eq!(loaded.status, LoadStatus::MigratedV0);
        assert_eq!(loaded.items.len(), 3);
        persist(dir.path(), &loaded.items).unwrap();

        let again = load_index(dir.path());
        assert_eq!(again.status, LoadStatus::V2);
        assert_eq!(again.items.len(), 3);
        assert!(dir.path().join(INDEX_LEGACY_KEPT).is_file());
    }

    /// 迁移中途被强杀：旧文件还在、新快照只写了一半 → 下次启动仍能成功迁移。
    #[test]
    fn interrupted_migration_reenters() {
        let dir = TempDir::new("interrupted");
        let sample = sample_items();
        fs::write(
            dir.path().join(INDEX_LEGACY),
            encrypt_text(&serde_json::to_string(&sample).unwrap()),
        )
        .unwrap();
        // ① 上次写到一半：只剩一个垃圾临时文件，index.bin 根本没被 rename 出来
        fs::write(dir.path().join(INDEX_BIN_TMP), b"half written").unwrap();

        let first = load_index(dir.path());
        assert_eq!(first.status, LoadStatus::MigratedV1);
        assert_eq!(first.items.len(), 3);
        assert!(
            !dir.path().join(INDEX_BIN_TMP).exists(),
            "残留临时文件应被清掉"
        );
        persist(dir.path(), &first.items).unwrap();
        assert!(dir.path().join(INDEX_BIN).is_file());
        assert!(dir.path().join(INDEX_LEGACY_KEPT).is_file());

        // ② 更糟的形态：rename 之后被系统丢了半截数据，index.bin 存在但读不出来，
        //    同时旧索引还没来得及改名 → 必须回退到旧索引重迁，而不是重建
        let dir2 = TempDir::new("interrupted2");
        fs::write(
            dir2.path().join(INDEX_LEGACY),
            encrypt_text(&serde_json::to_string(&sample).unwrap()),
        )
        .unwrap();
        let mut torn = crypto::seal(
            &key_for(dir2.path()).expect("密钥应可用"),
            crypto::DOMAIN_INDEX,
            None,
            br#"[]"#,
            1,
        );
        torn.truncate(torn.len() / 2);
        fs::write(dir2.path().join(INDEX_BIN), &torn).unwrap();

        let fallback = load_index(dir2.path());
        assert_eq!(
            fallback.status,
            LoadStatus::MigratedV1,
            "v2 读不出来且旧索引还在时应回退重迁"
        );
        assert_eq!(fallback.items.len(), 3);
        assert!(dir2.path().join(INDEX_CORRUPT).is_file(), "坏文件要留现场");
        persist(dir2.path(), &fallback.items).unwrap();
        assert_eq!(load_index(dir2.path()).status, LoadStatus::V2);
    }

    /// 删掉 `key.bin`：生成新密钥 → 旧快照变 WrongKey → 改名保留 → 从实体重建。
    /// **不能**覆盖或二次生成密钥。
    #[test]
    fn missing_key_keeps_snapshot_and_rebuilds() {
        let dir = TempDir::new("nokey");
        let sample = sample_items();
        persist(dir.path(), &metas(&sample)).unwrap();
        let old_key = key_for(dir.path()).expect("密钥应可用");
        let old_id = old_key.key_id();

        // 模拟用户删掉 key.bin（或清理工具误删）后重启：清缓存 = 重启
        fs::remove_file(crypto::key_path(dir.path())).expect("删 key.bin");
        forget_key_cache();

        let loaded = load_index(dir.path());
        assert_eq!(loaded.status, LoadStatus::NeedsRebuild);
        assert!(loaded.items.is_empty());
        assert!(dir.path().join(INDEX_NOKEY).is_file(), "旧快照应改名保留");
        assert!(crypto::key_path(dir.path()).is_file(), "应生成新密钥");

        let new_key = key_for(dir.path()).expect("新密钥应可用");
        assert_ne!(new_key.key_id(), old_id, "必须是一把新钥匙");

        // 从内容实体重建：这里没有实体文件，重建结果为 0 条，但要能落盘
        let mut items = loaded.items;
        crate::clipboard::rebuild_index_from_files(dir.path(), &mut items);
        persist(dir.path(), &items).unwrap();
        assert_eq!(load_index(dir.path()).status, LoadStatus::V2);
    }

    /// 索引被翻一个字节：判 Tampered → 改名 `.corrupt` → 重建，不 panic。
    #[test]
    fn tampered_snapshot_is_kept_and_rebuilds() {
        let dir = TempDir::new("tamper");
        persist(dir.path(), &metas(&sample_items())).unwrap();
        let mut raw = fs::read(dir.path().join(INDEX_BIN)).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        fs::write(dir.path().join(INDEX_BIN), &raw).unwrap();

        let loaded = load_index(dir.path());
        assert_eq!(loaded.status, LoadStatus::NeedsRebuild);
        assert!(loaded.items.is_empty(), "坏索引不能交出去半截数据");
        let kept = fs::read(dir.path().join(INDEX_CORRUPT)).expect("现场文件应存在");
        assert_eq!(kept, raw, "现场文件必须是原样字节");
    }

    /// Meta 往返：五个路径字段逐字段等价，且标准路径真的没有被常驻。
    #[test]
    fn meta_roundtrip_matches_history_item() {
        for item in sample_items() {
            let meta = Meta::from_item(&item);
            assert!(!meta.has_custom_path(), "标准路径不该留下兜底副本");
            let back = meta.to_item();
            assert_eq!(back, item, "Meta 往返必须逐字段等价：{}", item.id);
        }

        // 路径推导确实生效：text 条目的标准路径没有常驻，但取出来是标准值
        let t = &sample_items()[0];
        let meta = Meta::from_item(t);
        assert_eq!(meta.text_rel(), Some(text_rel(&t.id)));
        assert_eq!(meta.html_rel(), Some(html_rel(&t.id)));
        assert_eq!(meta.rtf_rel(), Some(rtf_rel(&t.id)));
        assert_eq!(meta.files_rel(), None, "文本条目没有文件清单");
        assert_eq!(meta.to_item().text_path, t.text_path);

        // 非常规路径必须原样保留（早期版本/手工迁移留下的存量数据）
        let mut odd = item("1a1b2c3d4e5-9", "奇怪的路径");
        odd.text_path = Some("data/legacy-2025/old.txt".into());
        let meta = Meta::from_item(&odd);
        assert!(meta.has_custom_path());
        assert_eq!(meta.text_rel().as_deref(), Some("data/legacy-2025/old.txt"));
        assert_eq!(meta.to_item().text_path, odd.text_path);
    }

    /// `files` 超过 500 条：内存只留前 500 并打标记，磁盘形态不写截断后的列表。
    #[test]
    fn files_over_limit_is_truncated_and_marked() {
        let mut f = item("1a1b2c3d4e6-1", "");
        f.kind = "files".into();
        f.text_path = None;
        f.files_path = Some(files_rel("1a1b2c3d4e6-1"));
        f.files = Some((0..600).map(|i| format!("C:/dir/file{i}.txt")).collect());

        let meta = Meta::from_item(&f);
        assert_eq!(meta.files.as_ref().unwrap().len(), FILES_RESIDENT_MAX);
        assert!(meta.files_overflow, "截断必须打标记");
        assert_eq!(meta.files.as_ref().unwrap()[0], "C:/dir/file0.txt");
        assert_eq!(
            meta.files.as_ref().unwrap()[FILES_RESIDENT_MAX - 1],
            "C:/dir/file499.txt"
        );

        // 写回磁盘形态时**不能**把截断后的 500 条当完整清单写出去 ——
        // 完整清单在 files/<id>.txt 里，磁盘上那一份交给实体文件当唯一数据源。
        let back = meta.to_item();
        assert!(back.files.is_none(), "截断时磁盘形态不应写 files");
        assert_eq!(back.files_path, f.files_path);

        // 恰好 500 条不算截断
        let mut exact = f.clone();
        exact.files = Some((0..500).map(|i| format!("C:/dir/file{i}.txt")).collect());
        let meta = Meta::from_item(&exact);
        assert!(!meta.files_overflow);
        assert_eq!(meta.to_item().files.as_ref().unwrap().len(), 500);
    }

    /// 时钟回拨：按 ts 插入后整体仍严格降序，新条目按真实时间落到正确位置。
    #[test]
    fn clock_rollback_keeps_order() {
        let mut list: Vec<Meta> = Vec::new();
        for (id, ts) in [("a-1", 2000u64), ("b-2", 1900), ("c-3", 1800)] {
            let mut it = item(id, "x");
            it.ts = ts;
            insert_ordered(&mut list, Meta::from_item(&it));
        }
        // 正常情况（新条目 ts 最大）：插在最前，与老的 insert(0) 一致
        assert_eq!(
            list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a-1", "b-2", "c-3"]
        );

        // 时钟回拨：新条目 ts 落在中间 → 插到中间，整体仍严格降序
        let mut back = item("d-4", "x");
        back.ts = 1850;
        insert_ordered(&mut list, Meta::from_item(&back));
        assert_eq!(
            list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a-1", "b-2", "d-4", "c-3"]
        );
        assert!(list.windows(2).all(|w| w[0].ts >= w[1].ts));

        // 同 ts：新的排在同 ts 的最前面
        let mut same = item("e-5", "x");
        same.ts = 2000;
        insert_ordered(&mut list, Meta::from_item(&same));
        assert_eq!(list[0].id, "e-5");
        assert_eq!(list[1].id, "a-1");
    }

    // -----------------------------------------------------------------------
    // WAL
    // -----------------------------------------------------------------------

    fn op_insert(id: &str, text: &str, ts: u64) -> Op {
        let mut it = item(id, text);
        it.ts = ts;
        Op::Insert { item: it }
    }

    /// 追加 → 加载：状态与「直接对内存做同样的 op」逐条一致。
    #[test]
    fn wal_append_then_load_matches_expected_state() {
        let dir = TempDir::new("wal-basic");
        let mut a = item("a-1", "第一条");
        a.ts = 2000;
        let mut b = item("b-2", "第二条");
        b.ts = 1000;
        persist(dir.path(), &metas(&[a.clone(), b.clone()])).unwrap();
        assert_eq!(wal_len(dir.path()), 0, "整理后日志应为空");

        let new_item = {
            let mut it = item("c-3", "第三条");
            it.ts = 3000;
            it
        };
        let ops = vec![
            Op::Insert {
                item: new_item.clone(),
            },
            Op::Pins {
                id: "a-1".into(),
                views: vec!["history".into(), "text".into()],
            },
            Op::Fav {
                id: "b-2".into(),
                on: true,
            },
            Op::Patch {
                id: "a-1".into(),
                set: PatchSet {
                    preview: Some("改过的预览".into()),
                    ..Default::default()
                },
            },
            Op::Delete { id: "b-2".into() },
        ];
        for op in &ops {
            append(dir.path(), op.clone()).expect("append");
        }
        assert!(wal_len(dir.path()) > 0);

        // 期望状态：把同样的 op 应用到内存副本上（常驻形态）
        let mut expect = metas(&[a, b]);
        for op in ops {
            apply_op(&mut expect, op);
        }

        let loaded = load_index(dir.path());
        assert_eq!(loaded.items, expect, "快照 + 日志重放必须与逐条应用等价");
        // 关键字段再点名核一遍，避免上面那句「两边都用 apply_op」把 bug 抵消掉
        assert_eq!(loaded.items.len(), 2);
        assert_eq!(loaded.items[0].id, "c-3", "新条目 ts 最大，应排在最前");
        assert_eq!(loaded.items[1].id, "a-1");
        assert_eq!(loaded.items[1].pinned_views, vec!["history", "text"]);
        assert_eq!(loaded.items[1].preview, "改过的预览");
        assert!(!loaded.items.iter().any(|i| i.id == "b-2"), "删除要生效");
        assert!(loaded.needs_compaction, "日志里还有帧 → 建议整理");
    }

    /// 同一份日志连放两遍结果相同（重放幂等）。
    #[test]
    fn wal_replay_is_idempotent() {
        let dir = TempDir::new("wal-idem");
        persist(dir.path(), &[]).unwrap();
        append(dir.path(), op_insert("a-1", "一", 1000)).unwrap();
        append(
            dir.path(),
            Op::Pins {
                id: "a-1".into(),
                views: vec!["history".into()],
            },
        )
        .unwrap();
        append(dir.path(), op_insert("b-2", "二", 2000)).unwrap();

        let first = load_index(dir.path());
        let len_after_first = wal_len(dir.path());
        let second = load_index(dir.path());
        assert_eq!(first.items, second.items, "连放两遍必须一致");
        assert_eq!(first.items.len(), 2);
        assert_eq!(
            wal_len(dir.path()),
            len_after_first,
            "重放本身不改日志（只在尾帧残了才截）"
        );
    }

    /// 尾部半帧：截断到最后一条完整帧后加载成功，且截断本身幂等。
    #[test]
    fn wal_tail_truncation_recovers_and_is_idempotent() {
        let dir = TempDir::new("wal-tail");
        persist(dir.path(), &[]).unwrap();
        append(dir.path(), op_insert("a-1", "一", 1000)).unwrap();
        append(dir.path(), op_insert("b-2", "二", 2000)).unwrap();
        let good_len = wal_len(dir.path());

        // 模拟「写到一半被强杀」：文件尾巴上多了半截帧
        {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join(WAL_FILE))
                .unwrap();
            f.write_all(&[0xAB; 20]).unwrap();
        }
        assert!(wal_len(dir.path()) > good_len);

        let first = load_index(dir.path());
        assert_eq!(first.items.len(), 2, "两条完整帧都要生效");
        assert_eq!(wal_len(dir.path()), good_len, "残帧应被截掉");
        assert!(first.needs_compaction, "截断后要立刻整理");

        let second = load_index(dir.path());
        assert_eq!(second.items, first.items);
        assert_eq!(wal_len(dir.path()), good_len, "第二次加载不该再截");
    }

    /// 中间帧坏掉：留现场、不静默跳过、不 panic。
    #[test]
    fn wal_mid_frame_corruption_is_quarantined() {
        let dir = TempDir::new("wal-mid");
        persist(dir.path(), &[]).unwrap();
        append(dir.path(), op_insert("a-1", "一", 1000)).unwrap();
        append(dir.path(), op_insert("b-2", "二", 2000)).unwrap();
        append(dir.path(), op_insert("c-3", "三", 3000)).unwrap();

        // 翻掉**第二帧**密文里的一个字节：第一帧完好、第二帧起不可信
        let path = dir.path().join(WAL_FILE);
        let mut raw = fs::read(&path).unwrap();
        let first_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        let second = WAL_HEADER_LEN + first_len;
        raw[second + 40] ^= 0x01;
        fs::write(&path, &raw).unwrap();

        let loaded = load_index(dir.path());
        assert!(
            dir.path().join(WAL_CORRUPT).is_file(),
            "中间帧坏了必须留现场"
        );
        assert_eq!(
            fs::read(dir.path().join(WAL_CORRUPT)).unwrap(),
            raw,
            "现场文件必须是原样字节"
        );
        assert!(!path.exists(), "坏日志不该留在原地继续被重放");
        assert_eq!(loaded.items.len(), 1, "坏帧之前的完好前缀要保住");
        assert_eq!(loaded.items[0].id, "a-1");
        assert!(loaded.needs_compaction, "坏了要立刻重写快照");

        // 继续之后的第一次整理：快照里不再引用坏帧之后的任何 op。
        // 这是「别让同一个丢失窗口一直挂着」的落点 —— 整理完再加载，
        // b-2 / c-3 不能因为日志被截断而以任何方式复活。
        compact(dir.path(), &loaded.items).unwrap();
        assert_eq!(wal_len(dir.path()), 0, "整理后日志应清空");
        let again = load_index(dir.path());
        assert_eq!(again.items.len(), 1, "坏帧之后的 op 不得复活");
        assert!(again.items.iter().all(|m| m.id == "a-1"));
    }

    /// 整理前后状态相等；整理期间追加的帧不丢。
    #[test]
    fn compaction_preserves_state_and_keeps_concurrent_appends() {
        let dir = TempDir::new("wal-compact");
        persist(dir.path(), &[]).unwrap();
        append(dir.path(), op_insert("a-1", "一", 1000)).unwrap();
        append(dir.path(), op_insert("b-2", "二", 2000)).unwrap();

        // compaction 开始：取检查点，然后「慢慢」写快照
        let cp = checkpoint(dir.path()).unwrap();
        let state = load_index(dir.path()).items;
        assert_eq!(state.len(), 2);

        // 整理期间用户又复制了一条 —— 这条必须在整理完仍然存在
        append(dir.path(), op_insert("c-3", "整理期间进来的", 3000)).unwrap();
        let wal_after_append = wal_len(dir.path());
        assert!(wal_after_append > cp.wal_len);

        let key = key_for(dir.path()).unwrap();
        save_snapshot(dir.path(), &state, &key, cp.seq).unwrap();
        truncate_wal_prefix(dir.path(), cp.wal_len).unwrap();

        // 快照覆盖了 a/b，日志里只剩「整理期间」那一条
        assert!(wal_len(dir.path()) > 0, "并发的帧不能被截掉");
        let loaded = load_index(dir.path());
        assert_eq!(loaded.items.len(), 3, "整理不能丢掉并发追加的条目");
        assert!(loaded.items.iter().any(|i| i.id == "c-3"));

        // 完整走一遍 compact 之后：状态不变、日志清空
        compact(dir.path(), &loaded.items).unwrap();
        assert_eq!(wal_len(dir.path()), 0);
        let after = load_index(dir.path());
        assert_eq!(after.items, loaded.items, "整理前后状态必须逐条相等");
    }

    /// `clear` 进了快照之后不能被重放「复活」条目。
    #[test]
    fn clear_in_snapshot_is_not_replayed() {
        let dir = TempDir::new("wal-clear");
        let mut a = item("a-1", "一");
        a.ts = 1000;
        persist(dir.path(), &metas(&[a])).unwrap();
        assert_eq!(load_index(dir.path()).items.len(), 1);

        // clear 之后又来一条新的
        append(dir.path(), Op::Clear).unwrap();
        append(dir.path(), op_insert("b-2", "清空后新增", 2000)).unwrap();
        let after_clear = load_index(dir.path()).items;
        assert_eq!(after_clear.len(), 1);
        assert_eq!(after_clear[0].id, "b-2", "clear 之后只剩新增那条");

        // 模拟「快照已包含 clear，但 WAL 还没来得及截断就被强杀」：
        // 直接用当前状态写快照（write_seq = 1，正好覆盖 clear 那一帧），日志原样留着
        let key = key_for(dir.path()).unwrap();
        save_snapshot(dir.path(), &after_clear, &key, 1).unwrap();
        assert!(wal_len(dir.path()) > 0, "日志仍在（模拟截断失败）");

        let loaded = load_index(dir.path());
        assert_eq!(
            loaded.items, after_clear,
            "已进快照的 clear 被重放会把历史抹掉 —— 必须按序号跳过"
        );
    }

    /// 同 id 连续 insert / delete / insert 的重放结果正确。
    #[test]
    fn insert_delete_insert_same_id() {
        let dir = TempDir::new("wal-same-id");
        persist(dir.path(), &[]).unwrap();
        append(dir.path(), op_insert("x-1", "第一版", 1000)).unwrap();
        append(dir.path(), Op::Delete { id: "x-1".into() }).unwrap();
        append(dir.path(), op_insert("x-1", "第二版", 1000)).unwrap();

        let loaded = load_index(dir.path());
        assert_eq!(loaded.items.len(), 1, "同一个 id 只能有一条");
        assert_eq!(loaded.items[0].preview, "第二版", "后写入的覆盖先写入的");

        // 同一份日志再放一遍：结果不变（upsert 幂等）
        let again = load_index(dir.path());
        assert_eq!(again.items, loaded.items);
    }

    /// 一批 op 一次 fsync：结果与逐条追加等价（批量删除走这条）。
    #[test]
    fn batch_append_matches_sequential() {
        let dir = TempDir::new("wal-batch");
        persist(dir.path(), &[]).unwrap();
        let ops = vec![
            op_insert("a-1", "一", 1000),
            op_insert("b-2", "二", 2000),
            op_insert("c-3", "三", 3000),
        ];
        append_many(dir.path(), &ops).expect("批量追加");

        let mut expect: Vec<Meta> = Vec::new();
        for op in ops.clone() {
            apply_op(&mut expect, op);
        }
        let loaded = load_index(dir.path());
        assert_eq!(loaded.items, expect);
        assert_eq!(loaded.items.len(), 3);

        // 序号必须仍然严格递增：再来一条要接在批量之后
        append(dir.path(), Op::Delete { id: "b-2".into() }).unwrap();
        let again = load_index(dir.path());
        assert_eq!(again.items.len(), 2);
        assert!(!again.items.iter().any(|i| i.id == "b-2"));
    }

    /// 真实形态的 v0 索引（明文 JSON）必须能迁移：这是升级用户的必经路径。
    ///
    /// 用一条**逐字段写全**的样例（含 pinnedViews / files / shot / subtype / 各种
    /// 路径组合），宁可与真实数据重复也不要漏字段 —— 老版本写出来的 JSON 就是长这样。
    #[test]
    fn migrate_v0_realistic_index() {
        let dir = TempDir::new("v0-real");
        let json = r#"[
          {"id":"18bcfe56fa2-1","kind":"text","ts":1730000000000,"pinnedViews":["history"],
           "favorite":false,"edited":false,"subtype":"code",
           "preview":"第 0 条剪贴板内容","preview_truncated":true,"size":120,"shot":false,
           "text_path":"data/18bcfe56fa2-1.txt","html_path":null,"rtf_path":null,
           "rich_images":[],"img_path":null,"files_path":null,"files":null},
          {"id":"18bcfe56fa3-1","kind":"files","ts":1730000001000,"pinnedViews":[],
           "favorite":true,"edited":false,"preview":"报告.pdf 等 3 个文件",
           "preview_truncated":true,"size":120,"shot":false,
           "text_path":null,"img_path":null,"files_path":"files/18bcfe56fa3-1.txt",
           "files":["C:/a.pdf","C:/b.pdf","C:/c.pdf"]},
          {"id":"18bcfe56fa4-1","kind":"image","ts":1730000002000,"pinnedViews":[],
           "favorite":false,"edited":false,"preview":"[图片]","preview_truncated":true,
           "size":120,"shot":false,"text_path":null,"img_path":null}
        ]"#;
        fs::write(dir.path().join(INDEX_LEGACY), json).unwrap();

        let loaded = load_index(dir.path());
        assert_eq!(loaded.status, LoadStatus::MigratedV0);
        assert_eq!(loaded.items.len(), 3, "三条都要在");
        assert_eq!(loaded.items[0].pinned_views, vec!["history"]);
        assert_eq!(loaded.items[1].files.as_ref().unwrap().len(), 3);
        assert!(loaded.items[1].favorite);
        assert_eq!(loaded.items[2].kind, "image");

        // 迁移落盘后再读一次：v2
        persist(dir.path(), &loaded.items).unwrap();
        assert_eq!(load_index(dir.path()).status, LoadStatus::V2);
        assert_eq!(load_index(dir.path()).items.len(), 3);
    }

    /// 密钥不可用（key.bin 坏掉）→ 保存明确报 `NoKey`，绝不写出读不回来的索引。
    #[test]
    fn save_without_key_reports_nokey() {
        let broken = TempDir::new("save-nokey");
        // 「key.bin 存在但解不开」：换账户/换机器的形态
        fs::write(crypto::key_path(broken.path()), b"not a key file").unwrap();
        forget_key_cache();
        assert!(
            key_for(broken.path()).is_none(),
            "坏密钥文件不应静默换成新密钥"
        );

        match persist(broken.path(), &metas(&[item("x-1", "y")])) {
            Err(SaveError::NoKey) => {}
            other => panic!("应报 NoKey，实际 {:?}", other.err()),
        }
        assert!(
            !broken.path().join(INDEX_BIN).exists(),
            "没有密钥时不该写出索引"
        );
        assert_eq!(
            fs::read(crypto::key_path(broken.path())).unwrap(),
            b"not a key file",
            "失败时不许覆盖用户的密钥文件"
        );

        // 显式传密钥的纯写入路径仍然可用（T02b 的 WAL 会用这条）
        let ok = TempDir::new("save-ok");
        let key = crypto::load_or_create_key(ok.path()).expect("生成密钥");
        let metas = vec![Meta::from_item(&item("x-1", "y"))];
        save_snapshot(ok.path(), &metas, &key, 1).expect("写入快照");
        assert!(ok.path().join(INDEX_BIN).is_file());
        let back = load_index(ok.path());
        assert_eq!(back.status, LoadStatus::V2);
        assert_eq!(back.items.len(), 1);
    }
}
