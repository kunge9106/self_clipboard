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
    /// 自定义缓存目录（None = 使用 app_data_dir/clipboard）
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// 最近一次成功的更新检查结果（跨重启保留，用于设置红点与「最新版更新日志」）。
    /// 检查失败时保持原值不动 —— 离线重启不该把已知的新版本信息抹掉。
    #[serde(default)]
    pub update_cache: Option<UpdateCache>,
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
pub const ALPHA_MIN: u32 = 40;
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
            alpha_straight: default_alpha_straight(),
            alpha_round: default_alpha_round(),
            legacy_alpha: None,
            asked_autostart: false,
            asked_tray_on_close: false,
            round_mode: false,
            file_copy: false, // 默认关闭：不记录文件复制
            pin_global: false, // 默认关闭：置顶按分类独立（沿用既有行为）
            cache_dir: None,
            update_cache: None,
        }
    }
}

static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init(app: &tauri::AppHandle) {
    let dir = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    let _ = CONFIG_PATH.set(dir.join("config.json"));

    let cfg = load();
    println!("[settings] 已加载配置：{:?}", cfg);
    let _ = CONFIG.set(Mutex::new(cfg));
}

fn load() -> Config {
    let Some(path) = CONFIG_PATH.get() else {
        return Config::default();
    };
    match fs::read_to_string(path) {
        Ok(s) => {
            let mut cfg: Config = serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("[settings] config.json 解析失败：{e}，使用默认配置");
                Config::default()
            });
            // 老配置的单一浓度归进对应模式那一档，并立刻回写 ——
            // 否则每次启动都做一遍同样的迁移（虽然结果一致，但没必要）。
            if cfg.migrate() {
                if let Ok(out) = serde_json::to_string_pretty(&cfg) {
                    let _ = fs::write(path, out);
                }
            }
            cfg
        }
        Err(_) => Config::default(),
    }
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
                if let Ok(s) = serde_json::to_string_pretty(&snapshot) {
                    let _ = fs::write(path, s);
                }
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
        // 当前模式生效的那一档（前端滑块直接用这个）
        "materialAlpha": cfg.alpha(),
        // 两档原值：前端切换模式后可以立刻更新界面，不必再拉一次配置
        "alphaStraight": cfg.alpha_straight,
        "alphaRound": cfg.alpha_round,
        "roundMode": cfg.round_mode,
        "fileCopy": cfg.file_copy,
        "pinGlobal": cfg.pin_global,
        "cacheDir": cfg.cache_dir,
        "autoStart": autostart::is_enabled(),
        "askedAutostart": cfg.asked_autostart,
        "askedTrayOnClose": cfg.asked_tray_on_close,
    })
}

/// 记下「首次运行引导」的某一项已经问过，之后不再打扰。
///
/// kind：`autostart`（首次运行问开机自启）/ `tray`（首次点关闭问后台运行）。
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

/// 「置顶互通」开关（设置页，默认关闭）
///
/// 改完设置必须把已有置顶记录归一到对应模式的表示，否则同一份数据里会同时存在
/// 「互通」与「按分类独立」两种写法，界面表现与前面对不上。
#[tauri::command]
pub fn set_pin_global(value: bool) -> Result<(), String> {
    update(|c| c.pin_global = value);
    crate::clipboard::normalize_pins(value);
    Ok(())
}

#[tauri::command]
pub fn set_clean_include_pinned(value: bool) -> Result<(), String> {
    update(|c| c.clean_include_pinned = value);
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
    autostart::set_enabled(value)
}
