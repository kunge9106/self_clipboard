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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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
    pub items: Vec<HistoryItem>,
    pub status: LoadStatus,
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
pub fn insert_ordered(items: &mut Vec<HistoryItem>, item: HistoryItem) -> usize {
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

/// 快照的写入序号。进程内单调递增，只用于诊断（发现「加载到的快照比上次还旧」）。
///
/// 不跨进程持久化：跨进程比较需要先把序号解出来，而序号进了 AAD，解出来就得先解密 ——
/// 为了一个诊断字段多看一遍数据不值得。回滚重放不在威胁模型内（有写权限就能直接删）。
static WRITE_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_write_seq() -> u64 {
    WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
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

/// 手里已经是完整 `HistoryItem` 时的快路径：省掉一次 Meta 往返（十万条要几十毫秒）。
pub fn save_items(dir: &Path, items: &[HistoryItem], key: &Key, write_seq: u64) -> io::Result<()> {
    write_snapshot(dir, items, key, write_seq)
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

/// 应用层保存入口：自己解密钥、自己推进写序号、成功后完成迁移收尾。
pub fn persist(dir: &Path, items: &[HistoryItem]) -> Result<(), SaveError> {
    let key = key_for(dir).ok_or(SaveError::NoKey)?;
    let seq = next_write_seq();
    save_items(dir, items, &key, seq)?;
    finish_migration(dir, items.len(), seq);
    Ok(())
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

/// 读索引。按「v2 快照 → 旧格式 → 没有」的顺序，任何一种失败都不 panic。
pub fn load_index(dir: &Path) -> Loaded {
    // 上一次写到一半被杀的残留：`index.bin` 还没被 rename 出来，只有这个临时文件。
    // 它永远不该被当成数据源，先清掉再继续（清不掉也不影响后面的判断）。
    let _ = fs::remove_file(dir.join(INDEX_BIN_TMP));
    match fs::read(dir.join(INDEX_BIN)) {
        Ok(raw) => match read_snapshot(dir, &raw) {
            Ok(items) => {
                crate::log_print!(
                    "[store] 索引加载成功（v2）：{} 条，{} 字节",
                    items.len(),
                    raw.len()
                );
                Loaded {
                    items: pin_migrate(items),
                    status: LoadStatus::V2,
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
                Loaded {
                    items: Vec::new(),
                    status: LoadStatus::NeedsRebuild,
                }
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => load_legacy(dir),
        Err(e) => {
            crate::log_err!("store", "索引读取失败（{INDEX_BIN}）：{e}");
            Loaded {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
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

/// 解一份 v2 快照。
fn read_snapshot(dir: &Path, raw: &[u8]) -> Result<Vec<HistoryItem>, SnapshotError> {
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
            serde_json::from_str(&json)
                .map_err(|e| SnapshotError::Corrupt(format!("解析失败：{e}")))
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
fn load_legacy(dir: &Path) -> Loaded {
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
            return Loaded {
                items: Vec::new(),
                status: LoadStatus::Missing,
            };
        }
    };
    if raw.is_empty() {
        crate::log_err!(
            "store",
            "{INDEX_LEGACY} 是空文件（0 字节）—— 上次写入被中断（强杀/崩溃/断电）的特征，历史列表将为空"
        );
        keep(dir, INDEX_LEGACY, &raw, "空文件", LEGACY_CORRUPT);
        return Loaded {
            items: Vec::new(),
            status: LoadStatus::NeedsRebuild,
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
                return Loaded {
                    items: Vec::new(),
                    status: LoadStatus::NeedsRebuild,
                };
            }
        },
        Format::V0Plaintext => String::from_utf8_lossy(&raw).to_string(),
        // 认不出来：按损坏处理，留现场再重建。
        Format::V2 => {
            // 理论上不会发生（v2 分支在上面处理过）。真发生了说明 index.json 里
            // 装的是一份 v2 容器，按损坏处理即可，不用特殊兼容。
            keep(dir, INDEX_LEGACY, &raw, "是 v2 容器", LEGACY_CORRUPT);
            return Loaded {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
            };
        }
        Format::Unknown => {
            keep(dir, INDEX_LEGACY, &raw, "格式无法识别", LEGACY_CORRUPT);
            return Loaded {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
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
            Loaded {
                items: pin_migrate(items),
                status,
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
            Loaded {
                items: Vec::new(),
                status: LoadStatus::NeedsRebuild,
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
            for (a, b) in again.items.iter().zip(sample.iter()) {
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
                assert_eq!(a.text_path, b.text_path);
                assert_eq!(a.html_path, b.html_path);
                assert_eq!(a.rtf_path, b.rtf_path);
                assert_eq!(a.rich_images, b.rich_images);
                assert_eq!(a.img_path, b.img_path);
                assert_eq!(a.files_path, b.files_path);
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
        persist(dir.path(), &sample).unwrap();
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
        let mut items: Vec<HistoryItem> = loaded.items;
        crate::clipboard::rebuild_index_from_files(dir.path(), &mut items);
        persist(dir.path(), &items).unwrap();
        assert_eq!(load_index(dir.path()).status, LoadStatus::V2);
    }

    /// 索引被翻一个字节：判 Tampered → 改名 `.corrupt` → 重建，不 panic。
    #[test]
    fn tampered_snapshot_is_kept_and_rebuilds() {
        let dir = TempDir::new("tamper");
        persist(dir.path(), &sample_items()).unwrap();
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
        let mut list: Vec<HistoryItem> = Vec::new();
        for (id, ts) in [("a-1", 2000u64), ("b-2", 1900), ("c-3", 1800)] {
            let mut it = item(id, "x");
            it.ts = ts;
            insert_ordered(&mut list, it);
        }
        // 正常情况（新条目 ts 最大）：插在最前，与老的 insert(0) 一致
        assert_eq!(
            list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a-1", "b-2", "c-3"]
        );

        // 时钟回拨：新条目 ts 落在中间 → 插到中间，整体仍严格降序
        let mut back = item("d-4", "x");
        back.ts = 1850;
        insert_ordered(&mut list, back);
        assert_eq!(
            list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a-1", "b-2", "d-4", "c-3"]
        );
        assert!(list.windows(2).all(|w| w[0].ts >= w[1].ts));

        // 同 ts：新的排在同 ts 的最前面
        let mut same = item("e-5", "x");
        same.ts = 2000;
        insert_ordered(&mut list, same);
        assert_eq!(list[0].id, "e-5");
        assert_eq!(list[1].id, "a-1");
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

        match persist(broken.path(), &[item("x-1", "y")]) {
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
