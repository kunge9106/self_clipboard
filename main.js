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

const { app, BrowserWindow, ipcMain, nativeImage, clipboard, nativeTheme } = require("electron");
const path = require("path");

// ========== 应用配置 ==========

/** 历史记录最大保留条数 */
// 不限制历史记录条数

/** 剪贴板轮询间隔（毫秒） */
const POLL_INTERVAL = 1000;

// ========== 全局状态 ==========

/** 主窗口引用 */
let mainWindow = null;

/** 剪贴板历史记录列表 */
let history = [];

/** 上一次读取到的剪贴板文本 */
let lastClipboardText = "";

/** 上一次读取到的剪贴板图片哈希值 */
let lastClipboardImageHash = null;

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
        const dataUrl = `data:image/png;base64,${pngBuffer.toString("base64")}`;
        addHistoryItem({ type: "image", content: dataUrl });
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

  history.unshift(entry);
  

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
    const img = nativeImage.createFromDataURL(item.content);
    clipboard.writeImage(img);
    lastClipboardImageHash = getImageHash(img);
  }
});

/** 处理关闭窗口请求 */
ipcMain.handle("close-window", () => {
  console.log("[ipc] close-window");
  if (mainWindow) mainWindow.close();
});

/** 处理最小化窗口请求 */
ipcMain.handle("minimize-window", () => {
  console.log("[ipc] minimize-window");
  if (mainWindow) mainWindow.minimize();
});

/** 处理清空请求 */
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
  history = history.filter(i => i.id !== itemId);
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
ipcMain.handle("set-auto-start", (event, enable) => {
  if (!app.isPackaged) {
    console.log("[auto-start] skipped in dev mode");
    return;
  }
  const { execSync } = require("child_process");
  // ?? template literal ??????
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
      // ?? Chromium ??????????????
      enablePreferredSizeMode: false,
    },
    // ?????????
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

  // ??????/????????-webkit-app-region:drag ?? DOM ???
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
  mainWindow.on("will-move", () => {
    try { mainWindow.webContents.send("close-panels"); } catch(e) {}
  });

  // ???????? acrylic ?????????
  let themeReady = false;
  setTimeout(() => { themeReady = true; console.log("[window] theme handler ready"); }, 2000);
  
  const sendThemeCSS = () => {
    if (mainWindow && !mainWindow.isDestroyed() && themeReady) {
      const isDark = nativeTheme.shouldUseDarkColors;
      console.log("[window] theme changed, isDark:", isDark);
      mainWindow.webContents.send("theme-changed", isDark);
    }
  };
  
  // ???????????? acrylic ????????
  let pendingTheme = null;
  nativeTheme.on("updated", () => {
    if (themeReady) {
      setTimeout(sendThemeCSS, 50);
    } else {
      // acrylic ??????????
      pendingTheme = setTimeout(sendThemeCSS, 50);
      console.log("[window] theme change queued (acrylic not ready)");
    }
  });
  // ??/????????? acrylic ????
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

  mainWindow.on("closed", () => {
    console.log("[window] closed");
    mainWindow = null;
  });

  // 页面加载完成后推送历史
  mainWindow.webContents.on("did-finish-load", () => {
    console.log("[window] page loaded");
    sendHistoryToRenderer();
    // acrylic ?????????????? DWM ???????
    setTimeout(() => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        try { mainWindow.setBackgroundMaterial("none"); } catch(e) {}
        setTimeout(() => {
          if (mainWindow && !mainWindow.isDestroyed()) {
            try { mainWindow.setBackgroundMaterial("acrylic"); } catch(e) {}
            console.log("[window] acrylic warm-up done");
          }
        }, 200);
      }
    }, 500);
    // ? acrylic ????????????? themeReady ???
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
  
  // ????????????????? exe + HKLM ??????
  if (app.isPackaged) {
    const { execSync } = require("child_process");
    const { homedir } = require("os");
    const path = require("path");

    // 1. Clean known self_clipboard entries with electron.exe
    const regLocations = [
      "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
      "HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
      "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\RunOnce",
    ];
    for (const loc of regLocations) {
      try {
        const buf = execSync(`reg query "${loc}" /v self_clipboard`, { timeout: 3000 });
        if (buf.toString().includes("electron.exe")) {
          execSync(`reg delete "${loc}" /v self_clipboard /f`, { timeout: 3000 });
          console.log("[cleanup] deleted stale self_clipboard with electron.exe");
        }
      } catch(e) { /* key not found */ }
    }

    // 2. Full scan: delete ANY value containing electron.exe in Run keys
    for (const loc of regLocations) {
      try {
        const buf = execSync(`reg query "${loc}"`, { timeout: 3000 });
        const lines = buf.toString().split(/\r?\n/);
        for (const line of lines) {
          if (line.includes("electron.exe") && line.includes("REG_")) {
            const parts = line.trim().split(/\s{2,}/);
            const valName = parts[0] || "";
            if (valName && valName !== "(Default)") {
              try {
                execSync(`reg delete "${loc}" /v "${valName}" /f`, { timeout: 3000 });
                console.log("[cleanup] deleted stale entry:", valName);
              } catch(e2) { /* skip */ }
            }
          }
        }
      } catch(e) { /* key not found */ }
    }

    // 3. Clean Startup folder shortcuts
    try {
      const startupDir = path.join(homedir(), "AppData", "Roaming", "Microsoft", "Windows", "Start Menu", "Programs", "Startup");
      const { readdirSync, unlinkSync } = require("fs");
      const files = readdirSync(startupDir);
      for (const f of files) {
        const lower = f.toLowerCase();
        if (lower.endsWith(".lnk") && (lower.includes("self") || lower.includes("clipboard") || lower.includes("electron") || lower.includes("light"))) {
          // Use PowerShell to read shortcut target
          try {
            const psCmd = "powershell -NoProfile -Command \"$w = New-Object -ComObject WScript.Shell; $s = $w.CreateShortcut('" + path.join(startupDir, f).replace(/'/g, "''") + "'); Write-Output $s.TargetPath\"";
            const psResult = execSync(psCmd, { timeout: 5000 });
            const target = psResult.toString().trim();
            if (target && (target.includes("electron.exe") || target.includes("self_clipboard"))) {
              unlinkSync(path.join(startupDir, f));
              console.log("[cleanup] deleted startup shortcut:", f);
            }
          } catch(e3) { /* skip */ }
        }
      }
    } catch(e) { /* startup dir not accessible */ }
  }

  createWindow();
  pollingTimer = setInterval(pollClipboard, POLL_INTERVAL);
  setTimeout(() => {
    console.log("[app] first poll");
    pollClipboard();
  }, 1000);
});

app.on("window-all-closed", () => {
  console.log("[app] all windows closed, quit");
  if (pollingTimer) clearInterval(pollingTimer);
  app.quit();
});

app.on("activate", () => {
  if (mainWindow === null) {
    console.log("[app] recreate window");
    createWindow();
  }
});
