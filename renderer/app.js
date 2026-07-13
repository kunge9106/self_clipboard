// ========== 日志 ==========
function log(m) { console.log("[app]", m); }

// ========== 禁用 F12 ==========
document.addEventListener("keydown", (e) => { if (e.key === "F12") e.preventDefault(); });

// ========== 时间格式化 ==========
function formatTime(ts) {
  const now = Date.now(), diff = now - ts;
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000), m = Math.floor(s / 60), h = Math.floor(m / 60);
  const d = new Date(ts), t = new Date(now);
  const today = d.getFullYear() === t.getFullYear() && d.getMonth() === t.getMonth() && d.getDate() === t.getDate();
  if (!today) {
    return String(d.getMonth()+1).padStart(2,"0") + "-" + String(d.getDate()).padStart(2,"0") + " " + String(d.getHours()).padStart(2,"0") + ":" + String(d.getMinutes()).padStart(2,"0") + ":" + String(d.getSeconds()).padStart(2,"0");
  }
  if (s < 60) return Math.max(1,s) + "秒钟前";
  if (m < 60) return m + "分钟前";
  return h + "小时前";
}

// ========== DOM ==========
const listEl = document.getElementById("list");
const emptyState = document.getElementById("emptyState");
const btnClear = document.getElementById("btnClear");
const btnClose = document.getElementById("btnClose");
const itemCount = document.getElementById("itemCount");
const searchInput = document.getElementById("searchInput");

let allItems = [], searchQuery = "";
let refreshCacheStats = null;

function filterItems(q) {
  if (!q || !q.trim()) return allItems;
  const lq = q.trim().toLowerCase();
  return allItems.filter(i => i.type === "image" ? lq.includes("图片") || lq.includes("image") || lq.includes("tupian") : i.content.toLowerCase().includes(lq));
}

function render(items) {
  const di = items || filterItems(searchQuery);
  log("render:", di.length);
  if (!di || di.length === 0) {
    listEl.innerHTML = "";
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3"; btnClear.style.pointerEvents = "none";
    itemCount.textContent = "";
    // 搜索无结果时更新提示文字
    const emptyTitle = emptyState.querySelector(".empty-title");
    const emptyDesc = emptyState.querySelector(".empty-desc");
    if (emptyTitle && emptyDesc) {
      if (searchQuery && searchQuery.trim()) {
        emptyTitle.textContent = "暂无" + (searchQuery.trim().length > 12 ? searchQuery.trim().substring(0, 12) + "..." : searchQuery.trim()) + "相关内容";
        emptyDesc.textContent = "";
      } else {
        emptyTitle.textContent = "暂无相关记录";
        emptyDesc.textContent = "您暂时没有任何剪贴板历史记录";
      }
    }
    return;
  }
  emptyState.classList.remove("visible");
  listEl.style.display = "";
  btnClear.style.opacity = "1"; btnClear.style.pointerEvents = "auto";
  itemCount.textContent = searchQuery ? "找到 " + di.length + " 条结果" : "共 " + di.length + " 条";
  const frag = document.createDocumentFragment();
  for (const item of di) {
    const div = document.createElement("div");
    div.className = "item item-enter"; div.dataset.id = item.id;
    const body = document.createElement("div"); body.className = "item-body";
    if (item.type === "text") {
      const t = document.createElement("div"); t.className = "item-title"; t.textContent = item.content; body.appendChild(t);
    } else if (item.type === "image") {
      const img = document.createElement("img"); img.className = "item-image"; img.src = item.content; img.draggable = false; img.onerror = function() { this.style.display = "none"; var err = document.createElement("div"); err.className = "item-title"; err.textContent = "[???????]"; err.style.color = "#999"; err.style.fontSize = "12px"; body.appendChild(err); }; body.appendChild(img);
    }
    const time = document.createElement("div"); time.className = "item-time"; time.textContent = formatTime(item.timestamp); body.appendChild(time);
    const check = document.createElement("div"); check.className = "item-check"; check.textContent = "✓";
    div.appendChild(body); div.appendChild(check);
    div.addEventListener("click", async () => {
      try {
        log("copy:", item.id);
        await window.clipboardAPI.copyToClipboard(item);
        check.classList.add("show"); div.classList.add("item-copied");
        setTimeout(() => { check.classList.remove("show"); div.classList.remove("item-copied"); }, 800);
      } catch(e) { log("copy err:", e.message); }
    });
    frag.appendChild(div);
  }
  listEl.innerHTML = ""; listEl.appendChild(frag);
}

// ========== 时间自动更新 ==========
let timer = null;
function startTimer() {
  if (timer) clearInterval(timer);
  timer = setInterval(() => {
    const els = listEl.querySelectorAll(".item-time");
    const items = filterItems(searchQuery);
    els.forEach((el, i) => { if (items[i]) el.textContent = formatTime(items[i].timestamp); });
  }, 1000);
}

// ========== 初始化 ==========
async function init() {
  log("init");
  if (!window.clipboardAPI) { log("ERROR: no clipboardAPI"); return; }

  try {
    const h = await window.clipboardAPI.getHistory();
    allItems = h || []; render(filterItems(searchQuery));
  } catch(e) { log("getHistory err:", e.message); }

  window.clipboardAPI.onHistoryUpdate((data) => { allItems = data || []; render(filterItems(searchQuery)); if (refreshCacheStats) refreshCacheStats(); });
  searchInput.addEventListener("input", (e) => { searchQuery = e.target.value; render(filterItems(searchQuery)); const sc = document.getElementById("searchClear"); if (sc) sc.classList.toggle("visible", searchQuery.length > 0); });

  // IME composition events: re-apply acrylic when candidate window appears/disappears
  searchInput.addEventListener("compositionstart", () => {
    window.clipboardAPI.refreshAcrylic();
  });
  searchInput.addEventListener("compositionend", () => {
    setTimeout(() => window.clipboardAPI.refreshAcrylic(), 100);
  });
  const sc2 = document.getElementById("searchClear"); if (sc2) sc2.addEventListener("click", () => { searchInput.value = ""; searchQuery = ""; render(allItems); sc2.classList.remove("visible"); searchInput.focus(); });
  btnClear.addEventListener("click", async () => { await window.clipboardAPI.clearAll(); allItems = []; render([]); });
  const minimizeBtn = document.getElementById('btnMinimize'); if (minimizeBtn) minimizeBtn.addEventListener('click', async () => { await window.clipboardAPI.minimizeWindow(); });
  if (btnClose) btnClose.addEventListener("click", async () => { await window.clipboardAPI.closeWindow(); });
  startTimer();
  

  // 统一处理
  document.addEventListener("mousedown", (e) => {
    if (!e.target.closest(".search-box")) searchInput.blur();
    if (!e.target.closest(".settings-panel") && !e.target.closest("#btnSettings")) {
      document.getElementById("settingsPanel").classList.remove("visible");
    }
    if (!e.target.closest(".context-menu")) {
      document.getElementById("contextMenu").classList.remove("visible");
    }
  });
  // ????????
  window.addEventListener("blur", () => {
    document.getElementById("settingsPanel").classList.remove("visible");
    document.getElementById("contextMenu").classList.remove("visible");
    dialogOverlay.classList.remove("visible");
  });

  // 右键删除菜单
  const ctxM = document.getElementById("contextMenu");
  let ctxId = null;
  document.querySelector(".list").addEventListener("contextmenu", (e) => {
    const item = e.target.closest(".item");
    if (!item) { ctxM.classList.remove("visible"); return; }
    e.preventDefault();
    ctxId = parseFloat(item.dataset.id);
    ctxM.style.left = e.clientX + "px";
    ctxM.style.top = e.clientY + "px";
    ctxM.classList.add("visible");
  });
  ctxM.querySelector("[data-action='delete']").addEventListener("click", async () => {
    if (ctxId != null) { await window.clipboardAPI.deleteItem(ctxId); ctxId = null; }
    ctxM.classList.remove("visible");
  });


  // 设置面板
  const bSet = document.getElementById("btnSettings");
  const sPan = document.getElementById("settingsPanel");
  const tAuto = document.getElementById("toggleAutoStart");
  bSet.addEventListener("click", (e) => {
    e.stopPropagation();
    sPan.classList.toggle("visible");
    dialogOverlay.classList.toggle("visible", sPan.classList.contains("visible"));
  });
  // 设置弹窗和右键菜单关闭时隐藏overlay
  document.addEventListener("mousedown", (e) => {
    if (!e.target.closest(".settings-panel") && !e.target.closest("#btnSettings") && !e.target.closest(".context-menu")) {
      dialogOverlay.classList.remove("visible");
    }
  });
  tAuto.addEventListener("change", async () => { await window.clipboardAPI.setAutoStart(tAuto.checked); });
  (async () => { tAuto.checked = await window.clipboardAPI.getAutoStart(); })();


  // JS??????? CSS -webkit-app-region????????
  let dragActive = false, dragStartX = 0, dragStartY = 0;
  document.querySelector(".window").addEventListener("mousedown", (e) => {
    // ????????????delete????????????
    const target = e.target;
    if (target.closest(".win-btn") || target.closest(".search-box") ||
        target.closest(".btn-settings") || target.closest(".btn-clear") ||
        target.closest(".settings-panel") || target.closest(".context-menu") ||
        target.closest(".close-dialog") || target.closest(".dialog-overlay") ||
        target.closest(".list") || target.closest(".empty-state")) {
      return;
    }
    dragActive = true;
    dragStartX = e.screenX;
    dragStartY = e.screenY;
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragActive) return;
    const dx = e.screenX - dragStartX;
    const dy = e.screenY - dragStartY;
    if (Math.abs(dx) > 0 || Math.abs(dy) > 0) {
      window.clipboardAPI.dragWindow(dx, dy);
      dragStartX = e.screenX;
      dragStartY = e.screenY;
    }
  });
  document.addEventListener("mouseup", () => { dragActive = false; });
  window.clipboardAPI.onClosePanels(() => {
    document.getElementById("settingsPanel").classList.remove("visible");
    document.getElementById("contextMenu").classList.remove("visible");
  });

  // 主题切换时设置 body class 配合 DWM acrylic
  window.clipboardAPI.onThemeChanged((isDark) => {
    document.body.classList.toggle("dark", isDark);
    document.body.classList.toggle("light", !isDark); // ??? DWM ???? acrylic ?
  });

  // 初始化 body class
  document.body.classList.add(
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
  );


  // 关闭弹窗交互
  const closeDialog = document.getElementById("closeDialog");
  const dialogOverlay = document.getElementById("dialogOverlay");
  function showCloseDialog() {
    dialogOverlay.classList.add("visible");
    closeDialog.classList.add("visible");
  }
  function hideCloseDialog() {
    dialogOverlay.classList.remove("visible");
    closeDialog.classList.remove("visible");
  }
  window.clipboardAPI.onShowCloseDialog(() => {
    showCloseDialog();
  });
  closeDialog.querySelectorAll(".close-dialog-opt").forEach((btn) => {
    btn.addEventListener("click", async () => {
      hideCloseDialog();
      await window.clipboardAPI.sendCloseAction(btn.dataset.action);
    });
  });
  closeDialog.addEventListener("click", (e) => {
    if (e.target === closeDialog) hideCloseDialog();
  });
  // 设置面板 - 关闭时到托盘
  const toggleTray = document.getElementById("toggleTrayMode");
  (async () => {
    const cfg = await window.clipboardAPI.getConfig();
    toggleTray.checked = cfg.trayOnClose || false;
  })();
  toggleTray.addEventListener("change", async () => {
    const cfg = await window.clipboardAPI.getConfig();
    cfg.trayOnClose = toggleTray.checked;
    await window.clipboardAPI.setConfig(cfg);
  });

  log("init done");
  // cache settings
  const bpc = document.getElementById("btnPickCache");
  const tcc = document.getElementById("toggleCleanCache");
  const cpd = document.getElementById("cachePathDisplay");
  if (cpd) { cpd.addEventListener("click", () => { window.clipboardAPI.openCacheDir(); }); }
  if (bpc) {
    bpc.addEventListener("click", async () => {
      const dir = await window.clipboardAPI.pickCacheDir();
      if (dir) {
        const cfg = await window.clipboardAPI.getConfig();
        cfg.imageCacheDir = dir;
        await window.clipboardAPI.setConfig(cfg);
        updateCacheDisplay();
      }
    });
  }
  if (tcc) {
    tcc.addEventListener("change", async () => {
      const cfg = await window.clipboardAPI.getConfig();
      cfg.cleanImagesOnClose = tcc.checked;
      await window.clipboardAPI.setConfig(cfg);
    });
  }
  refreshCacheStats = updateCacheDisplay;
  async function updateCacheDisplay() {
    try {
      const info = await window.clipboardAPI.getImageCacheInfo();
      if (info && cpd) {
        cpd.textContent = info.dir;
        const st = document.getElementById("cacheStats");
        if (st) st.textContent = "(" + info.count + " pics, " + (info.size / 1024 / 1024).toFixed(1) + "MB)";
      }
    } catch(e) {}
    try {
      const cfg = await window.clipboardAPI.getConfig();
      if (tcc) tcc.checked = cfg.cleanImagesOnClose || false;
    } catch(e) {}
  }
  updateCacheDisplay();

}
document.addEventListener("DOMContentLoaded", init);
log("loaded");

