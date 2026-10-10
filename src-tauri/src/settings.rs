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
    /// 材质浓度：窗口主体层的不透明度百分比（25–90，默认 62）
    #[serde(default = "default_material_alpha")]
    pub material_alpha: u32,
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
}

fn default_retention() -> u32 {
    3
}

fn default_material_alpha() -> u32 {
    // 现在这个值控制的是 CSS 主体层的不透明度（acrylic 的 tint 已固定压到 8%），
    // 62% 相当于「看得出模糊、文字也清楚」的中间值
    62
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
            round_mode: false,
            file_copy: false, // 默认关闭：不记录文件复制
            cache_dir: None,
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
    })
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
    // 开关即时生效：关闭 → 移除托盘；开启 → 不立即建托盘，等首次关闭时再建
    if !value {
        crate::tray::remove_tray();
    }
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
