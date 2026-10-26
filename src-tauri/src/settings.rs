//! 应用配置持久化（app_data_dir/config.json）。
//!
//! 迁移自 v1 的 config.json（trayOnClose / cleanImagesOnClose / imageCacheDir），
//! 字段语义保持一致；另加 retentionDays（v1 是硬编码 3 天）。

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::autostart;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 关闭时后台运行（隐藏到托盘）
    #[serde(default)]
    pub tray_on_close: bool,
    /// 关闭时清理缓存（退出时清空内容缓存与历史）
    #[serde(default)]
    pub clean_on_close: bool,
    /// 清理时是否连置顶/收藏一起删（默认豁免，单独存放）
    #[serde(default)]
    pub clean_include_pinned: bool,
    /// 退出前弹窗提醒确认
    #[serde(default)]
    pub clean_confirm_exit: bool,
    /// 历史保留天数（0 = 永久保留；v1 硬编码 3）
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    /// 上次运行时的版本号（每次启动写成当前版本）。
    ///
    /// 用来判断「本次是不是升级上来的」：两个版本号不一致，就说明用户刚换了版本，
    /// 关于页需要把中间跨过的所有更新日志一次性显示出来，而不是只显示当前这一节。
    #[serde(default)]
    pub last_run_version: String,
    /// 本次升级的起点版本（`last_run_version` 在版本变化时留下的一份）。
    ///
    /// 只在版本变化时更新，所以升级后**一直保留**：用户随时打开关于页，看到的都是
    /// 「从哪一版升上来、中间都改了什么」，直到下一次升级才换成新的起点。
    #[serde(default)]
    pub upgraded_from: String,
    /// 保留期到期清理时，是否连置顶/收藏一起删。**默认 false = 置顶/收藏永不到期**。
    ///
    /// 与 `clean_include_pinned` 分开而不是合并：两者风险等级不同。
    /// 退出清理是用户主动退出触发的；保留期清理是**后台自动、用户无感知**的，
    /// 一旦连置顶一起删，用户只能在「记录怎么没了」之后才发现。
    /// 绑成一个开关会让「想让退出清理也清置顶」的人，在不知情下让后台也清置顶。
    #[serde(default)]
    pub prune_include_pinned: bool,
    /// **直角模式**下的材质浓度（窗口主体层不透明度百分比，40–90）
    ///
    /// 两个模式**各记一档**、互不影响：切换圆角模式只是换用另一档的值，
    /// 切回去还是你上次为那个模式设的浓度。
    /// （圆角模式没有背景模糊，同样浓度会显得更透，所以默认档更高。）
    #[serde(default = "default_alpha_straight")]
    pub alpha_straight: u32,
    /// **圆角模式**下的材质浓度（40–90）
    #[serde(default = "default_alpha_round")]
    pub alpha_round: u32,
    /// v2.1.8 及更早版本的「单一浓度」字段，**只用于迁移**。
    ///
    /// 读到后按**当时的模式**归进 `alpha_straight` / `alpha_round`（见 `migrate`），
    /// 随后置空并且不再序列化（`skip_serializing_if`），所以 config.json 里不会再出现它。
    #[serde(
        default,
        rename = "material_alpha",
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_alpha: Option<u32>,
    /// 首次运行引导：开机自启那一次问过了吗
    #[serde(default)]
    pub asked_autostart: bool,
    /// 首次运行引导：第一次点关闭按钮时「后台运行还是退出」问过了吗
    #[serde(default)]
    pub asked_tray_on_close: bool,
    /// 圆角模式：**圆角 + 不用系统 blur**
    ///
    /// 为什么要有这个开关：Win10 的 acrylic 恒为**矩形**、`SetWindowRgn` 对它
    /// 无效（社区实证 SO 63324480 等），所以「圆角 + 系统模糊」必然在四角留下
    /// 一圈矩形残留。两种模式各有取舍，交给用户自选：
    ///   - 开（圆角）：关掉系统 blur，窗口为纯透明 layered → 四角真透明、100% 干净，
    ///     代价是背后的壁纸不再被模糊（面板要更实才好看）。
    ///   - 关（直角）：保留系统 blur，窗口用直角 → 窗口矩形与 acrylic 完全吻合，
    ///     同样没有任何残留。
    #[serde(default)]
    pub round_mode: bool,
    /// 文件复制：是否记录「资源管理器里复制的文件」（CF_HDROP）。
    ///
    /// **默认关闭**——这是用户明确要求的默认值：文件复制涉及真实文件路径，
    /// 且其读/写都走 shell 层（历史上这里有一个二次释放崩溃），默认不参与采集最稳妥。
    /// 关闭时：read_clipboard 完全不看 CF_HDROP；前端「文件」入口保留但显示已关闭提示。
    #[serde(default)]
    pub file_copy: bool,
    /// 置顶是否在**所有分类间互通**。
    ///
    /// **默认关闭** —— 保持「置顶按分类独立」的既有行为：在「文本」里置顶不影响它在
    /// 「历史」里的位置。开启后整条内容只有一个置顶状态，任一分类置顶/取消其它分类同步
    /// （存储上收敛成 `pinned_views = ["*"]`，见 `clipboard::PIN_ALL` / `normalize_pins`）。
    ///
    /// 默认关闭的原因：这是交互语义的改变，不该在升版本时静默替换用户已经形成的用法。
    #[serde(default)]
    pub pin_global: bool,
    /// 关闭「置顶互通」时，是否一并取消它产生的同步置顶。
    ///
    /// **默认开启**：删掉 `pinned_views` 里的同步标记 `"*"`，置顶回到当初那个分类
    /// （来源分类记录在 `pinned_views` 的具体分类名里）。
    /// 关闭时：`"*"` 原样保留 —— 同步产生的置顶是**永久标记**，关掉互通后它仍在各分类
    /// 置顶，下次再开互通时仍是同一条同步置顶，不会退化成每分类一条独立置顶。
    ///
    /// 只影响「关闭互通」这一刻的归一动作，见 `clipboard::normalize_pins`。
    #[serde(default = "default_true")]
    pub pin_cancel_on_disable: bool,
    /// 自定义缓存目录（None = 使用 app_data_dir/clipboard）
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// 最近一次成功的更新检查结果（跨重启保留，用于设置红点与「最新版更新日志」）。
    /// 检查失败时保持原值不动 —— 离线重启不该把已知的新版本信息抹掉。
    #[serde(default)]
    pub update_cache: Option<UpdateCache>,
    /// 功能导览看到第几版了。与 `TOUR_VERSION` 比较，小于就等于没看过。
    ///
    /// **为什么用版本号而不是布尔**：以后加了新功能，把 `TOUR_VERSION` 提到 2，
    /// 老用户就会再看到一次「新功能导览」；布尔值只能二选一，要么永久不再提示，
    /// 要么得手动重置。
    ///
    /// **默认一律是 0（= 没看过）**，也包括「有 config.json 但缺这个字段」的老配置。
    /// 早先这里刻意让老配置视为「已看过」，理由是「老用户早就摸索过了，弹窗是打扰」——
    /// 但导览的价值恰恰是让人知道**有哪些功能**，而升级上来的老用户正是最该看一遍的人；
    /// 让他们因为「配置文件里没有这个字段」而永远看不到，等于把这功能只做给了
    /// 人数最少的全新安装用户。用户明确要求「更新到这版都得默认弹一次」。
    ///
    /// 弹过之后 `dismiss_onboarding` 会写入当前版本号，此后不再出现。
    #[serde(default = "default_tour_seen")]
    pub tour_seen_version: u32,
    /// 不跟随 Windows 缩放比例：界面固定按 **100%** 渲染（**默认开启**）。
    ///
    /// 为什么默认开：窗口尺寸是按逻辑单位定的（1060×690）。系统缩放调高后可用
    /// **逻辑**分辨率变小 —— 1080p 屏在 150% 下只有 1280×720，扣掉任务栏约 680，
    /// 比窗口还矮，底部状态栏直接被切掉，看起来就是「布局乱了」。托盘菜单同理。
    ///
    /// 开启时的做法：webview 缩放设为 1/scale，窗口尺寸改用**物理**像素，
    /// 于是 1 CSS px = 1 物理像素 —— 不管系统怎么缩放，界面看上去都和 100% 时一样。
    /// 关掉就回到常规的「跟随系统缩放」。
    #[serde(default = "default_true")]
    pub ignore_dpi_scale: bool,
    /// 主题模式：`"light"` / `"dark"` / `"system"`（默认 `system` = 跟随 Windows）。
    ///
    /// 为什么是字符串而不是枚举：这个字段要原样进 config.json 和下发给前端，
    /// 用字符串最省事，也不必为了兼容旧值写反序列化兜底。非法值在 `set_theme_mode`
    /// 里挡掉，读的时候一律按 `system` 处理（见 `theme_is_dark`）。
    #[serde(default = "default_theme_mode")]
    pub theme_mode: String,
    /// 删除文件条目时，是否同时处理它引用的**本地文件**（默认**关闭**）。
    ///
    /// 文件条目记的是路径引用而非副本，删除条目本身只删记录（本地文件不受影响）；
    /// 打开这一项才表示「我要连磁盘上的文件一起处理」。默认关是刻意的：
    /// 这是会动用户真实文件的操作，必须是用户自己到设置里点开，程序不替他决定。
    #[serde(default)]
    pub delete_files_with_item: bool,
    /// 本地文件的处理方式：`false` = 移入回收站（可还原，默认），`true` = 永久删除。
    ///
    /// 单独一项而不是跟上面合成枚举：两个都是布尔语义，存起来最省事；
    /// 合并成枚举还要为旧配置写兼容。默认 false 保证「即使误开主开关」也不会
    /// 造成不可恢复的后果。
    #[serde(default)]
    pub delete_files_permanent: bool,
    /// 「链接 / 代码」识别规则的版本号。
    ///
    /// 识别是启发式的，规则改了之后**存量条目必须重算**，否则用户看到的还是旧结论
    /// （比如不带 http 的裸域名仍然是「文本」）。条目本身没有地方存规则版本，
    /// 所以记在配置里：`< 当前版本` 时把存量文本的 subtype 全部重置、再跑一遍判定。
    /// 只在规则变更时递增，正常运行不会触发。
    #[serde(default)]
    pub subtype_version: u32,
}

/// 当前的功能导览版本。改了导览内容就 +1，老用户会再看一次增量。
pub const TOUR_VERSION: u32 = 1;

/// serde 缺字段时的默认值：**0 = 没看过**。
///
/// 即「有 config.json 但没这个字段」的老配置也视为没看过 —— 升级到本版的用户
/// 会看到一次功能导览。写入时机在 `dismiss_onboarding`，弹过一次之后不再出现。
fn default_tour_seen() -> u32 {
    0
}

/// 一次成功的 `releases/latest` 查询结果。
///
/// 只存「GitHub 上最新的那个正式版」的元信息，**不存 hasUpdate**——那是个
/// 会随时间变的派生值（等用户升级之后同一个缓存就该判为「已是最新」），
/// 所以一律拿 `latest` 与当前 `CARGO_PKG_VERSION` 现算。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCache {
    /// 最新版版本号（已去掉前缀 v）
    #[serde(default)]
    pub latest: String,
    /// Release 正文（Markdown）
    #[serde(default)]
    pub notes: String,
    /// Release 页面地址
    #[serde(default)]
    pub url: String,
    /// 安装包直链（`assets[].browser_download_url`），没有则为空
    #[serde(default)]
    pub asset_url: String,
    /// 安装包文件名（ASCII）
    #[serde(default)]
    pub asset_name: String,
    /// 安装包字节数（0 = 未知）
    #[serde(default)]
    pub asset_size: u64,
    /// 检查时间（epoch 秒）
    #[serde(default)]
    pub checked_at: u64,
}

fn default_retention() -> u32 {
    3
}

fn default_theme_mode() -> String {
    "system".into()
}

fn default_true() -> bool {
    true
}

/// 直角 / 圆角模式各自的**默认**浓度。
///
/// 圆角模式没有背景模糊，同样浓度会显得更透，所以默认档更高。
pub const STRAIGHT_ALPHA: u32 = 50;
pub const ROUND_ALPHA: u32 = 90;

fn default_alpha_straight() -> u32 {
    STRAIGHT_ALPHA
}

fn default_alpha_round() -> u32 {
    ROUND_ALPHA
}

/// 浓度的合法区间（前端滑块 40–90，后端一律夹到这个范围）
pub const ALPHA_MIN: u32 = 10;
pub const ALPHA_MAX: u32 = 90;

impl Config {
    /// 当前模式生效的浓度。
    pub fn alpha(&self) -> u32 {
        if self.round_mode {
            self.alpha_round
        } else {
            self.alpha_straight
        }
    }

    /// 写入**当前模式**的浓度；另一个模式那一档不动。
    pub fn set_alpha(&mut self, v: u32) {
        let v = v.clamp(ALPHA_MIN, ALPHA_MAX);
        if self.round_mode {
            self.alpha_round = v;
        } else {
            self.alpha_straight = v;
        }
    }

    /// 老配置迁移：把 v2.1.8 及更早的单一 `material_alpha` 归进对应模式那一档。
    ///
    /// 返回是否发生了迁移（调用方据此决定要不要立刻回写文件）。
    /// 归到哪个模式取决于**当时**的 `round_mode`——那个值就是用户当时正在用的模式。
    fn migrate(&mut self) -> bool {
        match self.legacy_alpha.take() {
            Some(v) => {
                self.set_alpha(v);
                true
            }
            None => false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tray_on_close: false,
            clean_on_close: false,
            clean_include_pinned: false,
            clean_confirm_exit: false,
            retention_days: default_retention(),
            // 置顶/收藏默认不受保留期影响
            prune_include_pinned: false,
            // 首次安装时为空：没有「从哪一版升上来」，关于页只显示当前版本一节
            last_run_version: String::new(),
            upgraded_from: String::new(),
            alpha_straight: default_alpha_straight(),
            alpha_round: default_alpha_round(),
            legacy_alpha: None,
            asked_autostart: false,
            asked_tray_on_close: false,
            round_mode: false,
            file_copy: false, // 默认关闭：不记录文件复制
            // 默认关闭：删条目只删记录，不碰磁盘上的真实文件
            delete_files_with_item: false,
            // 默认回收站（可还原）
            delete_files_permanent: false,
            // 0 = 尚未按当前规则算过，首次启动会重算一遍存量
            subtype_version: 0,
            pin_global: false, // 默认关闭：置顶按分类独立（沿用既有行为）
            pin_cancel_on_disable: true, // 默认开启：关掉互通即撤销它铺开的置顶
            cache_dir: None,
            update_cache: None,
            // 全新安装（没有 config.json）才走这里 → 0 → 会弹功能导览
            tour_seen_version: 0,
            // 默认不跟随系统缩放（见字段注释）
            ignore_dpi_scale: true,
            theme_mode: default_theme_mode(),
        }
    }
}

static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 配置文件的默认位置：`%APPDATA%\<bundle identifier>\config.json`
/// —— 与 Tauri 的 `app_data_dir()` 同构，但不依赖 `AppHandle`。
///
/// 为什么要这个：`run()` 在 `setup` 之前就要打日志（单实例检测、启动横幅前半段），
/// 而那时拿不到 `AppHandle`。日志系统需要提前知道 `cache_dir` 才能把这几行写进
/// 用户真正在看的那个 log，否则它们会落到 %LOCALAPPDATA% 下的另一份文件里。
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|p| PathBuf::from(p).join("com.kunge.selfclipboard").join("config.json"))
}

pub fn init(app: &tauri::AppHandle) {
    let path = default_config_path().unwrap_or_else(|| {
        let dir = app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| PathBuf::from("."));
        dir.join("config.json")
    });
    let _ = CONFIG_PATH.set(path);

    let cfg = load();
    // 启动快照：只打诊断需要的字段，**不 Debug 整份**。
    // 原因：`update_cache.notes` 装着整篇 release notes（几 KB），
    // 用 `{:?}` 打一份配置就等于每次启动往日志里灌一篇文案。
    // 用户在意的定位信息是「数据在哪、留多久、开了哪些开关」。
    crate::log_print!(
        "[settings] 配置就绪：缓存={} 保留={}天 主题={} 缩放跟随={} 文件复制={} 圆角={} 材质={}/{}",
        cfg.cache_dir.as_deref().unwrap_or("(默认 %APPDATA%)"),
        cfg.retention_days,
        cfg.theme_mode,
        !cfg.ignore_dpi_scale,
        cfg.file_copy,
        cfg.round_mode,
        cfg.alpha_straight,
        cfg.alpha_round
    );
    let _ = CONFIG.set(Mutex::new(cfg));

    // 版本升级检测：把上次运行的版本留作「升级起点」。
    //
    // 放在 init 末尾而不是读配置时做：这里 CONFIG 已经就位，`update` 能正常落盘。
    // 判据只看两个版本号是否相同 —— 降级（用户装回旧版）同样会记录起点，
    // 关于页显示的是「旧版到当前之间」的日志，语义仍然成立。
    let cur = env!("CARGO_PKG_VERSION");
    update(|c| record_upgrade(c, cur));
}

/// 记录「本次是从哪一版升上来的」，供关于页显示跨版本的完整更新日志。
///
/// 抽成独立函数是为了能单测：这段逻辑错了，表现是「升级后看不到中间版本的说明」，
/// 或者更糟 —— 把起点清空导致永远只显示一节，而这类问题在开发机上很难复现
/// （得真的装旧版再升级）。
fn record_upgrade(c: &mut Config, cur: &str) {
    if c.last_run_version == cur {
        return; // 同版本：什么都不动
    }
    if c.last_run_version.is_empty() {
        // 首次安装：没有「从哪一版来」，关于页只显示当前版本一节
        c.upgraded_from.clear();
    } else {
        // 升级（或用户装回旧版）：留下起点。起点是**上一版**，不是更早那一次。
        c.upgraded_from = c.last_run_version.clone();
    }
    c.last_run_version = cur.to_string();
}

/// 落盘配置。**原子写 + 失败必报**。
///
/// 两件事都不能省：
/// 1. 原子性：配置文件里有 `cache_dir` —— 它一丢，程序就回默认目录，
///    用户看到的是「记录全没了」（其实都在旧目录里）。`fs::write` 直接覆盖是
///    非原子的，强杀/断电会留下半截 JSON，下次解析失败直接回落默认值。
/// 2. 可观测性：以前是 `let _ = fs::write(...)`，写失败一个字都不记 ——
///    用户改完设置、重启回退，日志里查不出任何痕迹，是最难定位的一类问题。
fn persist(path: &std::path::Path, cfg: &Config) {
    let s = match serde_json::to_string_pretty(cfg) {
        Ok(s) => s,
        Err(e) => {
            crate::log_err!("settings", "配置序列化失败：{e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, s.as_bytes()) {
        crate::log_err!("settings", "配置写入失败（{}）：{e}", tmp.display());
        let _ = fs::remove_file(&tmp);
        return;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        crate::log_err!("settings", "配置原子替换失败（{}）：{e}", path.display());
        let _ = fs::remove_file(&tmp);
    }
}

fn load() -> Config {
    let Some(path) = CONFIG_PATH.get() else {
        crate::log_warn!("[settings] 配置路径未初始化，使用默认配置");
        return Config::default();
    };
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                crate::log_print!("[settings] 无 config.json（首次运行），使用默认配置");
            } else {
                // 读不动 ≠ 没有：权限、被占用、磁盘错误都会走到这里，
                // 以前一律静默回落默认值，用户会发现「所有设置都重置了」却查不到原因。
                crate::log_err!("settings", "配置读取失败（{}）：{e}", path.display());
            }
            return Config::default();
        }
    };
    let mut cfg: Config = serde_json::from_str(&raw).unwrap_or_else(|e| {
        // 损坏的配置必须留一份：里面带着 cache_dir、保留天数等用户数据定位信息，
        // 就地回落默认值等于把「为什么找不到历史」的唯一线索也删了。
        let dst = path.with_extension("json.corrupt");
        match fs::write(&dst, &raw) {
            Ok(()) => crate::log_err!(
                "settings",
                "config.json 解析失败：{e}；原文件已另存为 {}（本次使用默认配置）",
                dst.display()
            ),
            Err(err) => crate::log_err!(
                "settings",
                "config.json 解析失败：{e}；备份也失败：{err}（本次使用默认配置）"
            ),
        }
        Config::default()
    });
    // 老配置的单一浓度归进对应模式那一档，并立刻回写 ——
    // 否则每次启动都做一遍同样的迁移（虽然结果一致，但没必要）。
    if cfg.migrate() {
        crate::log_print!("[settings] 已迁移旧版材质浓度字段，回写配置");
        persist(path, &cfg);
    }
    cfg
}

pub fn get() -> Config {
    CONFIG
        .get()
        .and_then(|m| m.lock().ok())
        .map(|c| c.clone())
        .unwrap_or_default()
}

pub fn update<F: FnOnce(&mut Config)>(f: F) -> Config {
    let cfg = CONFIG
        .get()
        .map(|m| {
            let mut c = m.lock().unwrap();
            f(&mut c);
            let snapshot = c.clone();
            if let Some(path) = CONFIG_PATH.get() {
                persist(path, &snapshot);
            }
            snapshot
        })
        .unwrap_or_else(get);
    cfg
}

// ---------------------------------------------------------------------------
// 对前端的命令
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_settings() -> serde_json::Value {
    let cfg = get();
    serde_json::json!({
        "trayOnClose": cfg.tray_on_close,
        "cleanOnClose": cfg.clean_on_close,
        "cleanIncludePinned": cfg.clean_include_pinned,
        "cleanConfirmExit": cfg.clean_confirm_exit,
        "retentionDays": cfg.retention_days,
        "pruneIncludePinned": cfg.prune_include_pinned,
        // 当前模式生效的那一档（前端滑块直接用这个）
        "materialAlpha": cfg.alpha(),
        // 两档原值：前端切换模式后可以立刻更新界面，不必再拉一次配置
        "alphaStraight": cfg.alpha_straight,
        "alphaRound": cfg.alpha_round,
        "roundMode": cfg.round_mode,
        "fileCopy": cfg.file_copy,
        "deleteFilesWithItem": cfg.delete_files_with_item,
        "deleteFilesPermanent": cfg.delete_files_permanent,
        "pinGlobal": cfg.pin_global,
        "pinCancelOnDisable": cfg.pin_cancel_on_disable,
        "cacheDir": cfg.cache_dir,
        "autoStart": autostart::is_enabled(),
        "askedAutostart": cfg.asked_autostart,
        "askedTrayOnClose": cfg.asked_tray_on_close,
        // 两个一起下发：前端只做 `tourSeenVersion < tourVersion` 比较，
        // 不自己再存一份版本号常量 —— 那样改后端时前端会静默不跟着变。
        "tourSeenVersion": cfg.tour_seen_version,
        "tourVersion": TOUR_VERSION,
        "ignoreDpiScale": cfg.ignore_dpi_scale,
        "themeMode": cfg.theme_mode,
    })
}

/// 记下「首次运行引导」的某一项已经问过，之后不再打扰。
///
/// kind：`autostart`（首次运行问开机自启）/ `tray`（首次点关闭问后台运行）
/// / `tour`（功能导览看完或跳过）。
/// 用户选什么都算问过——选了关掉也不该每次启动都再问一遍；
/// 真正的开关随时可以在「设置」里改。
#[tauri::command]
pub fn dismiss_onboarding(kind: String) -> Result<(), String> {
    match kind.as_str() {
        "autostart" => {
            update(|c| c.asked_autostart = true);
        }
        "tray" => {
            update(|c| c.asked_tray_on_close = true);
        }
        // 导览的「跳过」「Esc」「点遮罩」也走这里 —— 任何退出方式都算看过，
        // 否则每次启动都弹，比不弹更烦。
        "tour" => {
            update(|c| c.tour_seen_version = TOUR_VERSION);
        }
        other => return Err(format!("未知的引导项：{other}")),
    }
    Ok(())
}

#[tauri::command]
pub fn set_clean_on_close(value: bool) -> Result<(), String> {
    update(|c| c.clean_on_close = value);
    Ok(())
}

/// 「文件复制」开关（设置页，默认关闭）
#[tauri::command]
pub fn set_file_copy(value: bool) -> Result<(), String> {
    update(|c| c.file_copy = value);
    Ok(())
}

/// 开关「删除文件条目时同时处理本地文件」。默认关闭，必须用户到设置里主动打开。
#[tauri::command]
pub fn set_delete_files_with_item(value: bool) -> Result<(), String> {
    // 这一项动的是用户磁盘上的真实文件，开关本身值得留一条日志：
    // 以后若出现「文件怎么没了」，一眼能看出是不是这个开关被打开过。
    crate::log_print!(
        "[settings] 「删除时同时删本地文件」已{}",
        if value { "开启" } else { "关闭" }
    );
    update(|c| c.delete_files_with_item = value);
    Ok(())
}

/// 本地文件处理方式：`false` = 回收站（默认），`true` = 永久删除。
#[tauri::command]
pub fn set_delete_files_permanent(value: bool) -> Result<(), String> {
    crate::log_print!(
        "[settings] 本地文件处理方式：{}",
        if value { "永久删除（不可恢复）" } else { "移入回收站（可还原）" }
    );
    update(|c| c.delete_files_permanent = value);
    Ok(())
}

/// 切换「不跟随系统缩放」。改完**立刻**重算 webview 缩放与窗口尺寸，
/// 不用重启 —— 这个开关的效果是即时可见的，让人重启才生效反而反直觉。
#[tauri::command]
pub fn set_ignore_dpi_scale(app: tauri::AppHandle, value: bool) -> Result<(), String> {
    update(|c| c.ignore_dpi_scale = value);
    if let Some(w) = app.get_webview_window("main") {
        crate::effects::apply_dpi_policy(&w);
    }
    Ok(())
}

/// 切换主题模式：`light` / `dark` / `system`。
///
/// 改完立刻生效：窗口主题被钉住/交还系统后，前端的 `onThemeChanged` 会自己跟上，
/// 不需要重启 —— 深浅色是「点了就要看见」的那类设置。
#[tauri::command]
pub fn set_theme_mode(app: tauri::AppHandle, value: String) -> Result<(), String> {
    if !matches!(value.as_str(), "light" | "dark" | "system") {
        return Err(format!("未知主题模式：{value}"));
    }
    update(|c| c.theme_mode = value);
    // 托盘菜单窗口也要跟上，否则主界面深色、菜单浅色
    for label in ["main", "tray-menu"] {
        if let Some(w) = app.get_webview_window(label) {
            crate::effects::apply_theme_mode(&w);
        }
    }
    Ok(())
}

/// 「置顶互通」开关（设置页，默认关闭）
///
/// 改完设置必须把已有置顶记录归一到对应模式的表示，否则同一份数据里会同时存在
/// 「互通」与「按分类独立」两种写法，界面表现与前面对不上。
#[tauri::command]
pub fn set_pin_global(value: bool) -> Result<(), String> {
    // update 返回改动后的快照：归一动作要用到子选项当前的值
    let cfg = update(|c| c.pin_global = value);
    crate::clipboard::normalize_pins(value, cfg.pin_cancel_on_disable);
    Ok(())
}

/// 「关闭时取消同步置顶」子选项（设置页，默认开启）
///
/// 只存值，不立刻动数据 —— 它决定的是**下次关闭互通时**的归一动作。
#[tauri::command]
pub fn set_pin_cancel_on_disable(value: bool) -> Result<(), String> {
    update(|c| c.pin_cancel_on_disable = value);
    Ok(())
}

#[tauri::command]
pub fn set_clean_include_pinned(value: bool) -> Result<(), String> {
    update(|c| c.clean_include_pinned = value);
    Ok(())
}

/// 保留期到期清理是否连置顶/收藏一起删。
///
/// **刻意不在这里立刻执行一次清理**：勾上开关就立即删掉一批置顶条目，是很惊悚的
/// 交互。开关只决定「下次清理用什么规则」，启动时与改保留天数时才会真正跑。
#[tauri::command]
pub fn set_prune_include_pinned(value: bool) -> Result<(), String> {
    update(|c| c.prune_include_pinned = value);
    Ok(())
}

#[tauri::command]
pub fn set_clean_confirm_exit(value: bool) -> Result<(), String> {
    update(|c| c.clean_confirm_exit = value);
    Ok(())
}

/// 前端确认弹窗后调用：立即执行退出清理并退出进程
#[tauri::command]
pub fn exit_app(app: tauri::AppHandle) -> Result<(), String> {
    crate::clipboard::on_app_exit();
    app.exit(0);
    Ok(())
}

#[tauri::command]
pub fn set_tray_on_close(app: tauri::AppHandle, value: bool) -> Result<(), String> {
    update(|c| c.tray_on_close = value);
    // 托盘图标现在**常驻**（程序运行期间一直在），所以这个开关只决定「点关闭按钮」的
    // 行为：开 = 隐藏到托盘继续后台运行；关 = 直接退出。这里不再增删托盘图标。
    let _ = app;
    Ok(())
}

#[tauri::command]
pub fn set_retention_days(days: u32) -> Result<(), String> {
    update(|c| c.retention_days = days);
    // 立即按新天数清理一次
    crate::clipboard::prune_retention(days);
    Ok(())
}

#[tauri::command]
pub fn set_autostart(value: bool) -> Result<(), String> {
    // 开关是「界面显示的状态」，注册表才是「真正生效的状态」。两者一旦不一致
    // （注册表被安全软件拦、权限不足），用户只会觉得「开了但不开机启动」——
    // 必须在日志里留下到底是写失败还是读失败。
    match autostart::set_enabled(value) {
        Ok(()) => {
            crate::log_print!("[settings] 开机自启已{}", if value { "开启" } else { "关闭" });
            Ok(())
        }
        Err(e) => {
            crate::log_err!("settings", "设置开机自启失败（目标={value}）：{e}");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 升级起点的记录：关于页靠它显示「跨版本」的完整更新日志。
    ///
    /// 四种情况都要对，写错任一种的表现都是「日志显示不全」或「永远只显示一节」，
    /// 而这类问题只有真的装旧版再升级才会暴露 —— 所以在这里钉死。
    #[test]
    fn record_upgrade_tracks_origin_version() {
        // 1. 首次安装：没有起点
        let mut c = Config::default();
        record_upgrade(&mut c, "2.4.1");
        assert_eq!(c.last_run_version, "2.4.1");
        assert_eq!(c.upgraded_from, "");

        // 2. 正常升级：起点 = 上一版，且 last_run 前进
        let mut c = Config {
            last_run_version: "2.3.0".into(),
            ..Config::default()
        };
        record_upgrade(&mut c, "2.4.1");
        assert_eq!(c.upgraded_from, "2.3.0");
        assert_eq!(c.last_run_version, "2.4.1");

        // 3. 同版本再启动：起点保持不变（否则第二次启动就把跨越范围丢了）
        record_upgrade(&mut c, "2.4.1");
        assert_eq!(c.upgraded_from, "2.3.0");

        // 4. 装回旧版：起点换成被降级掉的那一版，语义仍是「从它到当前」
        record_upgrade(&mut c, "2.2.0");
        assert_eq!(c.upgraded_from, "2.4.1");
        assert_eq!(c.last_run_version, "2.2.0");
    }

    /// 功能导览的判定：**新装与升级都要弹一次**，弹过之后不再弹。
    ///
    /// 曾经的设计是「老用户视为已看过」（缺字段默认给当前版本），当时怕打扰；
    /// 但导览讲的是「这软件有什么功能」，升级上来的老用户才是最需要这份说明的人。
    /// 用户明确要求改成「更新到这版的都得默认弹一次」。
    #[test]
    fn tour_pops_once_for_both_new_install_and_upgrade() {
        // 全新安装（没有 config.json）走 impl Default → 0 → 小于当前版本 → 会弹
        assert_eq!(Config::default().tour_seen_version, 0);

        // 老配置：文件在、但没这个字段 → 同样视为 0 → 也会弹
        let upgraded: Config = serde_json::from_str("{}").expect("老 config.json 必须能解析");
        assert_eq!(upgraded.tour_seen_version, 0);

        // 弹过之后：`dismiss_onboarding` 写入当前版本号，此后不再弹
        let seen = format!(r#"{{"tour_seen_version":{}}}"#, TOUR_VERSION);
        let cfg: Config = serde_json::from_str(&seen).unwrap();
        assert_eq!(cfg.tour_seen_version, TOUR_VERSION);
        assert!(
            cfg.tour_seen_version >= TOUR_VERSION,
            "看过之后必须满足 >= 当前版本，否则会重复弹"
        );
    }
}

