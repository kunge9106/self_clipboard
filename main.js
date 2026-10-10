/**
 * self_clipboard — Windows 透明磨砂毛玻璃剪贴板工具
 *
 * 主进程文件：负责创建窗口、设置毛玻璃效果、监听系统剪贴板，
 * 并通过 IPC 与渲染进程通信。
 *
 * @module main
 */

const { app, BrowserWindow, ipcMain, nativeImage, clipboard, nativeTheme, Tray, Menu, shell } = require("electron");
const path = require("path");
const fs = require("fs");
const os = require("os");
const crypto = require("crypto");

// Suppress EPIPE errors in packaged app (console unavailable)
try { process.stdout.on("error", () => {}); process.stderr.on("error", () => {}); } catch (e) {}

// ========== 应用配置 ==========

/** 剪贴板轮询间隔（毫秒） */
const POLL_INTERVAL = 1000;

/**
 * 历史保留天数。0 = 不按时间清理（真正的无上限）。
 * 默认 3 保持与旧版行为一致；要无上限改成 0。
 */
const RETENTION_DAYS = 3;

/** 缩略图宽度（列表渲染用，避免解码全尺寸大图） */
const THUMB_WIDTH = 240;

/** 调试模式标识（命令行传 --dev 启用，或打包前始终开启） */
const isDev = process.argv.includes("--dev") || !app.isPackaged;

/** 统一日志：生产版静默，避免高频 IO 与剪贴板内容落日志 */
function log(...args) { if (isDev) console.log("[main]", ...args); }
function logAlways(...args) { console.log("[main]", ...args); }

// ========== 图片缓存目录 ==========

let IMAGE_DIR = null;

/** 缓存目录统计（增量维护，避免每次 IPC 全量 stat） */
let cacheStats = { dir: "", count: 0, size: 0, dirty: true };

function getDefaultCacheDir() {
  // 固定放在 userData：写入 Program Files 需要提权，且升级/卸载会丢数据
  const dir = path.join(app.getPath("userData"), "self_clipboard_images");
  try {
    if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
    return dir;
  } catch (e) {
    logAlways("[cache] create failed:", e.message);
    return dir;
  }
}

function getImageDir() {
  if (!IMAGE_DIR) {
    const cfg = loadConfig();
    const candidate = cfg.imageCacheDir || getDefaultCacheDir();
    IMAGE_DIR = candidate;
    try {
      if (!fs.existsSync(IMAGE_DIR)) fs.mkdirSync(IMAGE_DIR, { recursive: true });
    } catch (e) {
      logAlways("[cache] mkdir failed, fallback:", e.message);
      IMAGE_DIR = getDefaultCacheDir();
    }
    cacheStats.dirty = true;
    logAlways("[cache] image dir:", IMAGE_DIR);
  }
  return IMAGE_DIR;
}

function markCacheDirty() { cacheStats.dirty = true; }

function recomputeCacheStats() {
  const dir = getImageDir();
  let count = 0, size = 0;
  try {
    if (fs.existsSync(dir)) {
      for (const f of fs.readdirSync(dir)) {
        // thumb_ 是列表专用缩略图，不计入用户缓存统计
        if (f.endsWith(".png") && !f.startsWith("thumb_")) {
          count++;
          try { size += fs.statSync(path.join(dir, f)).size; } catch (e) {}
        }
      }
    }
  } catch (e) {}
  cacheStats = { dir, count, size, dirty: false };
  return cacheStats;
}

function getCacheStats() {
  if (cacheStats.dirty || cacheStats.dir !== getImageDir()) recomputeCacheStats();
  return cacheStats;
}

// ========== 路径工具 ==========

/** file:// URL -> 本地路径（统一处理编码与分隔符） */
function fileUrlToPath(url) {
  if (typeof url !== "string") return "";
  if (url.indexOf("file:///") !== 0) return url;
  let p = url.slice("file:///".length).replace(/^\/+/, "");
  try { p = decodeURIComponent(p); } catch (e) {}
  return path.normalize(p);
}

/** 本地路径 -> file:// URL */
function pathToFileUrl(p) {
  return "file:///" + encodeURI(p.replace(/\\/g, "/").replace(/^\/+/, ""));
}

/** 判断 target 是否位于 dir 之内（防目录穿越） */
function isPathInside(target, dir) {
  if (!target || !dir) return false;
  const rt = path.resolve(target);
  const rd = path.resolve(dir);
  return rt === rd || rt.startsWith(rd + path.sep);
}

// ========== 图片存取 ==========

/**
 * 保存图片到缓存目录，同时生成一张缩略图
 * @param {Buffer} pngBuffer 原图 PNG 数据
 * @param {Electron.NativeImage} img 原图对象（用于生成缩略图）
 * @returns {Promise<{content:string, thumb:string}|null>} 文件 URL；失败返回 null
 */
async function saveImageToFile(pngBuffer, img) {
  try {
    const dir = getImageDir();
    if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
    const stamp = Date.now() + "_" + Math.random().toString(36).slice(2, 8);
    const filePath = path.join(dir, "img_" + stamp + ".png");
    await fs.promises.writeFile(filePath, pngBuffer);

    let thumbPath = "";
    try {
      if (img && !img.isEmpty()) {
        const t = img.resize({ width: THUMB_WIDTH, quality: "better" });
        thumbPath = path.join(dir, "thumb_" + stamp + ".png");
        await fs.promises.writeFile(thumbPath, t.toPNG());
      }
    } catch (e) {
      log("[image] thumbnail failed:", e.message);
    }

    if (!cacheStats.dirty && cacheStats.dir === dir) {
      cacheStats.count++;
      cacheStats.size += pngBuffer.length;
    } else {
      cacheStats.dirty = true;
    }

    return { content: pathToFileUrl(filePath), thumb: thumbPath ? pathToFileUrl(thumbPath) : "" };
  } catch (e) {
    logAlways("[image] save failed:", e.message);
    return null;
  }
}

/**
 * 删除缓存文件（含缩略图）
 * @returns {boolean} 是否真的删除了
 */
function deleteImageFile(filePathOrUrl) {
  if (!filePathOrUrl) return false;
  try {
    const p = fileUrlToPath(filePathOrUrl);
    if (!isPathInside(p, getImageDir())) {
      logAlways("[image] refuse delete outside cache dir:", p);
      return false;
    }
    if (fs.existsSync(p)) {
      fs.unlinkSync(p);
      cacheStats.dirty = true;
    }
    // 顺带清掉同名缩略图
    const base = path.basename(p);
    if (base.startsWith("img_")) {
      const tp = path.join(path.dirname(p), base.replace(/^img_/, "thumb_"));
      if (fs.existsSync(tp)) { try { fs.unlinkSync(tp); } catch (e) {} }
    }
    return true;
  } catch (e) {
    return false;
  }
}

// ========== 全局状态 ==========

let mainWindow = null;
let tray = null;

/** 剪贴板历史记录列表 */
let history = [];

/** 上一次读取到的剪贴板文本 */
let lastClipboardText = "";

/** 上一次剪贴板图片的精确哈希（仅在廉价指纹变化时更新） */
let lastClipboardImageHash = null;

/** 上一次剪贴板图片的廉价指纹（每秒比较，无需 PNG 编码） */
let lastClipboardImageFingerprint = null;

const HISTORY_PATH = path.join(app.getPath("userData"), "history.json");

// ========== 历史持久化（debounce，避免每次复制全量同步写盘） ==========

let saveTimer = null;

function saveHistoryNow() {
  if (saveTimer) { clearTimeout(saveTimer); saveTimer = null; }
  try {
    fs.writeFileSync(HISTORY_PATH, JSON.stringify(history), "utf-8");
  } catch (e) {
    logAlways("[history] save error:", e.message);
  }
}

function saveHistory() {
  if (saveTimer) return;
  saveTimer = setTimeout(() => {
    saveTimer = null;
    try {
      fs.promises.writeFile(HISTORY_PATH, JSON.stringify(history), "utf-8").catch((e) => {
        logAlways("[history] save error:", e.message);
      });
    } catch (e) {}
  }, 800);
}

function loadHistory() {
  try {
    if (fs.existsSync(HISTORY_PATH)) {
      const raw = JSON.parse(fs.readFileSync(HISTORY_PATH, "utf-8"));
      history = Array.isArray(raw) ? raw : [];
      logAlways("[history] loaded:", history.length);
    }
  } catch (e) {
    logAlways("[history] load error:", e.message);
    history = [];
  }
}

let pollingTimer = null;

// ========== 剪贴板图片指纹 ==========

/**
 * 廉价指纹：只取尺寸 + 采样像素，用于每秒判重。
 * 相比 toPNG()（完整 PNG 编码），toBitmap() 仅是一次内存拷贝，
 * 可避免「图片没变也要每秒编码一次」的 CPU 空转。
 */
function getImageFingerprint(img) {
  if (!img || img.isEmpty()) return null;
  try {
    const s = img.getSize();
    const bmp = typeof img.toBitmap === "function" ? img.toBitmap() : img.toPNG();
    let h = s.width + "x" + s.height + ":" + bmp.length;
    const step = Math.max(1, Math.floor(bmp.length / 2048));
    for (let i = 0; i < bmp.length; i += step) h += bmp[i].toString(36);
    return h;
  } catch (e) {
    return null;
  }
}

/** 精确哈希：只在廉价指纹变化后才调用（full PNG -> sha1） */
function hashPng(buf) {
  return crypto.createHash("sha1").update(buf).digest("hex").slice(0, 20);
}

// ========== 剪贴板轮询监听 ==========

let polling = false;

async function pollClipboard() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (polling) return;
  polling = true;
  try {
    // ---- 文本 ----
    const text = clipboard.readText();
    if (text && text.trim() && text !== lastClipboardText) {
      lastClipboardText = text;
      addHistoryItem({ type: "text", content: text });
    }

    // ---- 图片 ----
    const img = clipboard.readImage();
    if (!img.isEmpty()) {
      const fp = getImageFingerprint(img);
      // 指纹没变：直接跳过，不做 PNG 编码（这是最大的性能收益点）
      if (fp && fp !== lastClipboardImageFingerprint) {
        lastClipboardImageFingerprint = fp;
        const pngBuffer = img.toPNG();
        const hash = hashPng(pngBuffer);
        if (hash !== lastClipboardImageHash) {
          lastClipboardImageHash = hash;
          const saved = await saveImageToFile(pngBuffer, img);
          if (saved) {
            addHistoryItem({ type: "image", content: saved.content, thumb: saved.thumb });
          } else {
            // 落盘失败才退回内联 base64
            addHistoryItem({ type: "image", content: "data:image/png;base64," + pngBuffer.toString("base64") });
          }
        }
      }
    } else {
      lastClipboardImageFingerprint = null;
    }
  } catch (e) {
    // 剪贴板被其他程序占用时抛出异常属于正常情况，忽略即可
    log("[poll] clipboard access error:", e.message);
  } finally {
    polling = false;
  }
}

// ========== 历史记录管理 ==========

/** 按保留策略过滤后的可见历史（保证与推送给渲染层的数据一致） */
function getVisibleHistory() {
  if (RETENTION_DAYS <= 0) return history;
  const deadline = Date.now() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
  return history.filter((i) => i.timestamp >= deadline);
}

function addHistoryItem(item) {
  const entry = {
    id: crypto.randomUUID(),
    type: item.type,
    content: item.content,
    thumb: item.thumb || "",
    timestamp: Date.now(),
  };

  // 去重：和最新一条比对
  if (history.length > 0) {
    const last = history[0];
    if (last.type === entry.type && last.content === entry.content) {
      log("[history] duplicate, skip");
      return;
    }
  }

  history.unshift(entry);
  saveHistory();
  pushHistoryOp({ op: "add", item: entry });
}

function clearHistory() {
  history = [];
  markCacheDirty();
  logAlways("[history] cleared");
  pushHistoryOp({ op: "clear" });
}

// ========== 增量推送（不再每次全量序列化） ==========

function pushHistoryOp(payload) {
  if (mainWindow && !mainWindow.isDestroyed()) {
    try {
      mainWindow.webContents.send("clipboard-history", payload);
    } catch (e) {
      log("[history] send error:", e.message);
    }
  }
}

function sendHistorySnapshot() {
  pushHistoryOp({ op: "init", items: getVisibleHistory() });
}

// ========== 用户配置 ==========

const CONFIG_PATH = path.join(app.getPath("userData"), "config.json");

/** 允许渲染层写入的配置字段白名单 */
const CONFIG_KEYS = ["trayOnClose", "cleanImagesOnClose", "imageCacheDir"];

let configCache = null;

function loadConfig() {
  if (configCache) return configCache;
  try {
    if (fs.existsSync(CONFIG_PATH)) {
      configCache = JSON.parse(fs.readFileSync(CONFIG_PATH, "utf-8")) || {};
    }
  } catch (e) {}
  if (!configCache) configCache = {};
  return configCache;
}

function saveConfig(cfg) {
  configCache = cfg;
  try {
    fs.writeFileSync(CONFIG_PATH, JSON.stringify(cfg, null, 2), "utf-8");
  } catch (e) {
    logAlways("[config] save error:", e.message);
  }
}

/** 清理缓存目录：清空所有 png（含缩略图），可选是否清理历史 */
function purgeImageCache() {
  const dir = getImageDir();
  let removed = 0;
  try {
    if (fs.existsSync(dir)) {
      for (const f of fs.readdirSync(dir)) {
        if (f.endsWith(".png")) {
          try { fs.unlinkSync(path.join(dir, f)); removed++; } catch (e) {}
        }
      }
    }
  } catch (e) {}
  markCacheDirty();
  return removed;
}

// ========== IPC 通信 ==========

/** 处理复制请求：将指定条目写回系统剪贴板 */
ipcMain.handle("copy-to-clipboard", (event, item) => {
  if (!item || typeof item !== "object" || typeof item.content !== "string") return;
  log("[ipc] copy-to-clipboard:", item.type);
  try {
    if (item.type === "text") {
      clipboard.writeText(item.content);
      lastClipboardText = item.content;
    } else if (item.type === "image") {
      let img;
      if (item.content.indexOf("file:///") === 0) {
        const fp = fileUrlToPath(item.content);
        if (!isPathInside(fp, getImageDir())) {
          logAlways("[ipc] refuse to load image outside cache dir");
          return;
        }
        img = nativeImage.createFromPath(fp);
      } else if (item.content.indexOf("data:image/") === 0) {
        img = nativeImage.createFromDataURL(item.content);
      } else {
        return;
      }
      if (!img || img.isEmpty()) return;
      clipboard.writeImage(img);
      lastClipboardImageHash = hashPng(img.toPNG());
      lastClipboardImageFingerprint = getImageFingerprint(img);
    }
  } catch (e) {
    logAlways("[ipc] copy failed:", e.message);
  }
});

/** 处理关闭窗口请求 */
ipcMain.handle("close-window", () => {
  const cfg = loadConfig();
  if (cfg.trayOnClose) {
    if (mainWindow) mainWindow.hide();
    createTray();
  } else if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("show-close-dialog");
  }
});

/** 处理最小化窗口请求 */
ipcMain.handle("close-action", (event, action) => {
  log("[ipc] close-action:", action);
  if (action === "tray") {
    if (mainWindow) mainWindow.hide();
    createTray();
  } else if (action === "quit") {
    if (tray) { tray.destroy(); tray = null; }
    if (pollingTimer) clearInterval(pollingTimer);
    app.quit();
  }
});

ipcMain.handle("pick-cache-dir", async () => {
  const { dialog } = require("electron");
  const result = await dialog.showOpenDialog(mainWindow, {
    properties: ["openDirectory", "createDirectory"],
    title: "选择缓存目录",
  });
  if (!result.canceled && result.filePaths.length > 0) {
    const newDir = path.resolve(result.filePaths[0]);
    try {
      const stat = fs.statSync(newDir, { throwIfNoEntry: false });
      if (!stat || !stat.isDirectory()) return null;
    } catch (e) {
      return null;
    }
    // 同步移动已有缓存文件到新目录
    try {
      const oldDir = getImageDir();
      if (oldDir && path.resolve(oldDir) !== newDir && fs.existsSync(oldDir)) {
        for (const f of fs.readdirSync(oldDir)) {
          if (f.endsWith(".png")) {
            try { fs.renameSync(path.join(oldDir, f), path.join(newDir, f)); } catch (e) {}
          }
        }
        logAlways("[cache] moved files from", oldDir, "to", newDir);
      }
    } catch (e) {}
    IMAGE_DIR = newDir;
    markCacheDirty();
    return newDir;
  }
  return null;
});

ipcMain.handle("get-image-cache-info", () => {
  const s = getCacheStats();
  return { dir: s.dir, count: s.count, size: s.size };
});

ipcMain.handle("open-cache-dir", () => {
  const dir = getImageDir();
  if (!dir) return;
  try {
    // shell.openPath 对可执行文件是「执行」语义，必须先确认是目录
    const stat = fs.statSync(dir, { throwIfNoEntry: false });
    if (stat && stat.isDirectory()) shell.openPath(dir);
  } catch (e) {}
});

ipcMain.handle("get-config", () => {
  return Object.assign({}, loadConfig());
});

/** 只接受白名单字段，避免渲染层写入任意键导致路径劫持 */
ipcMain.handle("set-config", (event, cfg) => {
  if (!cfg || typeof cfg !== "object") return;
  const next = Object.assign({}, loadConfig());
  for (const k of CONFIG_KEYS) {
    if (!(k in cfg)) continue;
    if (k === "imageCacheDir") {
      if (typeof cfg[k] !== "string" || !cfg[k]) continue;
      const p = path.resolve(cfg[k]);
      const stat = fs.statSync(p, { throwIfNoEntry: false });
      if (!stat || !stat.isDirectory()) {
        logAlways("[config] reject imageCacheDir (not a directory):", p);
        continue;
      }
      next[k] = p;
    } else {
      next[k] = !!cfg[k];
    }
  }
  IMAGE_DIR = null;
  saveConfig(next);
  logAlways("[config] saved:", JSON.stringify(next));
});

ipcMain.handle("minimize-window", () => {
  if (mainWindow) mainWindow.minimize();
});

ipcMain.handle("refresh-acrylic", () => {
  refreshAcrylic(0);
});

/** 窗口拖拽：主进程只负责定位，节流由渲染层 rAF 保证 */
ipcMain.on("window-drag", (event, dx, dy) => {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) return;
  try {
    const [x, y] = mainWindow.getPosition();
    mainWindow.setPosition(x + Math.round(dx), y + Math.round(dy));
  } catch (e) {}
});

ipcMain.handle("clear-all", () => {
  logAlways("[ipc] clear-all");
  purgeImageCache();
  clearHistory();
  saveHistory();
});

ipcMain.handle("get-history", () => {
  return getVisibleHistory();
});

ipcMain.handle("delete-item", (event, itemId) => {
  // 兼容旧版本 numeric id
  const idx = history.findIndex((i) => String(i.id) === String(itemId));
  if (idx < 0) return;
  const deleted = history[idx];
  if (deleted.type === "image" && typeof deleted.content === "string") {
    deleteImageFile(deleted.content);
  }
  history.splice(idx, 1);
  saveHistory();
  pushHistoryOp({ op: "remove", id: deleted.id });
});

// ========== 开机自启 ==========

const RUN_KEY = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE = "self_clipboard";

/** 用 spawnSync 传参数数组，避免 execSync 的 cmd.exe 中转与命令拼接 */
function regQuery() {
  const { spawnSync } = require("child_process");
  const r = spawnSync("reg", ["query", RUN_KEY, "/v", RUN_VALUE], {
    windowsHide: true,
    encoding: "utf8",
    timeout: 3000,
  });
  return r.status === 0 && !!r.stdout;
}

ipcMain.handle("get-auto-start", () => {
  return regQuery();
});

ipcMain.handle("set-auto-start", (event, enable) => {
  if (!app.isPackaged) {
    logAlways("[auto-start] skipped in dev mode");
    return;
  }
  const { spawnSync } = require("child_process");
  const args = enable
    ? ["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", process.execPath, "/f"]
    : ["delete", RUN_KEY, "/v", RUN_VALUE, "/f"];
  const r = spawnSync("reg", args, { windowsHide: true, encoding: "utf8", timeout: 3000 });
  if (r.status === 0) {
    logAlways("[auto-start]", enable ? "enabled" : "disabled");
  } else {
    logAlways("[auto-start] failed:", (r.stderr || "").trim());
  }
});

// ========== 托盘 ==========

function createTray() {
  if (tray) return;
  try {
    const iconPath = path.join(__dirname, "assets", "icon.ico");
    const icon = nativeImage.createFromPath(iconPath).resize({ width: 16, height: 16 });
    tray = new Tray(icon);
    const ctxMenu = Menu.buildFromTemplate([
      { label: "打开界面", click: () => { if (mainWindow) { mainWindow.show(); mainWindow.focus(); } } },
      { type: "separator" },
      { label: "退出", click: () => {
        if (tray) { tray.destroy(); tray = null; }
        if (pollingTimer) clearInterval(pollingTimer);
        app.quit();
      } },
    ]);
    tray.setToolTip("轻贴 - 剪贴板管理器");
    tray.setContextMenu(ctxMenu);
    tray.on("double-click", () => { if (mainWindow) { mainWindow.show(); mainWindow.focus(); } });
    logAlways("[tray] created");
  } catch (e) {
    logAlways("[tray] create failed:", e.message);
  }
}

// ========== 窗口创建 ==========

// ========== 窗口材质（按 Windows 版本自适应） ==========
//
// Win11 22H2 (build >= 22621)  -> acrylic（DWM_SYSTEMBACKDROP_TYPE）
// Win11      (build >= 22000)  -> mica 兜底
// Win10 / 更早 / 非 Windows    -> 无系统模糊，走 CSS 半透明实体底
//
// Electron 的 acrylic 有个坑：setBackgroundMaterial 调用成功不代表真的画出来了，
// 必须在首帧后连续补涂几次，再配合一次窗口尺寸微调（强制 DWM 重算边框）才生效。
// 所以这里不做任何节流，宁可多调几次。

const WIN_BUILD = process.platform === "win32"
  ? parseInt(os.release().split(".")[2] || "0", 10)
  : 0;

let EFFECT_TIER = "solid"; // "acrylic" | "mica" | "solid"

function detectEffectTier() {
  if (process.platform !== "win32") return "solid";
  if (WIN_BUILD >= 22621) return "acrylic";
  if (WIN_BUILD >= 22000) return "mica";
  return "solid";
}

function applyWindowMaterial() {
  if (process.platform !== "win32") return;
  if (!mainWindow || mainWindow.isDestroyed()) return;
  const mat = EFFECT_TIER === "acrylic" ? "acrylic" : EFFECT_TIER === "mica" ? "mica" : null;
  if (!mat) return;
  try {
    mainWindow.setBackgroundMaterial(mat);
  } catch (e) {
    logAlways("[window] " + mat + " failed:", e.message);
  }
}

function refreshAcrylic(delay) {
  setTimeout(applyWindowMaterial, delay || 0);
}

/** 尺寸微调：强制 DWM 重算窗口边框，让 backdrop 真正生效 */
function nudgeWindowFrame() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  try {
    const b = mainWindow.getBounds();
    mainWindow.setBounds(b);
  } catch (e) {}
}

function createWindow() {
  EFFECT_TIER = detectEffectTier();
  logAlways("[window] effect tier:", EFFECT_TIER, "(build " + WIN_BUILD + ")");

  const iconPath = path.join(__dirname, "assets", "icon.ico");
  mainWindow = new BrowserWindow({
    icon: iconPath,
    width: 300,
    height: 510,
    resizable: true,
    frame: false,
    transparent: true,
    alwaysOnTop: false,
    skipTaskbar: false,
    title: "self_clipboard",
    minWidth: 300,
    minHeight: 510,
    show: true,
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      devTools: isDev,
      enablePreferredSizeMode: false,
    },
    backgroundColor: "#00000000",
  });

  // 仅 --dev 显式指定时才自动开 DevTools，npm start 保持干净窗口
  if (process.argv.includes("--dev")) mainWindow.webContents.openDevTools({ mode: "bottom" });

  // 拦截导航与新窗口：本地应用不应加载外部内容
  mainWindow.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  mainWindow.webContents.on("will-navigate", (e, url) => {
    if (!url.startsWith("file://")) e.preventDefault();
  });

  // 捕获渲染进程日志（仅 dev）
  if (isDev) {
    // 兼容新旧两种 console-message 回调签名
    mainWindow.webContents.on("console-message", (event, a, b) => {
      let msg = "";
      if (a && typeof a === "object" && typeof a.message === "string") msg = a.message;
      else if (typeof b === "string") msg = b;
      else if (typeof a === "string") msg = a;
      if (msg) console.log("[render]", msg);
    });
  }

  // 启用系统级毛玻璃（按版本自适应；acrylic 需要后续补涂才能真正渲染）
  applyWindowMaterial();

  // 监听鼠标/拖拽消息，关闭弹窗
  if (process.platform === "win32") {
    try {
      const closePanels = () => {
        try { mainWindow && mainWindow.webContents.send("close-panels"); } catch (e) {}
      };
      mainWindow.hookWindowMessage(0x0201, closePanels); // WM_LBUTTONDOWN
      mainWindow.hookWindowMessage(0x00A1, closePanels); // WM_NCLBUTTONDOWN
      mainWindow.hookWindowMessage(0x001A, () => {       // WM_SETTINGCHANGE
        setTimeout(() => refreshAcrylic(0), 100);
      });
    } catch (e) {
      logAlways("[window] hookWindowMessage failed:", e.message);
    }
  }

  mainWindow.on("blur", () => refreshAcrylic(100));
  mainWindow.on("will-move", () => {
    try { mainWindow.webContents.send("close-panels"); } catch (e) {}
  });

  // 主题与材质档位下发给渲染层，渲染层据此切换 CSS 底色
  const sendThemeCSS = () => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("theme-changed", {
        isDark: nativeTheme.shouldUseDarkColors,
        tier: EFFECT_TIER,
      });
    }
  };

  nativeTheme.on("updated", () => {
    refreshAcrylic(100);
    refreshAcrylic(300);
    setTimeout(sendThemeCSS, 150);
  });

  mainWindow.loadFile(path.join(__dirname, "renderer", "index.html"));

  mainWindow.on("show", () => refreshAcrylic(200));

  mainWindow.on("closed", () => {
    logAlways("[window] closed");
    mainWindow = null;
  });

  mainWindow.webContents.on("did-finish-load", () => {
    sendHistorySnapshot();
    // acrylic 必须在首帧后连续补涂 + 一次尺寸微调，否则不会真正渲染出来
    refreshAcrylic(100);
    refreshAcrylic(400);
    refreshAcrylic(900);
    setTimeout(nudgeWindowFrame, 600);
    setTimeout(sendThemeCSS, 1200);
  });
}

// ========== 应用生命周期 ==========

app.whenReady().then(() => {
  logAlways("[app] ready");
  loadHistory();

  // 初始化剪贴板基线，避免重启后把已有内容重复入库
  try {
    const txt = clipboard.readText();
    if (txt) lastClipboardText = txt;
    const img = clipboard.readImage();
    if (!img.isEmpty()) {
      lastClipboardImageFingerprint = getImageFingerprint(img);
      lastClipboardImageHash = hashPng(img.toPNG());
    }
  } catch (e) {
    logAlways("[app] clipboard init error:", e.message);
  }

  createWindow();
  createTray();

  // 启动 10s 后按保留策略清理过期条目
  if (RETENTION_DAYS > 0) {
    setTimeout(() => {
      const deadline = Date.now() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
      const old = history.length;
      const expired = history.filter((i) => i.timestamp < deadline);
      history = history.filter((i) => i.timestamp >= deadline);
      if (history.length !== old) {
        for (const item of expired) {
          if (item.type === "image") deleteImageFile(item.content);
        }
        saveHistory();
        sendHistorySnapshot();
        logAlways("[history] cleaned:", old, "->", history.length);
      }
    }, 10000);
  }

  pollingTimer = setInterval(pollClipboard, POLL_INTERVAL);
  setTimeout(pollClipboard, 1000);

  // 清理指向 electron.exe 的陈旧自启项
  if (app.isPackaged && mainWindow) {
    mainWindow.webContents.once("did-finish-load", () => {
      setTimeout(() => {
        try {
          const { spawnSync } = require("child_process");
          const r = spawnSync("reg", ["query", RUN_KEY, "/v", RUN_VALUE], {
            windowsHide: true, encoding: "utf8", timeout: 2000,
          });
          if (r.status === 0 && (r.stdout || "").includes("electron.exe")) {
            spawnSync("reg", ["delete", RUN_KEY, "/v", RUN_VALUE, "/f"], {
              windowsHide: true, timeout: 2000,
            });
            logAlways("[cleanup] fixed stale auto-start entry");
          }
        } catch (e) {}
      }, 5000);
    });
  }
});

app.on("before-quit", () => {
  const cfg = loadConfig();
  if (cfg.cleanImagesOnClose) {
    purgeImageCache();
    history = [];
  }
  saveHistoryNow();
});

app.on("window-all-closed", () => {
  logAlways("[app] all windows closed");
  if (!tray) {
    if (pollingTimer) clearInterval(pollingTimer);
    app.quit();
  }
});

app.on("activate", () => {
  if (mainWindow === null) createWindow();
});
