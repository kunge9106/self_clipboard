// ========== 鏃ュ織 ==========
function log(m) { console.log("[app]", m); }

// ========== 绂佺敤 F12 ==========
document.addEventListener("keydown", (e) => { if (e.key === "F12") e.preventDefault(); });

// ========== 鏃堕棿鏍煎紡鍖?==========
function formatTime(ts) {
  const now = Date.now(), diff = now - ts;
  if (diff < 0) return "鍒氬垰";
  const s = Math.floor(diff / 1000), m = Math.floor(s / 60), h = Math.floor(m / 60);
  const d = new Date(ts), t = new Date(now);
  const today = d.getFullYear() === t.getFullYear() && d.getMonth() === t.getMonth() && d.getDate() === t.getDate();
  if (!today) {
    return String(d.getHours()).padStart(2,"0") + ":" + String(d.getMinutes()).padStart(2,"0") + ":" + String(d.getSeconds()).padStart(2,"0");
  }
  if (s < 60) return Math.max(1,s) + "绉掗挓鍓?;
  if (m < 60) return m + "鍒嗛挓鍓?;
  return h + "灏忔椂鍓?;
}

// ========== DOM ==========
const listEl = document.getElementById("list");
const emptyState = document.getElementById("emptyState");
const btnClear = document.getElementById("btnClear");
const btnClose = document.getElementById("btnClose");
const itemCount = document.getElementById("itemCount");
const searchInput = document.getElementById("searchInput");

let allItems = [], searchQuery = "";

function filterItems(q) {
  if (!q || !q.trim()) return allItems;
  const lq = q.trim().toLowerCase();
  return allItems.filter(i => i.type === "image" ? lq.includes("鍥剧墖") || lq.includes("image") || lq.includes("tupian") : i.content.toLowerCase().includes(lq));
}

function render(items) {
  const di = items || filterItems(searchQuery);
  log("render:", di.length);
  if (!di || di.length === 0) {
    listEl.innerHTML = "";
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3"; btnClear.style.pointerEvents = "none";
    itemCount.textContent = "";
    return;
  }
  emptyState.classList.remove("visible");
  listEl.style.display = "";
  btnClear.style.opacity = "1"; btnClear.style.pointerEvents = "auto";
  itemCount.textContent = searchQuery ? "鎵惧埌 " + di.length + " 鏉＄粨鏋? : "鍏?" + di.length + " 鏉?;
  const frag = document.createDocumentFragment();
  for (const item of di) {
    const div = document.createElement("div");
    div.className = "item item-enter"; div.dataset.id = item.id;
    const body = document.createElement("div"); body.className = "item-body";
    if (item.type === "text") {
      const t = document.createElement("div"); t.className = "item-title"; t.textContent = item.content; body.appendChild(t);
    } else if (item.type === "image") {
      const img = document.createElement("img"); img.className = "item-image"; img.src = item.content; img.draggable = false; body.appendChild(img);
    }
    const time = document.createElement("div"); time.className = "item-time"; time.textContent = formatTime(item.timestamp); body.appendChild(time);
    const check = document.createElement("div"); check.className = "item-check"; check.textContent = "鉁?;
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

// ========== 鏃堕棿鑷姩鏇存柊 ==========
let timer = null;
function startTimer() {
  if (timer) clearInterval(timer);
  timer = setInterval(() => {
    const els = listEl.querySelectorAll(".item-time");
    const items = filterItems(searchQuery);
    els.forEach((el, i) => { if (items[i]) el.textContent = formatTime(items[i].timestamp); });
  }, 1000);
}

// ========== 鍒濆鍖?==========
async function init() {
  log("init");
  if (!window.clipboardAPI) { log("ERROR: no clipboardAPI"); return; }

  try {
    const h = await window.clipboardAPI.getHistory();
    allItems = h || []; render(filterItems(searchQuery));
  } catch(e) { log("getHistory err:", e.message); }

  window.clipboardAPI.onHistoryUpdate((data) => { allItems = data || []; render(filterItems(searchQuery)); });
  searchInput.addEventListener("input", (e) => { searchQuery = e.target.value; render(filterItems(searchQuery)); });

  // IME composition events: re-apply acrylic when candidate window appears/disappears
  searchInput.addEventListener("compositionstart", () => {
    window.clipboardAPI.refreshAcrylic();
  });
  searchInput.addEventListener("compositionend", () => {
    setTimeout(() => window.clipboardAPI.refreshAcrylic(), 100);
  });
  btnClear.addEventListener("click", async () => { await window.clipboardAPI.clearAll(); allItems = []; render([]); });
  document.getElementById('btnMinimize').addEventListener('click', async () => { await window.clipboardAPI.minimizeWindow(); });
  btnClose.addEventListener("click", async () => { await window.clipboardAPI.closeWindow(); });
  startTimer();
  

  // 缁熶竴澶勭悊
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

  // 鍙抽敭鍒犻櫎鑿滃崟
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
    dialogOverlay.classList.add("visible");
  });
  ctxM.querySelector("[data-action='delete']").addEventListener("click", async () => {
    if (ctxId != null) { await window.clipboardAPI.deleteItem(ctxId); ctxId = null; }
    ctxM.classList.remove("visible");
    dialogOverlay.classList.remove("visible");
  });


  // 璁剧疆闈㈡澘
  const bSet = document.getElementById("btnSettings");
  const sPan = document.getElementById("settingsPanel");
  const tAuto = document.getElementById("toggleAutoStart");
  bSet.addEventListener("click", (e) => {
    e.stopPropagation();
    sPan.classList.toggle("visible");
    dialogOverlay.classList.toggle("visible", sPan.classList.contains("visible"));
  });
  // 璁剧疆寮圭獥鍜屽彸閿彍鍗曞叧闂椂闅愯棌overlay
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

  // 涓婚鍒囨崲鏃惰缃?body class 閰嶅悎 DWM acrylic
  window.clipboardAPI.onThemeChanged((isDark) => {
    document.body.classList.toggle("dark", isDark);
    document.body.classList.toggle("light", !isDark); // ??? DWM ???? acrylic ?
  });

  // 鍒濆鍖?body class
  document.body.classList.add(
    window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"
  );


  // 鍏抽棴寮圭獥浜や簰
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
  // 璁剧疆闈㈡澘 - 鍏抽棴鏃跺埌鎵樼洏
  const toggleTray = document.getElementById("toggleTrayMode");
  (async () => {
    const cfg = await window.clipboardAPI.getConfig();
    toggleTray.checked = cfg.trayOnClose || false;
  })();
  toggleTray.addEventListener("change", async () => {
    await window.clipboardAPI.setConfig({ trayOnClose: toggleTray.checked });
  });

  log("init done");
  // cache settings
  const bpc = document.getElementById("btnPickCache");
  const tcc = document.getElementById("toggleCleanCache");
  const cpd = document.getElementById("cachePathDisplay");
  bpc.addEventListener("click", async () => {
    const dir = await window.clipboardAPI.pickCacheDir();
    if (dir) {
      const cfg = await window.clipboardAPI.getConfig();
      cfg.imageCacheDir = dir;
      await window.clipboardAPI.setConfig(cfg);
      updateCacheDisplay();
    }
  });
  tcc.addEventListener("change", async () => {
    const cfg = await window.clipboardAPI.getConfig();
    cfg.cleanImagesOnClose = tcc.checked;
    await window.clipboardAPI.setConfig(cfg);
  });
  async function updateCacheDisplay() {
    const info = await window.clipboardAPI.getImageCacheInfo();
    if (info) {
      cpd.textContent = info.dir + " (" + info.count + " pics, " + (info.size / 1024 / 1024).toFixed(1) + "MB)";
    }
    const cfg = await window.clipboardAPI.getConfig();
    tcc.checked = cfg.cleanImagesOnClose || false;
  }
  updateCacheDisplay();
}
document.addEventListener("DOMContentLoaded", init);
log("loaded");

