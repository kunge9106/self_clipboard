/**
 * self_clipboard — 预加载脚本
 * 
 * 通过 contextBridge 安全地为主进程和渲染进程建立通信桥梁。
 * 渲染进程只能通过 window.clipboardAPI 调用有限的 API，
 * 无法直接访问 Node.js 或 Electron 内部模块（安全隔离）。
 * 
 * @module preload
 */

const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("clipboardAPI", {
  /**
   * 监听剪贴板历史记录更新的回调函数
   * 每当主进程检测到剪贴板有新内容时，会通过此通道推送最新历史列表
   * 
   * @param {Function} callback - 接收历史数据数组的回调函数
   */
  onHistoryUpdate: (callback) => {
    ipcRenderer.on("clipboard-history", (event, data) => callback(data));
  },

  /**
   * 将指定的历史条目写到系统剪贴板
   * 支持文本和图片两种类型
   * 
   * @param {{type: string, content: string}} item - 要复制的内容条目
   * @returns {Promise<void>}
   */
  copyToClipboard: (item) => ipcRenderer.invoke("copy-to-clipboard", item),

  /**
   * 清空全部历史记录
   * 
   * @returns {Promise<void>}
   */
  clearAll: () => ipcRenderer.invoke("clear-all"),

  /**
   * 获取当前全部历史记录（用于页面初始化加载）
   * 
   * @returns {Promise<Array>} 历史记录数组
   */
  getHistory: () => ipcRenderer.invoke("get-history"),
});
