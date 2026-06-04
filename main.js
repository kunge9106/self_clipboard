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

const { app, BrowserWindow, ipcMain, nativeImage, clipboard } = require("electron");
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

// ========== 窗口创建 ==========

/**
 * 创建 Electron 主窗口
 * 应用 Windows Acrylic 磨砂毛玻璃效果
 */
function createWindow() {
  mainWindow = new BrowserWindow({
    width: 380,
    height: 600,
    resizable: true,
    frame: false,
    transparent: true,
    alwaysOnTop: true,
    skipTaskbar: false,
    title: "self_clipboard",
    minWidth: 300,
    minHeight: 350,
    show: true,
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      devTools: true,
    },
  });

  // 调试模式自动打开开发者工具
  if (isDev) {
    mainWindow.webContents.openDevTools({ mode: "bottom" });
  }

  // 捕获渲染进程日志
  mainWindow.webContents.on("console-message", (event, level, message) => {
    console.log("[render]", message);
  });

  // 启用 Windows 毛玻璃效果
  if (process.platform === "win32") {
    try {
      mainWindow.setBackgroundMaterial("acrylic");
      console.log("[window] acrylic enabled");
    } catch (e) {
      try {
        mainWindow.setBackgroundMaterial("mica");
        console.log("[window] mica enabled");
      } catch (e2) {
        console.log("[window] acrylic/mica not available, use css blur");
      }
    }
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
  });
}

// ========== 应用生命周期 ==========

app.whenReady().then(() => {
  console.log("[app] ready");
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




