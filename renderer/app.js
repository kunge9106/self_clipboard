// ========== 日志 ==========
function log(m) { console.log("[app]", m); }

// ========== 屏蔽调试快捷键（F12 / Ctrl+Shift+I / Ctrl+Shift+J） ==========
document.addEventListener("keydown", (e) => {
  if (e.key === "F12" ||
      (e.ctrlKey && e.shiftKey && (e.key === "I" || e.key === "J")) ||
      (e.ctrlKey && e.shiftKey && e.key === "C")) {
    e.preventDefault();
    e.stopPropagation();
  }
}, true);
document.addEventListener("contextmenu", (e) => {
  // 只在列表项上保留自定义右键菜单，其余位置屏蔽浏览器默认菜单
  if (!e.target.closest(".item")) e.preventDefault();
});

// ========== 时间格式化 ==========
function formatTime(ts) {
  const now = Date.now(), diff = now - ts;
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000), m = Math.floor(s / 60), h = Math.floor(m / 60);
  const d = new Date(ts), t = new Date(now);
  const today = d.getFullYear() === t.getFullYear() && d.getMonth() === t.getMonth() && d.getDate() === t.getDate();
  if (!today) {
    const p = (n) => String(n).padStart(2, "0");
    return p(d.getMonth() + 1) + "-" + p(d.getDate()) + " " + p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
  }
  if (s < 60) return Math.max(1, s) + "秒钟前";
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
const dialogOverlay = document.getElementById("dialogOverlay");
const closeDialog = document.getElementById("closeDialog");

/** 单次渲染上限，滚动到底自动增量加载（历史无上限时的兜底） */
const RENDER_LIMIT_STEP = 150;

let allItems = [], searchQuery = "", currentItems = [], renderLimit = RENDER_LIMIT_STEP;
/** 当前已渲染条目的 id -> item 索引，供定时器更新时间用（避免 O(n) 查找） */
let idIndex = new Map();
let refreshCacheStats = null;

function filterItems(q) {
  if (!q || !q.trim()) return allItems;
  const lq = q.trim().toLowerCase();
  return allItems.filter((i) => {
    if (i.type === "image") return lq === "图片" || lq === "image" || lq === "tupian";
    return i.content.toLowerCase().includes(lq);
  });
}

function buildItemNode(item) {
  const div = document.createElement("div");
  div.className = "item item-enter";
  div.dataset.id = item.id;
  const body = document.createElement("div");
  body.className = "item-body";
  if (item.type === "text") {
    const t = document.createElement("div");
    t.className = "item-title";
    t.textContent = item.content;
    body.appendChild(t);
  } else if (item.type === "image") {
    const img = document.createElement("img");
    img.className = "item-image";
    // 优先加载缩略图，避免解码全尺寸截图
    img.src = item.thumb || item.content;
    img.loading = "lazy";
    img.decoding = "async";
    img.draggable = false;
    img.onerror = function () {
      this.style.display = "none";
      const err = document.createElement("div");
      err.className = "item-title";
      err.textContent = "[图片已失效]";
      err.style.color = "#999";
      err.style.fontSize = "12px";
      body.appendChild(err);
    };
    body.appendChild(img);
  }
  const time = document.createElement("div");
  time.className = "item-time";
  time.textContent = formatTime(item.timestamp);
  body.appendChild(time);
  const check = document.createElement("div");
  check.className = "item-check";
  check.textContent = "✓";
  div.appendChild(body);
  div.appendChild(check);
  return div;
}

function render() {
  currentItems = filterItems(searchQuery);
  const visible = currentItems.slice(0, renderLimit);
  log("render " + visible.length + "/" + currentItems.length);

  if (currentItems.length === 0) {
    listEl.innerHTML = "";
    idIndex = new Map();
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3"; btnClear.style.pointerEvents = "none";
    itemCount.textContent = "";
    const emptyTitle = emptyState.querySelector(".empty-title");
    const emptyDesc = emptyState.querySelector(".empty-desc");
    if (emptyTitle && emptyDesc) {
      if (searchQuery && searchQuery.trim()) {
        const q = searchQuery.trim();
        emptyTitle.textContent = "暂无" + (q.length > 12 ? q.substring(0, 12) + "..." : q) + "相关内容";
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
  itemCount.textContent = searchQuery
    ? "找到 " + currentItems.length + " 条结果"
    : "共 " + currentItems.length + " 条" + (currentItems.length > visible.length ? "（已显示 " + visible.length + "）" : "");

  const frag = document.createDocumentFragment();
  idIndex = new Map();
  for (const item of visible) {
    idIndex.set(String(item.id), item);
    frag.appendChild(buildItemNode(item));
  }
  listEl.innerHTML = "";
  listEl.appendChild(frag);
}

/** 事件委托：整个列表只挂一个监听器，不再每条一个闭包 */
listEl.addEventListener("click", async (e) => {
  const el = e.target.closest(".item");
  if (!el) return;
  const item = idIndex.get(el.dataset.id);
  if (!item) return;
  try {
    await window.clipboardAPI.copyToClipboard(item);
    const check = el.querySelector(".item-check");
    if (check) check.classList.add("show");
    el.classList.add("item-copied");
    setTimeout(() => {
      if (check) check.classList.remove("show");
      el.classList.remove("item-copied");
    }, 800);
  } catch (err) {
    log("copy err:", err.message);
  }
});

/** 滚动到底自动加载下一批 */
listEl.addEventListener("scroll", () => {
  if (listEl.scrollTop + listEl.clientHeight < listEl.scrollHeight - 240) return;
  if (renderLimit < currentItems.length) {
    renderLimit += RENDER_LIMIT_STEP;
    render();
  }
}, { passive: true });

// ========== 时间自动更新 ==========
let timer = null;
function startTimer() {
  if (timer) clearInterval(timer);
  // formatTime 的最小粒度是分钟级，30s 刷新一次足够，避免每秒全量刷 DOM
  timer = setInterval(() => {
    const els = listEl.querySelectorAll(".item");
    els.forEach((el) => {
      const item = idIndex.get(el.dataset.id);
      const t = el.querySelector(".item-time");
      if (!item || !t) return;
      const next = formatTime(item.timestamp);
      if (t.textContent !== next) t.textContent = next;
    });
  }, 30000);
}

// ========== 历史增量同步 ==========
function applyHistoryOp(payload) {
  if (!payload || !payload.op) return;
  if (payload.op === "init") {
    allItems = payload.items || [];
    renderLimit = RENDER_LIMIT_STEP;
    render();
  } else if (payload.op === "add") {
    allItems.unshift(payload.item);
    renderLimit = RENDER_LIMIT_STEP;
    render();
  } else if (payload.op === "remove") {
    allItems = allItems.filter((i) => String(i.id) !== String(payload.id));
    render();
  } else if (payload.op === "clear") {
    allItems = [];
    render();
  }
  if (refreshCacheStats) refreshCacheStats();
}

// ========== 初始化 ==========
async function init() {
  log("init");
  if (!window.clipboardAPI) { log("ERROR: no clipboardAPI"); return; }

  try {
    const h = await window.clipboardAPI.getHistory();
    allItems = h || [];
    render();
  } catch (e) { log("getHistory err:", e.message); }

  window.clipboardAPI.onHistoryUpdate(applyHistoryOp);

  searchInput.addEventListener("input", (e) => {
    searchQuery = e.target.value;
    renderLimit = RENDER_LIMIT_STEP;
    render();
    const sc = document.getElementById("searchClear");
    if (sc) sc.classList.toggle("visible", searchQuery.length > 0);
  });

  // IME composition events: re-apply acrylic when candidate window appears/disappears
  searchInput.addEventListener("compositionstart", () => { window.clipboardAPI.refreshAcrylic(); });
  searchInput.addEventListener("compositionend", () => {
    setTimeout(() => window.clipboardAPI.refreshAcrylic(), 100);
  });

  const sc2 = document.getElementById("searchClear");
  if (sc2) sc2.addEventListener("click", () => {
    searchInput.value = "";
    searchQuery = "";
    renderLimit = RENDER_LIMIT_STEP;
    render();
    sc2.classList.remove("visible");
    searchInput.focus();
  });

  btnClear.addEventListener("click", async () => {
    await window.clipboardAPI.clearAll();
    allItems = [];
    render();
  });

  const minimizeBtn = document.getElementById("btnMinimize");
  if (minimizeBtn) minimizeBtn.addEventListener("click", async () => { await window.clipboardAPI.minimizeWindow(); });
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
    if (!e.target.closest(".settings-panel") && !e.target.closest("#btnSettings") && !e.target.closest(".context-menu")) {
      dialogOverlay.classList.remove("visible");
    }
  });
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
    ctxId = item.dataset.id;
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
  tAuto.addEventListener("change", async () => { await window.clipboardAPI.setAutoStart(tAuto.checked); });
  (async () => { tAuto.checked = await window.clipboardAPI.getAutoStart(); })();

  // 窗口拖拽：rAF 合并位移，每帧最多一次 IPC
  let dragActive = false, dragStartX = 0, dragStartY = 0;
  let pendingDX = 0, pendingDY = 0, dragRaf = 0;
  document.querySelector(".window").addEventListener("mousedown", (e) => {
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
    pendingDX += e.screenX - dragStartX;
    pendingDY += e.screenY - dragStartY;
    dragStartX = e.screenX;
    dragStartY = e.screenY;
    if (!dragRaf) {
      dragRaf = requestAnimationFrame(() => {
        dragRaf = 0;
        const dx = pendingDX, dy = pendingDY;
        pendingDX = 0; pendingDY = 0;
        if (dx || dy) window.clipboardAPI.dragWindow(dx, dy);
      });
    }
  });
  document.addEventListener("mouseup", () => { dragActive = false; });

  window.clipboardAPI.onClosePanels(() => {
    document.getElementById("settingsPanel").classList.remove("visible");
    document.getElementById("contextMenu").classList.remove("visible");
  });

  window.clipboardAPI.onThemeChanged((payload) => {
    // 兼容旧版布尔载荷与新版 {isDark, tier}
    const p = payload && typeof payload === "object" ? payload : { isDark: payload, tier: "acrylic" };
    document.body.classList.toggle("dark", !!p.isDark);
    document.body.classList.toggle("light", !p.isDark);
    document.body.classList.remove("effect-acrylic", "effect-mica", "effect-solid");
    document.body.classList.add("effect-" + (p.tier || "solid"));
  });
  document.body.classList.add(
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
  );
  // 初始默认走 acrylic 档，主进程下发后自动修正
  document.body.classList.add("effect-acrylic");

  // 关闭弹窗交互
  function showCloseDialog() {
    dialogOverlay.classList.add("visible");
    closeDialog.classList.add("visible");
  }
  function hideCloseDialog() {
    dialogOverlay.classList.remove("visible");
    closeDialog.classList.remove("visible");
  }
  window.clipboardAPI.onShowCloseDialog(showCloseDialog);
  closeDialog.querySelectorAll(".close-dialog-opt").forEach((btn) => {
    btn.addEventListener("click", async () => {
      hideCloseDialog();
      await window.clipboardAPI.sendCloseAction(btn.dataset.action);
    });
  });
  closeDialog.addEventListener("click", (e) => { if (e.target === closeDialog) hideCloseDialog(); });

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

  // 缓存设置
  const bpc = document.getElementById("btnPickCache");
  const tcc = document.getElementById("toggleCleanCache");
  const cpd = document.getElementById("cachePathDisplay");
  if (cpd) cpd.addEventListener("click", () => { window.clipboardAPI.openCacheDir(); });
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
    } catch (e) {}
    try {
      const cfg = await window.clipboardAPI.getConfig();
      if (tcc) tcc.checked = cfg.cleanImagesOnClose || false;
    } catch (e) {}
  }
  updateCacheDisplay();

  log("init done");
}
document.addEventListener("DOMContentLoaded", init);
log("loaded");
