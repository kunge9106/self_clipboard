const { contextBridge, ipcRenderer } = require("electron");
contextBridge.exposeInMainWorld("clipboardAPI", {
  onHistoryUpdate: (cb) => ipcRenderer.on("clipboard-history", (e, d) => cb(d)),
  copyToClipboard: (item) => ipcRenderer.invoke("copy-to-clipboard", item),
  clearAll: () => ipcRenderer.invoke("clear-all"),
  getHistory: () => ipcRenderer.invoke("get-history"),
  closeWindow: () => ipcRenderer.invoke("close-window"),
  minimizeWindow: () => ipcRenderer.invoke("minimize-window"),
});
