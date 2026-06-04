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
    return String(d.getHours()).padStart(2,"0") + ":" + String(d.getMinutes()).padStart(2,"0") + ":" + String(d.getSeconds()).padStart(2,"0");
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

function filterItems(q) {
  if (!q || !q.trim()) return allItems;
  const lq = q.trim().toLowerCase();
  return allItems.filter(i => i.type !== "text" || i.content.toLowerCase().includes(lq));
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
      const img = document.createElement("img"); img.className = "item-image"; img.src = item.content; img.draggable = false; body.appendChild(img);
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

  window.clipboardAPI.onHistoryUpdate((data) => { allItems = data || []; render(filterItems(searchQuery)); });
  searchInput.addEventListener("input", (e) => { searchQuery = e.target.value; render(filterItems(searchQuery)); });
  btnClear.addEventListener("click", async () => { await window.clipboardAPI.clearAll(); allItems = []; render([]); });
  btnClose.addEventListener("click", async () => { await window.clipboardAPI.closeWindow(); });
  startTimer();
  log("init done");
}
document.addEventListener("DOMContentLoaded", init);
log("loaded");
