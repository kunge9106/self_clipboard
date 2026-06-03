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

/** 剪贴板轮询间隔（毫秒），每隔 500ms 检查一次系统剪贴板是否有变化 */
const POLL_INTERVAL = 500;

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
  if (!mainWindow) return;

  try {
    // 检查文本：读取系统剪贴板的纯文本内容
    const text = clipboard.readText();
    if (text && text !== lastClipboardText && text.trim()) {
      lastClipboardText = text;
      addHistoryItem({ type: "text", content: text }, false);
    }

    // 检查图片：读取系统剪贴板的图片内容
    const img = clipboard.readImage();
    if (!img.isEmpty()) {
      const hash = getImageHash(img);
      if (hash !== null && hash !== lastClipboardImageHash) {
        lastClipboardImageHash = hash;
        // 将图片转为 base64 格式的 data URL，方便渲染进程直接显示
        const pngBuffer = img.toPNG();
        const dataUrl = `data:image/png;base64,${pngBuffer.toString("base64")}`;
        addHistoryItem({ type: "image", content: dataUrl }, false);
      }
    }
  } catch (e) {
    // 剪贴板被其他程序占用时抛出异常属于正常情况，忽略即可
  }
}

// ========== 历史记录管理 ==========

/**
 * 向历史记录列表中添加新条目
 * 新条目插入到列表最前面，超过最大条数时移除末尾最旧的条目
 * 添加后主动去重：与最新一条内容相同时不重复添加
 * 
 * @param {{type: string, content: string}} item - 要添加的条目
 * @param {boolean} fromRenderer - 是否来自渲染进程的添加请求（预留参数）
 */
function addHistoryItem(item, fromRenderer) {
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
      return;
    }
  }

  history.unshift(entry);
  if (history.length > MAX_HISTORY) {
    history.pop();
  }

  // 更新缓存，避免重复触发
  lastClipboardText = clipboard.readText();
  sendHistoryToRenderer();
}

/**
 * 清空全部历史记录
 */
function clearHistory() {
  history = [];
  sendHistoryToRenderer();
}

/**
 * 将当前历史记录推送到渲染进程，通知 UI 更新
 */
function sendHistoryToRenderer() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("clipboard-history", history);
  }
}

// ========== IPC 通信处理（主进程 ↔ 渲染进程） ==========

/**
 * 处理渲染进程的复制请求：将指定条目写回系统剪贴板
 * 同时更新本地缓存，避免触发重复添加
 */
ipcMain.handle("copy-to-clipboard", (event, item) => {
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
  clearHistory();
});

/** 处理渲染进程的历史记录初始化加载请求 */
ipcMain.handle("get-history", () => {
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
    frame: false,                    // 无边框窗口，用自定义标题栏
    transparent: true,               // 透明背景，配合毛玻璃效果
    alwaysOnTop: true,               // 始终置顶，方便随时查看
    skipTaskbar: false,
    title: "self_clipboard",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,        // 启用上下文隔离，增强安全性
      nodeIntegration: false,        // 禁止渲染进程直接访问 Node.js
    },
  });

  // 在 Windows 上启用系统级毛玻璃效果
  if (process.platform === "win32") {
    try {
      // 优先尝试 Acrylic（Win10 开始支持），效果为半透明彩色模糊
      mainWindow.setBackgroundMaterial("acrylic");
    } catch (e) {
      try {
        // Acrylic 不可用时降级到 Mica（Win11 引入），效果为桌面色调融合
        mainWindow.setBackgroundMaterial("mica");
      } catch (e2) {
        // 什么都不做，退回到 CSS 层面的 backdrop-filter 模糊效果
      }
    }
  }

  mainWindow.loadFile(path.join(__dirname, "renderer", "index.html"));

  mainWindow.on("closed", () => {
    mainWindow = null;
  });
}

// ========== 应用生命周期 ==========

app.whenReady().then(() => {
  createWindow();

  // 启动剪贴板轮询监听
  pollingTimer = setInterval(pollClipboard, POLL_INTERVAL);

  // 延迟 300ms 后执行首次读取，确保窗口已就绪
  setTimeout(pollClipboard, 300);
});

app.on("window-all-closed", () => {
  if (pollingTimer) clearInterval(pollingTimer);
  app.quit();
});

app.on("activate", () => {
  if (mainWindow === null) createWindow();
});
