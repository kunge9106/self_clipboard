/**
 * self_clipboard — Windows 透明磨砂毛玻璃剪贴板工具
 * 
 * 主进程文件：负责创建窗口、设置毛玻璃效果、监听系统剪贴板，
 * 并通过 IPC 与渲染进程通信。
 * 
 * 注意：console.log 使用英文，避免 Windows 控制台编码导致的乱码
 * 
 * @module main
 */

const { app, BrowserWindow, ipcMain, nativeImage, clipboard, nativeTheme, Tray, Menu } = require("electron");
const path = require("path");
const fs = require("fs");

// Suppress EPIPE errors in packaged app (console unavailable)
try { process.stdout.on("error", () => {}); process.stderr.on("error", () => {}); } catch(e) {}

// ========== 应用配置 ==========

/** 历史记录最大保留条数 */
// 不限制历史记录条数

/** 剪贴板轮询间隔（毫秒） */
const POLL_INTERVAL = 1000;
/** ?????? */
let IMAGE_DIR;

/**
 * ????????????C??
 */
function getDefaultCacheDir() {
  // 使用安装目录作为默认缓存目录
  const baseDir = app.isPackaged
    ? path.dirname(process.execPath)
    : __dirname;
  const dir = path.join(baseDir, "self_clipboard_images");
  try {
    if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
    fs.accessSync(dir, fs.constants.W_OK);
    return dir;
  } catch(e) {
    console.log("[cache] failed to create in install dir, fallback to userData");
    const fallback = path.join(app.getPath("userData"), "self_clipboard_images");
    try { if (!fs.existsSync(fallback)) fs.mkdirSync(fallback, { recursive: true }); } catch(e2) {}
    return fallback;
  }
}


function getImageDir() {
  if (!IMAGE_DIR) {
    // ??????????
    const cfg = loadConfig();
    IMAGE_DIR = cfg.imageCacheDir || getDefaultCacheDir();
    try {
      if (!fs.existsSync(IMAGE_DIR)) fs.mkdirSync(IMAGE_DIR, { recursive: true });
    } catch(e) {
      IMAGE_DIR = getDefaultCacheDir();
    }
    console.log("[image] cache dir:", IMAGE_DIR);
  }
  return IMAGE_DIR;
}

function saveImageToFile(pngBuffer) {
  try {
    const dir = getImageDir();
    if (!fs.existsSync(dir)) {
      fs.mkdirSync(IMAGE_DIR, { recursive: true });
    }
    const filename = "img_" + Date.now() + "_" + Math.random().toString(36).slice(2, 8) + ".png";
    const filePath = path.join(dir, filename);
    fs.writeFileSync(filePath, pngBuffer);
    return filePath;
  } catch(e) {
    console.log("[image] save failed:", e.message);
    return null;
  }
}

function deleteImageFile(filePath) {
  if (filePath) {
    try {
      const dir = getImageDir();
      if (filePath.indexOf(dir) === 0) {
        fs.unlinkSync(filePath);
      }
    } catch(e) {}
  }
}


// ========== 全局状态 ==========

/** 主窗口引用 */
let mainWindow = null;
let tray = null;

/** 剪贴板历史记录列表 */
let history = [];

/** 上一次读取到的剪贴板文本 */
let lastClipboardText = "";

/** 上一次读取到的剪贴板图片哈希值 */
let lastClipboardImageHash = null;
const HISTORY_PATH = path.join(app.getPath("userData"), "history.json");

function saveHistory() {
  try {
    fs.writeFileSync(HISTORY_PATH, JSON.stringify(history), "utf-8");
  } catch(e) {
    console.log("[history] save error:", e.message);
  }
}

function loadHistory() {
  try {
    if (fs.existsSync(HISTORY_PATH)) {
      const raw = JSON.parse(fs.readFileSync(HISTORY_PATH, "utf-8"));
      const threeDaysAgo = Date.now() - 3 * 24 * 60 * 60 * 1000;
      history = raw.filter(i => i.timestamp >= threeDaysAgo);
      for (const item of raw) {
        if (item.timestamp < threeDaysAgo && item.type === "image") {
          deleteImageFile(item.content);
        }
      }
      console.log("[history] loaded:", history.length, "/", raw.length);
    }
  } catch(e) {
    console.log("[history] load error:", e.message);
  }
}


/** 轮询定时器句柄 */
let pollingTimer = null;

/** 调试模式标识（命令行传 --dev 启用） */
const isDev = process.argv.includes("--dev");

// ========== 剪贴板图片哈希计算 ==========

/**
 * 计算图片内容的哈希值，用于判断剪贴板图片是否变化
 * 
 * @param {Electron.NativeImage} img - Electron 原生图片对象
 * @returns {number|null} 哈希值，图片为空时返回 null
 */
function getImageHash(img) {
  if (img.isEmpty()) return null;
  const buf = img.toPNG();
  let hash = 0;
  const len = Math.min(buf.length, 1024);
  for (let i = 0; i < len; i++) {
    hash = ((hash << 5) - hash) + buf[i];
    hash = hash & hash;
  }
  return hash;
}

// ========== 剪贴板轮询监听 ==========

/**
 * 定期轮询系统剪贴板，检测是否有新的文本或图片内容
 * 通过对比上一次缓存的内容来判断是否发生变化
 */
function pollClipboard() {
  if (!mainWindow) {
    console.log("[poll] mainWindow not ready, skip");
    return;
  }

  try {
    // 检查文本
    const text = clipboard.readText();
    const textPreview = text ? text.substring(0, 40) : "(empty)";
    console.log("[poll] text:", textPreview);

    if (text && text !== lastClipboardText && text.trim()) {
      console.log("[poll] new text detected");
      lastClipboardText = text;
      addHistoryItem({ type: "text", content: text });
    }

    // 检查图片
    const img = clipboard.readImage();
    if (!img.isEmpty()) {
      const hash = getImageHash(img);
      if (hash !== null && hash !== lastClipboardImageHash) {
        console.log("[poll] new image detected");
        lastClipboardImageHash = hash;
        const pngBuffer = img.toPNG();
        const filePath = saveImageToFile(pngBuffer);
        if (filePath) {
          const fileUrl = "file:///" + filePath.replace(/\\/g, "/");
          addHistoryItem({ type: "image", content: fileUrl });
        } else {
          const dataUrl = `data:image/png;base64,${pngBuffer.toString("base64")}`;
          addHistoryItem({ type: "image", content: dataUrl });
        }
      }
    }
  } catch (e) {
    // 剪贴板被其他程序占用时抛出异常属于正常情况，忽略即可
    console.log("[poll] clipboard access error:", e.message);
  }
}

// ========== 历史记录管理 ==========

/**
 * 向历史记录列表中添加新条目
 * 
 * @param {{type: string, content: string}} item - 要添加的条目
 */
function addHistoryItem(item) {
  const entry = {
    id: Date.now() + Math.random(),
    type: item.type,
    content: item.content,
    timestamp: Date.now(),
  };

  // 去重：和最新一条比对
  if (history.length > 0) {
    const last = history[0];
    if (last.type === entry.type && last.content === entry.content) {
      console.log("[history] duplicate, skip");
      return;
    }
  }

  history.unshift(entry);saveHistory();
  

  lastClipboardText = clipboard.readText();
  console.log("[history] added, total:", history.length);
  sendHistoryToRenderer();
}

/**
 * 清空全部历史记录
 */
function clearHistory() {
  history = [];
  console.log("[history] cleared");
  sendHistoryToRenderer();
}

/**
 * 将当前历史记录推送到渲染进程
 */
function sendHistoryToRenderer() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    try {
      mainWindow.webContents.send("clipboard-history", history);
    } catch (e) {
      console.log("[history] send error:", e.message);
    }
  }
}

// ========== IPC 通信 ==========

/** 处理复制请求：将指定条目写回系统剪贴板 */
ipcMain.handle("copy-to-clipboard", (event, item) => {
  console.log("[ipc] copy-to-clipboard:", item.type);
  if (item.type === "text") {
    clipboard.writeText(item.content);
    lastClipboardText = item.content;
  } else if (item.type === "image") {
    let img;
    if (item.content.startsWith("file://")) {
      const filePath = item.content.replace(/^file:\/\//, "").replace(/\//g, "\\");
      img = nativeImage.createFromPath(filePath);
    } else {
      img = nativeImage.createFromDataURL(item.content);
    }
    clipboard.writeImage(img);
    lastClipboardImageHash = getImageHash(img);
  }
});

/** 处理关闭窗口请求 */
ipcMain.handle("close-window", () => {
  console.log("[ipc] close-window");
  const cfg = loadConfig();
  if (cfg.trayOnClose) {
    // 不再提示，直接到托盘
    if (mainWindow) mainWindow.hide();
    createTray();
  } else {
    // 显示弹窗
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("show-close-dialog");
    }
  }
});

/** 处理最小化窗口请求 */
ipcMain.handle("close-action", (event, action) => {
  console.log("[ipc] close-action:", action);
  if (action === "tray") {
    if (mainWindow) mainWindow.hide();
    createTray();
  } else if (action === "quit") {
    if (tray) { tray.destroy(); tray = null; }
    if (pollingTimer) clearInterval(pollingTimer);
    app.quit();
  }
});


/** ???????? */
ipcMain.handle("pick-cache-dir", async () => {
  const { dialog } = require("electron");
  const result = await dialog.showOpenDialog(mainWindow, {
    properties: ["openDirectory", "createDirectory"],
    title: "选择缓存目录"
  });
  if (!result.canceled && result.filePaths.length > 0) {
    const newDir = result.filePaths[0];
    // 同步移动已有缓存文件到新目录
    try {
      const oldDir = getImageDir();
      if (oldDir && oldDir !== newDir && fs.existsSync(oldDir)) {
        const files = fs.readdirSync(oldDir);
        for (const f of files) {
          if (f.endsWith(".png")) {
            try {
              fs.renameSync(path.join(oldDir, f), path.join(newDir, f));
            } catch(e) { /* file may be in use, skip */ }
          }
        }
        console.log("[cache] moved", files.filter(f => f.endsWith(".png")).length, "files from", oldDir, "to", newDir);
      }
    } catch(e) {}
    IMAGE_DIR = newDir;
    return newDir;
  }
  return null;
});

ipcMain.handle("get-image-cache-info", () => {
  const dir = getImageDir();
  let count = 0, size = 0;
  try {
    if (fs.existsSync(dir)) {
      const files = fs.readdirSync(dir);
      for (const f of files) {
        if (f.endsWith(".png")) {
          count++;
          try { size += fs.statSync(path.join(dir, f)).size; } catch(e) {}
        }
      }
    }
  } catch(e) {}
  return { dir, count, size };
});

ipcMain.handle("get-config", () => {
  return loadConfig();
});

ipcMain.handle("set-config", (event, cfg) => {
  IMAGE_DIR = null;
  saveConfig(cfg);
  console.log("[config] saved:", cfg);
});

ipcMain.handle("minimize-window", () => {
  console.log("[ipc] minimize-window");
  if (mainWindow) mainWindow.minimize();
});

/** ?????? */
ipcMain.on("window-drag", (event, dx, dy) => {
  if (mainWindow) {
    const bounds = mainWindow.getBounds();
    mainWindow.setBounds({ x: bounds.x + dx, y: bounds.y + dy });
  }
});

/** ?????? */

ipcMain.handle("clear-all", () => {
  console.log("[ipc] clear-all");
  clearHistory();
});

/** 处理历史加载请求 */
ipcMain.handle("get-history", () => {
  return history;
});


// 删除指定条目
ipcMain.handle("delete-item", (event, itemId) => {
  // delete cache file
  const deleted = history.find(i => i.id === itemId);
  if (deleted && deleted.type === "image" && typeof deleted.content === "string" && deleted.content.startsWith("file://")) {
    deleteImageFile(deleted.content.replace(/^file:\/\//, "").replace(/\//g, "\\"));
  }
  history = history.filter(i => i.id !== itemId);
  saveHistory();
  sendHistoryToRenderer();
});


// 开机自启（注册表方式，dev 模式也生效）
ipcMain.handle("get-auto-start", () => {
  try {
    const { execSync } = require("child_process");
    const buf = execSync("reg query HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run /v self_clipboard", { timeout: 3000 });
    const ok = buf.toString().includes("self_clipboard");
    console.log("[auto-start] check:", ok);
    return ok;
  } catch(e) {
    console.log("[auto-start] check: key not found");
    return false;
  }
});
ipcMain.handle("refresh-acrylic", () => {
  if (mainWindow && !mainWindow.isDestroyed()) {
    try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
    console.log("[window] acrylic refreshed (IME/composition)");
  }
});

ipcMain.handle("set-auto-start", (event, enable) => {
  if (!app.isPackaged) {
    console.log("[auto-start] skipped in dev mode");
    return;
  }
  const { execSync } = require("child_process");
  // 禁用 template literal ??????
  const exe = process.execPath;
  if (enable) {
    try {
      execSync(
        `reg add "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run" /v self_clipboard /t REG_SZ /d "${exe}" /f`,
        { timeout: 3000 }
      );
      console.log("[auto-start] enabled:", exe);
    } catch(e) {
      console.error("[auto-start] enable failed:", e.message);
    }
  } else {
    try {
      execSync(
        `reg delete "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run" /v self_clipboard /f`,
        { timeout: 3000 }
      );
      console.log("[auto-start] disabled - registry key removed");
    } catch(e) {
      console.log("[auto-start] disable - key not found");
    }
  }
});



// ========== 用户配置 ==========
const CONFIG_PATH = path.join(app.getPath("userData"), "config.json");

function loadConfig() {
  try {
    const fs = require("fs");
    if (fs.existsSync(CONFIG_PATH)) {
      return JSON.parse(fs.readFileSync(CONFIG_PATH, "utf-8"));
    }
  } catch(e) {}
  return {};
}

function saveConfig(cfg) {
  try {
    const fs = require("fs");
    fs.writeFileSync(CONFIG_PATH, JSON.stringify(cfg, null, 2), "utf-8");
  } catch(e) {}
}

// ========== 托盘 ==========

/**
 * 创建系统托盘图标
 */
function createTray() {
  if (tray) return;
  try {
    const iconPath = path.join(__dirname, "assets", "icon.ico");
    const icon = nativeImage.createFromPath(iconPath).resize({ width: 16, height: 16 });
    tray = new Tray(icon);
    const ctxMenu = Menu.buildFromTemplate([
      { label: "打开界面", click: () => {
        if (mainWindow) { mainWindow.show(); mainWindow.focus(); }
      }},
      { type: "separator" },
      { label: "退出", click: () => {
        if (tray) { tray.destroy(); tray = null; }
        if (pollingTimer) clearInterval(pollingTimer);
        app.quit();
      }},
    ]);
    tray.setToolTip("轻贴 - 剪贴板管理器");
    tray.setContextMenu(ctxMenu);
    tray.on("double-click", () => {
      if (mainWindow) { mainWindow.show(); mainWindow.focus(); }
    });
    console.log("[tray] created");
  } catch(e) {
    console.log("[tray] create failed:", e.message);
  }
}

// ========== 窗口创建 ==========
/**
 * 创建 Electron 主窗口
 * 应用 Windows Acrylic 磨砂毛玻璃效果
 */
function createWindow() {
  const iconPath = path.join(__dirname, "assets", "icon.ico");
  mainWindow = new BrowserWindow({
    icon: iconPath,
    width: 300,
    height: 485,
    resizable: true,
    frame: false,
    transparent: true,
    alwaysOnTop: false,
    skipTaskbar: false,
    title: "self_clipboard",
    minWidth: 300,
    minHeight: 485,
    show: true,
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      devTools: true,
      // 禁用 Chromium 自适应模式
      enablePreferredSizeMode: false,
    },
    // 等待背景材质初始化
    backgroundColor: "#00000000",
  });

  // 调试模式自动打开开发者工具
  if (isDev) {
    mainWindow.webContents.openDevTools({ mode: "bottom" });
  }

  // 捕获渲染进程日志
  mainWindow.webContents.on("console-message", (event, level, message) => {
    console.log("[render]", message);
  });

  // 启用 Windows 毛玻璃效果（增强版）
  if (process.platform === "win32") {
    let glassOk = false;
    try {
      mainWindow.setBackgroundMaterial("acrylic");
      console.log("[window] acrylic enabled");
      glassOk = true;
    } catch (e) {
      console.log("[window] acrylic failed:", e.message);
    }
    if (!glassOk) {
      try {
        mainWindow.setBackgroundMaterial("mica");
        console.log("[window] mica enabled");
        glassOk = true;
      } catch (e2) {
        console.log("[window] mica failed:", e2.message);
      }
    }
    if (!glassOk) {
      console.log("[window] acrylic/mica unavailable, CSS backdrop-filter is primary glass");
      // No destructive fallback, CSS backdrop-filter handles it
    }
  }

  // 监听鼠标/拖拽消息，关闭弹窗
  if (process.platform === "win32") {
    try {
      mainWindow.hookWindowMessage(0x0201, () => { // WM_LBUTTONDOWN
        try { mainWindow.webContents.send("close-panels"); } catch(e) {}
      });
      mainWindow.hookWindowMessage(0x00A1, () => { // WM_NCLBUTTONDOWN (drag)
        try { mainWindow.webContents.send("close-panels"); } catch(e) {}
      });
    } catch(e) {
      console.log("[window] hookWindowMessage failed:", e.message);
    }
  }
  // Re-apply acrylic on blur (fixes IME overlay graying)
  mainWindow.on("blur", () => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      setTimeout(() => {
        try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
      }, 100);
    }
  });

  mainWindow.on("will-move", () => {
    try { mainWindow.webContents.send("close-panels"); } catch(e) {}
  });

  // 监听窗口移动? acrylic ?????????
  let themeReady = false;
  setTimeout(() => { themeReady = true; console.log("[window] theme handler ready"); }, 2000);
  
  const sendThemeCSS = () => {
    if (mainWindow && !mainWindow.isDestroyed() && themeReady) {
      const isDark = nativeTheme.shouldUseDarkColors;
      console.log("[window] theme changed, isDark:", isDark);
      mainWindow.webContents.send("theme-changed", isDark);
    }
  };
  
  // 标记主题处理器就绪? acrylic ????????
  let pendingTheme = null;
  nativeTheme.on("updated", () => {
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
        console.log("[window] acrylic refreshed on theme switch");
      }
    }, 100);
    // Retry after 300ms for DWM to fully settle after theme change
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
        console.log("[window] acrylic re-refreshed on theme switch (retry)");
      }
    }, 300);
    if (themeReady) {
      setTimeout(sendThemeCSS, 150);
    } else {
      pendingTheme = setTimeout(sendThemeCSS, 150);
      console.log("[window] theme change queued (acrylic not ready)");
    }
  });
  // WM_SETTINGCHANGE ????? acrylic
  if (process.platform === "win32") {
    try {
      mainWindow.hookWindowMessage(0x001A, () => { // WM_SETTINGCHANGE
        setTimeout(() => {
          if (mainWindow && !mainWindow.isDestroyed()) {
            try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
          }
        }, 100);
      });
    } catch(e) {}
  }

  mainWindow.loadFile(path.join(__dirname, "renderer", "index.html"));

  // 从托盘显示时重新设置 acrylic
  mainWindow.on("show", () => {
    // Directly re-apply acrylic without none step (avoids gray flash)
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
        console.log("[window] acrylic re-applied on show");
      }
    }, 200);
  });

  mainWindow.on("closed", () => {
    console.log("[window] closed");
    mainWindow = null;
  });

  // 页面加载完成后推送历史
  mainWindow.webContents.on("did-finish-load", () => {
    console.log("[window] page loaded");
    sendHistoryToRenderer();
    // acrylic ???DWM ???
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
        console.log("[window] acrylic warm-up done");
      }
    }, 500);
    // 主题切换时重新应用
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send("theme-changed", nativeTheme.shouldUseDarkColors);
      }
    }, 1000);
  });
}

// ========== 应用生命周期 ==========

app.whenReady().then(() => {
  console.log("[app] ready");
  loadHistory();
  console.log("[history] loaded from disk, count:", history.length);
  loadHistory();
  console.log("[history] loaded from disk, count:", history.length);

  // 立即创建窗口
  createWindow();
  pollingTimer = setInterval(pollClipboard, POLL_INTERVAL);
  setTimeout(() => {
    console.log("[app] first poll");
    pollClipboard();
  }, 1000);

  // 延迟执行后台清理
  if (app.isPackaged) {
    mainWindow?.webContents?.once("did-finish-load", () => {
      setTimeout(() => {
        try {
          const { execSync } = require("child_process");
          const regLocations = ["HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run"];
          for (const loc of regLocations) {
            try {
              const buf = execSync(`reg query "${loc}" /v self_clipboard`, { timeout: 2000 });
              if (buf.toString().includes("electron.exe")) {
                execSync(`reg delete "${loc}" /v self_clipboard /f`, { timeout: 2000 });
                console.log("[cleanup] fixed stale entry");
              }
            } catch(e) { /* not found */ }
          }
        } catch(e) { /* skip */ }
        console.log("[app] startup cleanup done");
      }, 5000);
    });
  }
});


// ?????????
app.on("before-quit", () => {
  const cfg = loadConfig();
  if (cfg.cleanImagesOnClose) {
    const dir = getImageDir();
    try {
      if (fs.existsSync(dir)) {
        const files = fs.readdirSync(dir);
        for (const f of files) {
          if (f.endsWith(".png")) {
            try { fs.unlinkSync(path.join(dir, f)); } catch(e) {}
          }
        }
        console.log("[cleanup] image cache cleared");
      }
    } catch(e) {
      console.log("[cleanup] error:", e.message);
    }
  }
});
app.on("window-all-closed", () => {
  console.log("[app] all windows closed");
  if (!tray) {
    if (pollingTimer) clearInterval(pollingTimer);
    app.quit();
  }
});

app.on("activate", () => {
  if (mainWindow === null) {
    console.log("[app] recreate window");
    createWindow();
  }
});
