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
    /// 材质浓度：窗口主体层的不透明度百分比（40–90）
    ///
    /// 默认取「直角模式」的推荐档（见 `default_material_alpha`）。
    /// 圆角模式没有背景模糊，面板要更实，所以 `set_round_mode` 会按模式自动切档 ——
    /// **但仅限用户没有手动调过**，见 `alpha_is_custom`。
    #[serde(default = "default_material_alpha")]
    pub material_alpha: u32,
    /// 用户是否手动调过材质浓度。
    ///
    /// `None` = 老配置里没有这个字段，用数值反推（见 `alpha_is_custom`）。
    /// 一旦用户在设置页拖过滑块，就永远以用户的值优先，
    /// 开关圆角模式不再动它 —— 之前无条件覆盖，用户调好的浓度会被悄悄改掉。
    #[serde(default)]
    pub material_alpha_custom: Option<bool>,
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

/// 直角 / 圆角模式各自的推荐浓度。这两个值只在**用户没手动调过**时自动生效。
pub const STRAIGHT_ALPHA: u32 = 50;
pub const ROUND_ALPHA: u32 = 90;

fn default_material_alpha() -> u32 {
    // 新装默认 50（直角模式的推荐档）。以前是 62，但 62 既不是直角档也不是圆角档，
    // 会让「没手动调过」这个判断失效（见 alpha_is_custom）。
    STRAIGHT_ALPHA
}

impl Config {
    /// 用户是否手动调过材质浓度。
    ///
    /// 老配置没有 `material_alpha_custom` 字段（None），这时用数值反推：
    /// 50 / 90 正是两种模式的自动档，除此之外的值必然是人手动拖出来的。
    /// 新配置由 `Config::default()` 明确写成 `Some(false)`，不再依赖反推。
    pub fn alpha_is_custom(&self) -> bool {
        self.material_alpha_custom
            .unwrap_or(self.material_alpha != STRAIGHT_ALPHA && self.material_alpha != ROUND_ALPHA)
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
            material_alpha: default_material_alpha(),
            // 新装即「没手动调过」——不需要靠数值反推
            material_alpha_custom: Some(false),
            asked_autostart: false,
            asked_tray_on_close: false,
            round_mode: false,
            file_copy: false, // 默认关闭：不记录文件复制
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
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("[settings] config.json 解析失败：{e}，使用默认配置");
            Config::default()
        }),
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
        "materialAlpha": cfg.material_alpha,
        "roundMode": cfg.round_mode,
        "fileCopy": cfg.file_copy,
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
