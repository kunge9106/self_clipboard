const { contextBridge, ipcRenderer } = require("electron");
contextBridge.exposeInMainWorld("clipboardAPI", {
  onHistoryUpdate: (cb) => ipcRenderer.on("clipboard-history", (e, d) => cb(d)),
  copyToClipboard: (item) => ipcRenderer.invoke("copy-to-clipboard", item),
  clearAll: () => ipcRenderer.invoke("clear-all"),
  getHistory: () => ipcRenderer.invoke("get-history"),
  closeWindow: () => ipcRenderer.invoke("close-window"),
  minimizeWindow: () => ipcRenderer.invoke("minimize-window"),
  deleteItem: (itemId) => ipcRenderer.invoke("delete-item", itemId),
  getAutoStart: () => ipcRenderer.invoke("get-auto-start"),
  setAutoStart: (enable) => ipcRenderer.invoke("set-auto-start", enable),
  onClosePanels: (cb) => ipcRenderer.on("close-panels", () => cb()),
  dragWindow: (dx, dy) => ipcRenderer.send("window-drag", dx, dy),
});
