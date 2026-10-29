//! 查询层：窗口取数 + 搜索下推。
//!
//! 为什么要把筛选/排序/切片搬到后端：今天前端拿到的是**全量**数组（10 万条 = 38 MB
//! 的 IPC + 一次全量排序 + 一次全量过滤），面板打开一次就要克隆一遍。窗口协议把每次
//! 交互的代价从「与总条数成正比」变成「与一页条数成正比」。
//!
//! 语义必须与今天前端的 `filtered()` **逐条等价** —— 这是硬要求，不是「差不多就行」：
//! 用户在同一个搜索框里搜同一个词，切到新实现后结果集不能变（多一条、少一条、顺序变了
//! 都算回归）。所以：
//!   * 标签词表只有 `labels.rs` 一份，前端以后消费后端下发的字典；
//!   * 匹配面与 `main.js` 完全一致：`preview`、`files[]`、kind 中文标签、kind 英文
//!     token、subtype 中文标签、subtype 英文 token，全部「小写包含」；
//!   * 单测里把 `filtered()` 原样重写一份当 oracle，在随机数据 × 随机 query 上比对
//!     顺序与集合（见 `tests::search_matches_frontend_oracle`）。
//!
//! 分页协议：**偏移 + 锚点自校正**。偏移负责随机访问（虚拟列表要能拖到 60%），
//! 锚点负责并发写入下的「不重复、不跳号」：前端带上「它认为位于 offset 的那条 id」，
//! 后端算出它在当前结果里的真实下标，对不上就返回 `Drift` 让前端整体平移。

use serde::{Deserialize, Serialize};

use crate::clipboard::HistoryItem;
use crate::labels;
use crate::store::Meta;

// ---------------------------------------------------------------------------
// 版本号
// ---------------------------------------------------------------------------

/// 列表版本：列表内容一变就 +1，用于前端缓存失效与锚点判断。
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 列表变了（插入 / 删除 / 置顶 / 收藏 / 清空 / 加载完成）。
pub fn bump_epoch() {
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 当前列表版本。
pub fn epoch() -> u64 {
    EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// 请求 / 响应
// ---------------------------------------------------------------------------

/// 一次筛选请求。前端的搜索框原样把字符串丢过来，归一（trim + 折叠 ASCII 大小写）
/// 由后端做 —— 前后端各做一半是「同一个词两边结果不同」的经典来源。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFilter {
    /// `history` | `text` | `images` | `files` | `favorites`；认不出来的按 `history` 处理。
    #[serde(default)]
    pub view: String,
    #[serde(default)]
    pub query: String,
    /// 可选的精确收窄（面板/未来的二级筛选用），`None` = 不额外限制。
    #[serde(default)]
    pub kind_hint: Option<String>,
    #[serde(default)]
    pub subtype_hint: Option<String>,
}

/// 锚点自校正的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Anchor {
    /// 锚点就在请求的下标上，窗口不用动。
    Ok,
    /// 锚点漂移了：`offset` 是它的真实下标，前端应把窗口整体平移到这个位置。
    Drift { offset: u32 },
    /// 锚点条目已经不在结果里（被删了 / 不再匹配）→ 按请求的 offset 重取并重设锚点。
    Missing,
}

/// 一页数据。字段名与设计文档的 IPC 契约一致（camelCase）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// 仍是完整的 `HistoryItem`：前端字段名一个都不用改。
    pub items: Vec<HistoryItem>,
    /// 实际生效的起始下标（锚点校正后的值）。
    pub offset: u32,
    /// 请求的起始下标，原样回显（前端据此判断是否被校正）。
    pub requested: u32,
    pub anchor: Anchor,
    /// 过滤后的总条数；`want_total = false` 时为 0（契约如此，别拿它当 0 条）。
    pub total: u32,
    /// 置顶分区的条数（同上，`want_total = false` 时为 0）。
    pub pinned_count: u32,
    /// 本次筛选的指纹（含 `file_copy` / `pin_global` 两个设置）。
    pub filter_key: u64,
    /// 列表版本。
    pub epoch: u64,
}

/// `list_ids` 的一页。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdPage {
    pub ids: Vec<String>,
    pub offset: u32,
}

/// 「清空当前视图」弹窗要的分组统计：置顶优先、收藏次之、其余为普通。
///
/// 为什么由后端算：这三组互不重叠的划分依赖 `pinnedViews` 与 `favorite`，
/// 前端要自己算就得先拿到全量条目 —— 那正是这次要消灭的东西。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterStats {
    pub total: u64,
    pub normal: u64,
    pub pinned: u64,
    pub favorites: u64,
}

/// `delete_by_filter` 的结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport {
    pub deleted: u64,
    /// 本地文件处理的提示（两道闸都满足时才有）。
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// 匹配
// ---------------------------------------------------------------------------

/// ASCII 折叠：大写转小写，其余字节原样。
#[inline]
fn fold(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// 大小写不敏感的「包含」判断，**零分配**。
///
/// `needle` 已归一（trim + 折叠 ASCII 大小写）成字节。走法是「首字节对上再逐字节比」，
/// 与 `String::to_lowercase().contains()` 在「ASCII + 中文」这个数据面上等价。
///
/// 已知差异：JS 的 `toLowerCase()` 会折叠非 ASCII 的大小写（西里尔、希腊字母等），
/// 这里不折。本产品的数据是中文 + ASCII 标识符，碰不到这类字符；换语言就得换实现，
/// 所以这条写在这里而不是藏进注释。
pub fn contains_ci(haystack: &str, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    if hay.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    for start in 0..=last_start {
        if fold(hay[start]) != first {
            continue;
        }
        if needle
            .iter()
            .enumerate()
            .all(|(j, nb)| fold(hay[start + j]) == *nb)
        {
            return true;
        }
    }
    false
}

/// 搜索词归一：trim + 折叠 ASCII 大小写（与 `main.js` 的 `el.value.trim().toLowerCase()` 对齐）。
fn normalize_query(raw: &str) -> Vec<u8> {
    raw.trim().bytes().map(fold).collect()
}

/// 分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    History,
    Text,
    Images,
    Files,
    Favorites,
}

impl View {
    fn parse(s: &str) -> View {
        match s {
            "text" => View::Text,
            "images" => View::Images,
            "files" => View::Files,
            "favorites" => View::Favorites,
            // history 与一切认不出来的值都按「全部」处理：前端多传一个视图名
            // 不该导致列表空白
            _ => View::History,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            View::History => "history",
            View::Text => "text",
            View::Images => "images",
            View::Files => "files",
            View::Favorites => "favorites",
        }
    }
}

/// 这条内容是否在 `view` 这个分类里置顶（通配项 `"*"` 对所有分类成立）。
///
/// 与 `main.js::isPinnedIn` 一致。注意**不看 `pin_global`**：那个开关只影响「点置顶时
/// 怎么写 `pinned_views`」，读取侧永远只认这份数组 —— 两边各判一次就会分叉。
fn is_pinned_in(item: &Meta, view: View) -> bool {
    let want = view.as_str();
    item.pinned_views
        .iter()
        .any(|v| v == "*" || v.as_str() == want)
}

/// 归一后的筛选条件：一次解析，多次判定。
struct Resolved<'a> {
    view: View,
    /// 归一后的搜索词（空 = 不搜）。
    needle: Vec<u8>,
    kind_hint: Option<&'a str>,
    subtype_hint: Option<&'a str>,
    /// 文件复制开关（后端直接读 settings，不由前端传 —— 单一数据源）。
    file_copy: bool,
}

impl<'a> Resolved<'a> {
    fn new(filter: &'a HistoryFilter) -> Resolved<'a> {
        let cfg = crate::settings::get();
        Resolved {
            view: View::parse(&filter.view),
            needle: normalize_query(&filter.query),
            kind_hint: filter.kind_hint.as_deref(),
            subtype_hint: filter.subtype_hint.as_deref(),
            file_copy: cfg.file_copy,
        }
    }

    /// 分类 + 文件开关 + 精确收窄。**不含搜索**（搜索是另一段更贵的判定）。
    fn passes_view(&self, item: &Meta) -> bool {
        match self.view {
            View::History => {}
            View::Favorites => {
                if !item.favorite {
                    return false;
                }
            }
            View::Text => {
                if item.kind != "text" {
                    return false;
                }
            }
            View::Images => {
                if item.kind != "image" {
                    return false;
                }
            }
            View::Files => {
                if item.kind != "files" {
                    return false;
                }
            }
        }
        // 文件复制开关关闭 → 文件条目在**所有视图**（含历史与收藏）都不展示。
        // 旧条目只是隐藏，数据还在磁盘上，重新打开开关就回来。
        if !self.file_copy && item.kind == "files" {
            return false;
        }
        if let Some(k) = self.kind_hint {
            if item.kind != k {
                return false;
            }
        }
        if let Some(s) = self.subtype_hint {
            if item.subtype.as_deref() != Some(s) {
                return false;
            }
        }
        true
    }

    /// 搜索命中。逐条对应 `main.js::filtered()` 里那段 `list.filter(...)`。
    fn matches_query(&self, item: &Meta) -> bool {
        if self.needle.is_empty() {
            return true;
        }
        // 匹配面与前端逐项对齐，顺序也保持一致（先命中先返回，性能更好）
        if !item.preview.is_empty() && contains_ci(&item.preview, &self.needle) {
            return true;
        }
        if let Some(files) = &item.files {
            if files.iter().any(|f| contains_ci(f, &self.needle)) {
                return true;
            }
        }
        // 类型：中文标签与英文 token 都参与（搜「图片」与搜「image」都要命中）。
        // `search_aliases` 只覆盖字典内取值，未知 kind 的原值由下面那句兜住 ——
        // 与前端 `(i.kind || "")` 无条件参与匹配等价。
        for word in labels::search_aliases(&item.kind, item.subtype.as_deref()) {
            if contains_ci(word, &self.needle) {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// 扫描
// ---------------------------------------------------------------------------

/// 一次扫描的结果：命中条目的下标，按「置顶组 / 其余组」分开存。
///
/// 分组而不是直接排序的理由：常驻列表本身已经是 `ts` 降序，**稳定地**把置顶项提到
/// 前面就得到了目标顺序（两组内部仍是 `ts` 降序）。这样一次扫描就够，不用全量排序。
struct Hits {
    pinned: Vec<u32>,
    other: Vec<u32>,
    /// 锚点在哪一组、组内第几个；`None` = 不在结果里。
    anchor: Option<(bool, u32)>,
}

impl Hits {
    fn total(&self) -> usize {
        self.pinned.len() + self.other.len()
    }

    /// 取 `[offset, offset + limit)` 这一段的下标。
    fn range(&self, offset: usize, limit: usize) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::with_capacity(limit.min(self.total().saturating_sub(offset)));
        let mut remaining = limit;
        let mut off = offset;
        if off < self.pinned.len() {
            let take = remaining.min(self.pinned.len() - off);
            out.extend_from_slice(&self.pinned[off..off + take]);
            remaining -= take;
            off = 0;
        } else {
            off -= self.pinned.len();
        }
        if remaining > 0 && off < self.other.len() {
            let take = remaining.min(self.other.len() - off);
            out.extend_from_slice(&self.other[off..off + take]);
        }
        out
    }

    /// 锚点在「过滤后序列」里的绝对下标。
    fn anchor_index(&self) -> Option<u32> {
        self.anchor.map(|(is_pinned, at)| {
            if is_pinned {
                at
            } else {
                self.pinned.len() as u32 + at
            }
        })
    }
}

/// 扫一遍常驻列表，收集命中项（按过滤后的顺序分组）与锚点位置。
///
/// **前提**：`items` 恒按 `ts` 降序（`store::insert_ordered` 维护这条不变量）。
/// 有它才能用「一次稳定分组」代替「一次全量排序」：两组内部本来就是 ts 降序，
/// 把置顶组整体提到前面就得到了目标顺序。传进来的列表若没排过序，结果顺序会错 ——
/// 所以 debug 下直接断言，别让这种错悄悄流到界面上。
fn scan(items: &[Meta], r: &Resolved<'_>, anchor_id: Option<&str>) -> Hits {
    debug_assert!(
        items.windows(2).all(|w| w[0].ts >= w[1].ts),
        "常驻列表必须保持 ts 降序"
    );
    let mut hits = Hits {
        pinned: Vec::new(),
        other: Vec::new(),
        anchor: None,
    };
    let wants_anchor = anchor_id.is_some();
    for (idx, item) in items.iter().enumerate() {
        let pinned = is_pinned_in(item, r.view);
        // 锚点只要在**结果集**里就要记下来，与它是否命中搜索无关 —— 它本来就应该
        // 从结果里消失（前端会收到 Missing）。
        let is_anchor = wants_anchor && anchor_id == Some(item.id.as_str());
        if !r.passes_view(item) || !r.matches_query(item) {
            continue;
        }
        if pinned {
            if is_anchor {
                hits.anchor = Some((true, hits.pinned.len() as u32));
            }
            hits.pinned.push(idx as u32);
        } else {
            if is_anchor {
                hits.anchor = Some((false, hits.other.len() as u32));
            }
            hits.other.push(idx as u32);
        }
    }
    hits
}

/// 筛选指纹：`FNV-1a 64` 覆盖所有会影响结果集的输入（视图 / 查询 / 两个设置 / 两个收窄）。
///
/// 前端翻页时回传这个值，对不上就直接重取首页 —— 缓存错页比多取一页糟得多。
fn filter_key(r: &Resolved<'_>, filter: &HistoryFilter) -> u64 {
    const SEP: u8 = 0x1f; // 单元分隔符：普通文本里不会出现，避免「a|b」与「a」+「b」撞车
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= SEP as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    eat(r.view.as_str().as_bytes());
    eat(&r.needle);
    eat(if r.file_copy {
        b"file_copy=1"
    } else {
        b"file_copy=0"
    });
    let pin_global = crate::settings::get().pin_global;
    eat(if pin_global {
        b"pin_global=1"
    } else {
        b"pin_global=0"
    });
    eat(filter.kind_hint.as_deref().unwrap_or("").as_bytes());
    eat(filter.subtype_hint.as_deref().unwrap_or("").as_bytes());
    h
}

// ---------------------------------------------------------------------------
// 纯函数：窗口 / 计数 / id 列表（命令与单测共用这一份实现）
// ---------------------------------------------------------------------------

/// 取一页。
pub fn window(
    items: &[Meta],
    filter: &HistoryFilter,
    offset: u32,
    limit: u32,
    anchor_id: Option<&str>,
    want_total: bool,
) -> Window {
    let r = Resolved::new(filter);
    let hits = scan(items, &r, anchor_id);

    // 锚点校正：锚点下标与请求不一致 → 以锚点为准，让前端整体平移窗口。
    let mut anchor = Anchor::Ok;
    let mut effective = offset as usize;
    if anchor_id.is_some() {
        match hits.anchor_index() {
            Some(actual) => {
                if actual != offset {
                    anchor = Anchor::Drift { offset: actual };
                    effective = actual as usize;
                }
            }
            None => anchor = Anchor::Missing,
        }
    }

    let picked = hits.range(effective, limit as usize);
    let items_out: Vec<HistoryItem> = picked
        .iter()
        .map(|i| items[*i as usize].to_item())
        .collect();

    Window {
        items: items_out,
        offset: effective as u32,
        requested: offset,
        anchor,
        // 契约：want_total = false 时这两个字段是 0（翻页不需要，省一次前端状态更新）
        total: if want_total { hits.total() as u32 } else { 0 },
        pinned_count: if want_total {
            hits.pinned.len() as u32
        } else {
            0
        },
        filter_key: filter_key(&r, filter),
        epoch: epoch(),
    }
}

/// 过滤后的条数。
pub fn count(items: &[Meta], filter: &HistoryFilter) -> u64 {
    let r = Resolved::new(filter);
    items
        .iter()
        .filter(|item| r.passes_view(item) && r.matches_query(item))
        .count() as u64
}

/// 分组统计：一条一旦置顶就不再计入收藏组（与前端弹窗的三组划分一致）。
pub fn stats(items: &[Meta], filter: &HistoryFilter) -> FilterStats {
    let r = Resolved::new(filter);
    let mut out = FilterStats {
        total: 0,
        normal: 0,
        pinned: 0,
        favorites: 0,
    };
    for item in items {
        if !(r.passes_view(item) && r.matches_query(item)) {
            continue;
        }
        out.total += 1;
        if !item.pinned_views.is_empty() {
            out.pinned += 1;
        } else if item.favorite {
            out.favorites += 1;
        } else {
            out.normal += 1;
        }
    }
    out
}

/// 过滤后的 id（按结果顺序）。
pub fn matching_ids(items: &[Meta], filter: &HistoryFilter) -> Vec<String> {
    let r = Resolved::new(filter);
    let hits = scan(items, &r, None);
    let mut out = Vec::with_capacity(hits.total());
    for idx in hits.pinned.iter().chain(hits.other.iter()) {
        out.push(items[*idx as usize].id.clone());
    }
    out
}

/// 取一段 id（分页），批量操作用。
pub fn ids_page(items: &[Meta], filter: &HistoryFilter, offset: u32, limit: u32) -> IdPage {
    let r = Resolved::new(filter);
    let hits = scan(items, &r, None);
    let picked = hits.range(offset as usize, limit as usize);
    IdPage {
        ids: picked
            .iter()
            .map(|i| items[*i as usize].id.clone())
            .collect(),
        offset,
    }
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 取一页历史。`file_copy` 由后端直接读设置，前端不用传（单一数据源）。
#[tauri::command]
pub fn get_history_window(
    filter: HistoryFilter,
    offset: u32,
    limit: u32,
    anchor_id: Option<String>,
    want_total: bool,
) -> Result<Window, String> {
    if limit == 0 {
        return Err("limit 不能为 0".into());
    }
    Ok(crate::clipboard::with_history(|items| {
        window(
            items,
            &filter,
            offset,
            limit,
            anchor_id.as_deref(),
            want_total,
        )
    }))
}

/// 过滤后的条数（`count_by_filter` 与之等价，保留两个名字是为了对齐设计文档的调用点）。
#[tauri::command]
pub fn history_count(filter: HistoryFilter) -> Result<u64, String> {
    Ok(crate::clipboard::with_history(|items| {
        count(items, &filter)
    }))
}

/// 「清空当前视图」的分组统计（弹窗文案与勾选项用）。
#[tauri::command]
pub fn filter_stats(filter: HistoryFilter) -> Result<FilterStats, String> {
    Ok(crate::clipboard::with_history(|items| stats(items, &filter)))
}

/// 见 [`history_count`]。
#[tauri::command]
pub fn count_by_filter(filter: HistoryFilter) -> Result<u64, String> {
    history_count(filter)
}

/// 取一段 id（批量选择 / 校验用），不传全量。
#[tauri::command]
pub fn list_ids(filter: HistoryFilter, offset: u32, limit: u32) -> Result<IdPage, String> {
    if limit == 0 {
        return Err("limit 不能为 0".into());
    }
    Ok(crate::clipboard::with_history(|items| {
        ids_page(items, &filter, offset, limit)
    }))
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条常驻条目。`files` / `pinned_views` 由调用方按需补。
    fn meta(id: &str, kind: &str, ts: u64, preview: &str) -> Meta {
        let mut h = HistoryItem {
            id: id.to_string(),
            kind: kind.to_string(),
            ts,
            pinned: false,
            pinned_views: Vec::new(),
            favorite: false,
            edited: false,
            shot: false,
            subtype: None,
            preview: preview.to_string(),
            preview_truncated: false,
            size: preview.len() as u64,
            text_path: None,
            html_path: None,
            rtf_path: None,
            rich_images: Vec::new(),
            img_path: None,
            files_path: None,
            files: None,
        };
        if kind == "text" {
            h.text_path = Some(crate::store::text_rel(id));
        }
        Meta::from_item(&h)
    }

    fn filter(view: &str, query: &str) -> HistoryFilter {
        HistoryFilter {
            view: view.into(),
            query: query.into(),
            kind_hint: None,
            subtype_hint: None,
        }
    }

    // -----------------------------------------------------------------------
    // oracle：把 main.js::filtered() 原样重写一份当参照
    // -----------------------------------------------------------------------

    /// `main.js::filtered()` 的逐行翻译（**独立实现**，故意不用 `contains_ci`）：
    /// 用 `to_lowercase().contains()` 这种最朴素、最不可能写错的方式，
    /// 这样它才能当参照物。
    fn oracle(items: &[Meta], view: &str, query: &str, file_copy: bool) -> Vec<String> {
        let q = query.trim().to_lowercase();
        let mut list: Vec<&Meta> = items
            .iter()
            .filter(|i| match view {
                "favorites" => i.favorite,
                "text" => i.kind == "text",
                "images" => i.kind == "image",
                "files" => i.kind == "files",
                _ => true,
            })
            .filter(|i| file_copy || i.kind != "files")
            .filter(|i| {
                if q.is_empty() {
                    return true;
                }
                let kind_label = labels::kind_label(&i.kind).unwrap_or("");
                let sub_label = i
                    .subtype
                    .as_deref()
                    .and_then(labels::subtype_label)
                    .unwrap_or("");
                let sub = i.subtype.as_deref().unwrap_or("");
                (i.preview.to_lowercase().contains(&q) && !i.preview.is_empty())
                    || i.files
                        .as_ref()
                        .is_some_and(|fs| fs.iter().any(|f| f.to_lowercase().contains(&q)))
                    || kind_label.to_lowercase().contains(&q)
                    || i.kind.to_lowercase().contains(&q)
                    || sub_label.to_lowercase().contains(&q)
                    || sub.to_lowercase().contains(&q)
            })
            .collect();
        // 置顶优先，其余 ts 降序；sort_by 是稳定排序，与 JS 的 Array#sort 一致
        list.sort_by(|a, b| {
            let ap = is_pinned_in(a, View::parse(view));
            let bp = is_pinned_in(b, View::parse(view));
            bp.cmp(&ap).then(b.ts.cmp(&a.ts))
        });
        list.iter().map(|i| i.id.clone()).collect()
    }

    /// 随机数：自带的线性同余（不引依赖，够用来铺测试数据）。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 17
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[(self.next() as usize) % xs.len()]
        }
    }

    /// 300 条样例 × 200 个 query：窗口全量拉取的结果必须与 oracle 逐条相等（顺序 + 集合）。
    #[test]
    fn search_matches_frontend_oracle() {
        let mut rng = Rng(0x5EED);
        let kinds = ["text", "image", "files"];
        let subtypes: [Option<&str>; 4] = [None, Some("link"), Some("code"), Some("markdown")];
        let words = [
            "剪贴板",
            "代码",
            "链接",
            "图片",
            "文本",
            "文件",
            "Markdown",
            "rust",
            "Rust",
            "image",
            "code",
            "link",
            "files",
            "git",
            "提交",
            "npm",
            "http",
            "README",
            "密码",
            "C:/Users/",
            "中文 空格",
        ];
        let mut items: Vec<Meta> = Vec::new();
        for i in 0..300usize {
            let kind = *rng.pick(&kinds);
            let mut m = meta(
                &format!("{:x}-{}", 1_700_000_000_000u64 + i as u64, i),
                kind,
                1_700_000_000_000 + i as u64,
                "",
            );
            let mut preview = String::new();
            for _ in 0..(1 + rng.next() % 3) {
                preview.push_str(rng.pick(&words));
                preview.push(' ');
            }
            m.preview = preview;
            m.subtype = if kind == "text" {
                rng.pick(&subtypes).map(str::to_string)
            } else {
                None
            };
            if kind == "files" {
                let mut files = Vec::new();
                for _ in 0..(1 + rng.next() % 3) {
                    files.push(format!("C:/{}", rng.pick(&words)));
                }
                m.files = Some(files);
                m.set_files_path(Some(crate::store::files_rel(&m.id)));
            }
            if rng.next() % 5 == 0 {
                m.favorite = true;
            }
            if rng.next() % 7 == 0 {
                m.pinned_views = vec!["history".into()];
            } else if rng.next() % 11 == 0 {
                m.pinned_views = vec!["*".into()];
            }
            if rng.next() % 13 == 0 {
                m.pinned_views =
                    vec![
                        View::parse(rng.pick(&["text", "images", "files", "favorites"]))
                            .as_str()
                            .into(),
                    ];
            }
            items.push(m);
        }
        // 常驻列表恒按 ts 降序（生产里由 insert_ordered 维护）。测试数据必须照着
        // 这个不变量构造，否则比的不是实现而是「输入有没有排过序」。
        items.sort_by(|a, b| b.ts.cmp(&a.ts));

        let views = ["history", "text", "images", "files", "favorites"];
        let mut queries: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        for _ in 0..(200 - queries.len()) {
            let mut q = String::new();
            for _ in 0..(1 + rng.next() % 2) {
                q.push_str(rng.pick(&words));
            }
            queries.push(q);
        }
        queries.truncate(200);

        for file_copy in [false, true] {
            // 设置是全局的：直接在这里写（单测环境里 settings::update 是空操作，
            // 所以走「环境变量式」的旁路不可行）→ 用 Resolved 的构造参数替代
            for view in views {
                for q in &queries {
                    let f = filter(view, q);
                    let mut r = Resolved::new(&f);
                    r.file_copy = file_copy; // 覆盖全局设置，两个开关组合都要比
                    let hits = scan(&items, &r, None);
                    let got: Vec<String> = hits
                        .pinned
                        .iter()
                        .chain(hits.other.iter())
                        .map(|i| items[*i as usize].id.clone())
                        .collect();
                    let want = oracle(&items, view, q, file_copy);
                    assert_eq!(
                        got, want,
                        "view={view} query={q:?} file_copy={file_copy} 结果不一致"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // 翻页
    // -----------------------------------------------------------------------

    fn big_list(n: usize) -> Vec<Meta> {
        (0..n)
            .map(|i| {
                let mut m = meta(
                    &format!("{:x}-1", 1_700_000_000_000u64 + (n - i) as u64),
                    "text",
                    1_700_000_000_000 + (n - i) as u64,
                    "普通文本内容",
                );
                if i % 500 == 0 {
                    m.pinned_views = vec!["history".into()];
                }
                if i % 300 == 0 {
                    m.favorite = true;
                }
                m
            })
            .collect()
    }

    /// 10 万条按 limit=200 翻到底：每个 id 恰好出现一次，顺序 == 全量排序。
    #[test]
    fn paging_covers_all_once() {
        let items = big_list(100_000);
        let f = filter("history", "");
        let all = matching_ids(&items, &f);
        assert_eq!(all.len(), 100_000);

        let mut seen: Vec<String> = Vec::with_capacity(all.len());
        let mut offset = 0u32;
        while (offset as usize) < all.len() {
            let w = window(&items, &f, offset, 200, None, offset == 0);
            assert!(!w.items.is_empty());
            for it in &w.items {
                seen.push(it.id.clone());
            }
            offset += w.items.len() as u32;
        }
        assert_eq!(seen.len(), 100_000, "不能有重复或遗漏");
        assert_eq!(seen, all, "翻页顺序必须等于全量顺序");
    }

    /// 翻页期间持续插入（最常见的变更：用户又复制了东西）：不重复、不跳号。
    ///
    /// 客户端按设计协议做事：带「我认为位于 offset 的那条 id」当锚点；收到 `Drift`
    /// 就把 offset 校正到锚点的真实位置。插入发生在列表**上方**，锚点只会往后挪，
    /// 所以校正只会跳过新条目，不会与已看过的页重叠。
    #[test]
    fn paging_no_dup_while_inserting() {
        let mut items = big_list(10_000);
        let f = filter("history", "");
        let mut expected = matching_ids(&items, &f);
        let mut offset = 0u32;
        let mut seen: Vec<String> = Vec::new();
        let mut drifts = 0usize;
        let mut round = 0usize;

        loop {
            if offset as usize >= expected.len() {
                break;
            }
            let anchor = expected[offset as usize].clone();
            let w = window(&items, &f, offset, 200, Some(&anchor), true);
            match w.anchor {
                Anchor::Drift { offset: actual } => {
                    drifts += 1;
                    assert!(actual >= offset, "插入只会让锚点往后挪");
                    assert_eq!(w.items[0].id, anchor, "校正后第一条必须就是锚点");
                    offset = actual;
                    expected = matching_ids(&items, &f);
                    continue;
                }
                Anchor::Missing => panic!("插入不该让锚点消失"),
                Anchor::Ok => {}
            }
            seen.extend(w.items.iter().map(|i| i.id.clone()));
            offset += w.items.len() as u32;
            round += 1;

            // 每翻两页，用户又复制了 30 条（都更新，插到最前）
            if round % 2 == 0 && round <= 20 {
                for k in 0..30u64 {
                    items.insert(
                        0,
                        meta(
                            &format!("{:x}-1", 2_100_000_000_000u64 + k + round as u64 * 100),
                            "text",
                            2_100_000_000_000 + k + round as u64 * 100,
                            "翻页期间新复制的内容",
                        ),
                    );
                }
            }
            assert!(round <= 200, "翻页没有收敛");
        }

        // 不重复
        let mut uniq = seen.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), seen.len(), "翻页出现重复条目");

        // 不跳号：原来那 1 万条一条都不能漏（新插入的在上方，没翻到属正常）
        let old: std::collections::HashSet<String> =
            big_list(10_000).into_iter().map(|m| m.id).collect();
        let covered = seen.iter().filter(|id| old.contains(*id)).count();
        assert_eq!(covered, 10_000, "原有条目漏了 {} 条", 10_000 - covered);

        // Drift 路径确实被走过
        assert!(drifts >= 10, "插入造成的漂移没被检出（{drifts} 次）");
    }

    /// 游标**前面**被删掉 50 条：锚点报 `Drift`，且校正后的位置正好是锚点本身。
    ///
    /// 这就是「拖到 60% 时正好有人删了条目」的形态：不校正就会重复显示 50 条。
    #[test]
    fn anchor_corrects_when_rows_deleted_before_cursor() {
        let mut items = big_list(2_000);
        let f = filter("history", "");
        let expected = matching_ids(&items, &f);

        // 先取一页，记下「第 800 条」是谁
        let offset = 800u32;
        let anchor = expected[offset as usize].clone();
        let first = window(&items, &f, offset, 100, Some(&anchor), true);
        assert_eq!(first.anchor, Anchor::Ok);
        assert_eq!(first.items[0].id, anchor);

        // 在它前面删掉 50 条（模拟界面上别处删除）
        items.drain(100..150);
        let after = window(&items, &f, offset, 100, Some(&anchor), true);
        assert_eq!(
            after.anchor,
            Anchor::Drift {
                offset: offset - 50
            },
            "锚点前移 50 条，必须报 Drift 并给出真实位置"
        );
        assert_eq!(after.offset, offset - 50);
        assert_eq!(after.items[0].id, anchor, "校正后第一条就是锚点");
        // 正确的续页：从锚点（而不是旧 offset）继续，不会重复也不会跳过
        let next = window(&items, &f, after.offset + 100, 100, None, true);
        assert_eq!(
            next.items[0].id,
            expected[(offset + 100) as usize],
            "续页位置正确"
        );

        // 锚点自己被删掉 → Missing，按请求的 offset 重取
        let mut items2 = big_list(2_000);
        let gone = expected[offset as usize].clone();
        let vec_at = items2
            .iter()
            .position(|m| m.id == gone)
            .expect("锚点应在列表里");
        items2.remove(vec_at);
        let lost = window(&items2, &f, offset, 100, Some(&gone), true);
        assert_eq!(lost.anchor, Anchor::Missing);
        assert_eq!(lost.offset, offset, "丢失时按请求的 offset 重取");
    }

    /// 锚点三态。
    #[test]
    fn anchor_ok_drift_and_missing() {
        let items = big_list(1000);
        let f = filter("history", "");

        // 原位
        let id0 = items[0].id.clone();
        let w = window(&items, &f, 0, 50, Some(&id0), true);
        assert_eq!(w.anchor, Anchor::Ok);
        assert_eq!(w.offset, 0);

        // 漂移：在锚点前面插一条**同组**（都是非置顶）的新条目
        // 取一个非置顶的锚点：big_list 里只有 i%500==0 的那几条是置顶的
        // big_list 里 i%500==0 的是置顶项：items[0] 与 items[500] 在置顶组，
        // 所以 items[1] 的过滤后下标是 2
        let anchor_id = items[1].id.clone();
        let w = window(&items, &f, 2, 50, Some(&anchor_id), true);
        assert_eq!(w.anchor, Anchor::Ok, "原位时不该漂移");
        let mut moved = items.clone();
        // ts 取 items[0] 的值：插入后列表仍是非递增的（查询层的前提）
        let insert_ts = items[0].ts;
        moved.insert(1, meta("ffff-1", "text", insert_ts, "插到前面的新条目"));
        let w = window(&moved, &f, 2, 50, Some(&anchor_id), true);
        assert_eq!(w.anchor, Anchor::Drift { offset: 3 });
        assert_eq!(w.offset, 3, "偏移要校正到锚点的真实位置");
        assert_eq!(w.items[0].id, anchor_id, "校正后第一条就是锚点");

        // 丢失：锚点被删
        let w = window(&moved, &f, 10, 50, Some("不存在的id"), true);
        assert_eq!(w.anchor, Anchor::Missing);
        assert_eq!(w.offset, 10, "丢失时按请求的 offset 重取");
    }

    /// `file_copy` 开关：files 条目进出 + `filter_key` 变化。
    #[test]
    fn file_copy_toggle_changes_key_and_rows() {
        let mut items = big_list(50);
        items.push({
            let mut m = meta("f-1", "files", 1_600_000_000_000, "a.txt 等 2 个文件");
            m.files = Some(vec!["C:/a.txt".into(), "C:/b.txt".into()]);
            m.set_files_path(Some(crate::store::files_rel("f-1")));
            m
        });
        let f = filter("history", "");

        // 打开：files 条目在
        let mut r = Resolved::new(&f);
        r.file_copy = true;
        let key_on = filter_key(&r, &f);
        assert_eq!(scan(&items, &r, None).total(), 51);

        // 关闭：files 条目消失（含历史与收藏两个视图），filter_key 也变
        let mut r2 = Resolved::new(&f);
        r2.file_copy = false;
        let key_off = filter_key(&r2, &f);
        assert_eq!(scan(&items, &r2, None).total(), 50);
        assert_ne!(key_on, key_off, "开关必须参与指纹");

        let fav = filter("favorites", "");
        assert_eq!(Resolved::new(&fav).view, View::Favorites);
    }

    /// 置顶：通配项 `"*"` 对所有分类成立；按分类置顶只影响本分类。
    #[test]
    fn pin_wildcard_and_per_view() {
        let mut a = meta("a-1", "text", 3000, "a");
        a.pinned_views = vec!["*".into()];
        let mut b = meta("b-2", "image", 2000, "b");
        b.pinned_views = vec!["images".into()];
        let c = meta("c-3", "text", 1000, "c");
        let items = vec![a, b, c];

        // 历史视图：只有 `"*"` 的 a 置顶
        let f = filter("history", "");
        let ids = matching_ids(&items, &f);
        assert_eq!(ids, vec!["a-1", "b-2", "c-3"]);

        // 图片视图：a 是 text 不出现；b 在自己分类里置顶 → 第一个
        let f = filter("images", "");
        let ids = matching_ids(&items, &f);
        assert_eq!(ids, vec!["b-2"]);
    }

    /// `history_count` 与全量过滤后条数一致（含 query）。
    #[test]
    fn count_matches_full_filter() {
        let items = big_list(500);
        for (view, q) in [
            ("history", ""),
            ("text", ""),
            ("favorites", ""),
            ("history", "普通"),
            ("history", "文本内容"),
            ("history", "不存在的词"),
        ] {
            let f = filter(view, q);
            assert_eq!(count(&items, &f), matching_ids(&items, &f).len() as u64);
        }
    }

    /// `contains_ci` 等价于 `to_lowercase().contains()`（ASCII + 中文）。
    #[test]
    fn contains_ci_matches_lowercase_contains() {
        let cases = [
            ("Hello World", "hello"),
            ("Hello World", "WORLD"),
            ("剪贴板内容", "剪贴"),
            ("npm run build", "NPM"),
            ("短", "短"),
            ("", "x"),
            ("abc", ""),
        ];
        for (hay, needle) in cases {
            let want = hay.to_lowercase().contains(&needle.to_lowercase());
            let got = contains_ci(hay, &normalize_query(needle));
            assert_eq!(got, want, "{hay:?} 含 {needle:?}");
        }
        // 空搜索词永远命中（对应前端「没输入就不筛」）
        assert!(contains_ci("任意", &[]));
    }

    /// 性能基准（手动跑）：10 万条、limit=200 的 P50/P99。
    #[test]
    #[ignore]
    fn bench_window_latency() {
        let items = big_list(100_000);
        let mut q_items = items.clone();
        // 给每条塞一段更接近真实的预览，让带 query 的那条量得准
        for m in q_items.iter_mut() {
            m.preview = "剪贴板内容片段，用于压测的一条正常长度的预览文本".repeat(2);
        }

        for (name, query) in [("无 query", ""), ("带 query", "剪贴板")] {
            let f = filter("history", query);
            let list = if query.is_empty() { &items } else { &q_items };
            let mut samples: Vec<u128> = Vec::with_capacity(100);
            for k in 0..100u32 {
                let offset = (k as usize * 997) % 99_000;
                let t = std::time::Instant::now();
                let w = window(list, &f, offset as u32, 200, None, k == 0);
                std::hint::black_box(w.items.len());
                samples.push(t.elapsed().as_micros());
            }
            samples.sort_unstable();
            println!(
                "{name}：P50 {} µs，P99 {} µs（10 万条，limit=200，100 次）",
                samples[samples.len() / 2],
                samples[samples.len() * 99 / 100]
            );
        }
    }
}
