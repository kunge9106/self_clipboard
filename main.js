/**
 * self_clipboard — Windows 透明磨砂毛玻璃剪贴板工具
 * 
 * 主进程文件：负责创建窗口、设置毛玻璃效果、监听系统剪贴板，
 * 并通过 IPC 与渲染进程通信。
 * 
 * @module main
 */

const { app, BrowserWindow, ipcMain, nativeImage, clipboard } = require("electron");
const path = require("path");

// ========== 应用配置 ==========

/** 历史记录最大保留条数 */
const MAX_HISTORY = 100;

/** 剪贴板轮询间隔（毫秒），每隔 1000ms 检查一次系统剪贴板是否有变化 */
const POLL_INTERVAL = 1000;

// ========== 全局状态 ==========

/** 主窗口引用，用于向渲染进程发送消息 */
let mainWindow = null;

/** 
 * 剪贴板历史记录数组
 * @type {Array<{id: number, type: string, content: string, timestamp: number}>}
 */
let history = [];

/** 上一次读取到的剪贴板文本内容，用于去重判断 */
let lastClipboardText = "";

/** 上一次读取到的剪贴板图片哈希值，用于去重判断 */
let lastClipboardImageHash = null;

/** 轮询定时器句柄 */
let pollingTimer = null;

/** 调试模式下自动打开开发者工具（命令行传 --dev 参数时启用） */
const isDev = process.argv.includes("--dev");

// ========== 剪贴板图片哈希计算 ==========

/**
 * 计算图片内容的哈希值，用于判断剪贴板图片是否变化
 * 采用简单哈希算法，取 PNG 数据前 1024 字节做累加计算
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
 * 如果检测到新内容，自动添加到历史记录中
 */
function pollClipboard() {
  if (!mainWindow) {
    console.log("[pollClipboard] mainWindow 不存在，跳过");
    return;
  }

  try {
    // 检查文本：读取系统剪贴板的纯文本内容
    const text = clipboard.readText();
    console.log("[pollClipboard] 读取到文本:", text ? text.substring(0, 30) + "..." : "(空)");
    if (text && text !== lastClipboardText && text.trim()) {
      console.log("[pollClipboard] 检测到新文本");
      lastClipboardText = text;
      addHistoryItem({ type: "text", content: text });
    }

    // 检查图片：读取系统剪贴板的图片内容
    const img = clipboard.readImage();
    if (!img.isEmpty()) {
      const hash = getImageHash(img);
      if (hash !== null && hash !== lastClipboardImageHash) {
        console.log("[pollClipboard] 检测到新图片");
        lastClipboardImageHash = hash;
        // 将图片转为 base64 格式的 data URL，方便渲染进程直接显示
        const pngBuffer = img.toPNG();
        const dataUrl = `data:image/png;base64,${pngBuffer.toString("base64")}`;
        addHistoryItem({ type: "image", content: dataUrl });
      }
    }
  } catch (e) {
    // 剪贴板被其他程序占用时抛出异常属于正常情况，忽略即可
    console.log("[pollClipboard] 访问剪贴板出错:", e.message);
  }
}

// ========== 历史记录管理 ==========

/**
 * 向历史记录列表中添加新条目
 * 新条目插入到列表最前面，超过最大条数时移除末尾最旧的条目
 * 添加后主动去重：与最新一条内容相同时不重复添加
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

  // 去重：和最新一条比对，内容相同则跳过
  if (history.length > 0) {
    const last = history[0];
    if (last.type === entry.type && last.content === entry.content) {
      console.log("[addHistoryItem] 内容与最新条目相同，跳过");
      return;
    }
  }

  history.unshift(entry);
  if (history.length > MAX_HISTORY) {
    history.pop();
  }

  // 更新缓存，避免重复触发
  lastClipboardText = clipboard.readText();
  console.log("[addHistoryItem] 已添加，当前历史条数:", history.length);
  sendHistoryToRenderer();
}

/**
 * 清空全部历史记录
 */
function clearHistory() {
  history = [];
  console.log("[clearHistory] 已清空");
  sendHistoryToRenderer();
}

/**
 * 将当前历史记录推送到渲染进程，通知 UI 更新
 * 如果渲染进程尚未就绪（窗口未加载完毕），则等待后重试
 */
function sendHistoryToRenderer() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    try {
      mainWindow.webContents.send("clipboard-history", history);
      console.log("[sendHistoryToRenderer] 已推送", history.length, "条记录");
    } catch (e) {
      console.log("[sendHistoryToRenderer] 发送失败:", e.message);
    }
  }
}

// ========== IPC 通信处理（主进程 ↔ 渲染进程） ==========

/**
 * 处理渲染进程的复制请求：将指定条目写回系统剪贴板
 * 同时更新本地缓存，避免触发重复添加
 */
ipcMain.handle("copy-to-clipboard", (event, item) => {
  console.log("[IPC] copy-to-clipboard:", item.type);
  if (item.type === "text") {
    clipboard.writeText(item.content);
    lastClipboardText = item.content;
  } else if (item.type === "image") {
    const img = nativeImage.createFromDataURL(item.content);
    clipboard.writeImage(img);
    lastClipboardImageHash = getImageHash(img);
  }
});

/** 处理渲染进程的清空请求 */
ipcMain.handle("clear-all", () => {
  console.log("[IPC] clear-all");
  clearHistory();
});

/** 处理渲染进程的历史记录初始化加载请求 */
ipcMain.handle("get-history", () => {
  console.log("[IPC] get-history, 返回", history.length, "条");
  return history;
});

// ========== 窗口创建与毛玻璃效果 ==========

/**
 * 创建 Electron 主窗口
 * 应用 Windows Acrylic 磨砂毛玻璃效果（Windows 10/11）
 * 如果 Win32 的 acrylic 不可用，降级尝试 mica 效果
 * 窗口属性：透明背景、置顶显示、无边框
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

  // 记录渲染进程控制台日志
  mainWindow.webContents.on("console-message", (event, level, message) => {
    console.log("[renderer]", message);
  });

  // 在 Windows 上启用系统级毛玻璃效果
  if (process.platform === "win32") {
    try {
      mainWindow.setBackgroundMaterial("acrylic");
      console.log("[window] 已启用 Acrylic 毛玻璃效果");
    } catch (e) {
      console.log("[window] Acrylic 不可用，尝试 Mica...");
      try {
        mainWindow.setBackgroundMaterial("mica");
        console.log("[window] 已启用 Mica 效果");
      } catch (e2) {
        console.log("[window] Mica 也不可用，使用 CSS 模糊");
      }
    }
  }

  mainWindow.loadFile(path.join(__dirname, "renderer", "index.html"));

  mainWindow.on("closed", () => {
    console.log("[window] 窗口已关闭");
    mainWindow = null;
  });

  // 窗口准备好后主动推送一次历史（即使为空）
  mainWindow.webContents.on("did-finish-load", () => {
    console.log("[window] 页面加载完成");
    sendHistoryToRenderer();
  });
}

// ========== 应用生命周期 ==========

app.whenReady().then(() => {
  console.log("[app] 应用就绪");
  createWindow();

  // 剪贴板监听：轮询方式
  console.log("[app] 启动剪贴板轮询，间隔", POLL_INTERVAL, "ms");
  pollingTimer = setInterval(pollClipboard, POLL_INTERVAL);

  // 延迟执行首次剪贴板读取
  setTimeout(() => {
    console.log("[app] 执行首次剪贴板读取");
    pollClipboard();
  }, 1000);
});

app.on("window-all-closed", () => {
  console.log("[app] 所有窗口已关闭，退出");
  if (pollingTimer) clearInterval(pollingTimer);
  app.quit();
});

app.on("activate", () => {
  if (mainWindow === null) {
    console.log("[app] 重新创建窗口");
    createWindow();
  }
});
