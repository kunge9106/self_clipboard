/**
 * self_clipboard — 渲染进程逻辑（VisionOS 风格）
 * 
 * 注意：console.log 使用英文，避免 Windows 控制台编码导致乱码
 * 
 * @module renderer/app
 */

// ========== 调试日志 ==========
function log(msg) {
  console.log("[app]", msg);
}

// ========== 时间格式化 ==========

/**
 * 将时间戳格式化为可读的中文时间描述
 * 
 * 规则：
 *   不到 60 秒 → "xx秒钟前"
 *   不到 60 分 → "xx分钟前"
 *   超过 60 分但仍在当天 → "xx小时前"
 *   过了当天 0 点 → "xx:xx:xx"
 * 
 * @param {number} timestamp - 毫秒时间戳
 * @returns {string}
 */
function formatTime(timestamp) {
  const now = Date.now();
  const diff = now - timestamp;
  if (diff < 0) return "刚刚";

  const seconds = Math.floor(diff / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);

  const date = new Date(timestamp);
  const today = new Date(now);
  const isToday =
    date.getFullYear() === today.getFullYear() &&
    date.getMonth() === today.getMonth() &&
    date.getDate() === today.getDate();

  if (!isToday) {
    const hh = String(date.getHours()).padStart(2, "0");
    const mm = String(date.getMinutes()).padStart(2, "0");
    const ss = String(date.getSeconds()).padStart(2, "0");
    return `${hh}:${mm}:${ss}`;
  }

  if (seconds < 60) return `${Math.max(1, seconds)}秒钟前`;
  if (minutes < 60) return `${minutes}分钟前`;
  return `${hours}小时前`;
}

// ========== DOM 引用 ==========

const listEl = document.getElementById("list");
const emptyState = document.getElementById("emptyState");
const btnClear = document.getElementById("btnClear");
const itemCount = document.getElementById("itemCount");
const searchInput = document.getElementById("searchInput");

let allItems = [];       // 全部历史数据
let searchQuery = "";    // 当前搜索关键词

// ========== 搜索过滤 ==========

/**
 * 根据搜索关键词过滤历史记录
 * 匹配文本内容的完整关键词（不区分大小写）
 * 
 * @param {string} query - 搜索关键词
 * @returns {Array} 过滤后的条目列表
 */
function filterItems(query) {
  if (!query || !query.trim()) return allItems;
  const q = query.trim().toLowerCase();
  return allItems.filter((item) => {
    if (item.type === "text") {
      return item.content.toLowerCase().includes(q);
    }
    // 图片类型通过文本描述不可搜索，但保留（在搜索模式下仍然显示所有图片）
    return true;
  });
}

// ========== 渲染列表 ==========

function render(items) {
  const displayItems = items || filterItems(searchQuery);
  log("render, count:", displayItems.length);

  if (!displayItems || displayItems.length === 0) {
    listEl.innerHTML = "";
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3";
    btnClear.style.pointerEvents = "none";
    itemCount.textContent = "";
    return;
  }

  emptyState.classList.remove("visible");
  btnClear.style.opacity = "1";
  btnClear.style.pointerEvents = "auto";
  const totalText = searchQuery
    ? `找到 ${displayItems.length} 条结果`
    : `共 ${displayItems.length} 条`;
  itemCount.textContent = totalText;

  const fragment = document.createDocumentFragment();

  displayItems.forEach((item) => {
    const div = document.createElement("div");
    div.className = "item item-enter";
    div.dataset.id = item.id;

    // 图标
    const iconDiv = document.createElement("div");
    iconDiv.className = "item-icon";
    iconDiv.textContent = item.type === "image" ? "🖼️" : "📝";

    // 内容
    const bodyDiv = document.createElement("div");
    bodyDiv.className = "item-body";

    if (item.type === "text") {
      const titleEl = document.createElement("div");
      titleEl.className = "item-title";
      titleEl.textContent = item.content;
      bodyDiv.appendChild(titleEl);
    } else if (item.type === "image") {
      const imgEl = document.createElement("img");
      imgEl.className = "item-image";
      imgEl.src = item.content;
      imgEl.draggable = false;
      bodyDiv.appendChild(imgEl);
    }

    // 时间
    const timeEl = document.createElement("div");
    timeEl.className = "item-time";
    timeEl.textContent = formatTime(item.timestamp);
    bodyDiv.appendChild(timeEl);

    // 对勾
    const checkEl = document.createElement("div");
    checkEl.className = "item-check";
    checkEl.textContent = "✓";

    div.appendChild(iconDiv);
    div.appendChild(bodyDiv);
    div.appendChild(checkEl);

    // 点击复制
    div.addEventListener("click", async () => {
      try {
        log("copy item:", item.id);
        await window.clipboardAPI.copyToClipboard(item);
        checkEl.classList.add("show");
        div.classList.add("item-copied");
        setTimeout(() => {
          checkEl.classList.remove("show");
          div.classList.remove("item-copied");
        }, 800);
      } catch (e) {
        log("copy error:", e.message);
      }
    });

    fragment.appendChild(div);
  });

  listEl.innerHTML = "";
  listEl.appendChild(fragment);
  log("render done");
}

// ========== 时间自动更新 ==========

let timeUpdateTimer = null;

function startTimeUpdater() {
  if (timeUpdateTimer) clearInterval(timeUpdateTimer);
  timeUpdateTimer = setInterval(() => {
    const timeEls = listEl.querySelectorAll(".item-time");
    const items = filterItems(searchQuery);
    timeEls.forEach((el, index) => {
      if (items[index]) {
        el.textContent = formatTime(items[index].timestamp);
      }
    });
  }, 1000);
  log("time updater started");
}

// ========== 初始化 ==========

async function init() {
  log("init start");

  if (!window.clipboardAPI) {
    log("ERROR: clipboardAPI not available");
    document.getElementById("window").innerHTML = "<div style='padding:40px;color:red;text-align:center;'>App Error: clipboardAPI not loaded</div>";
    return;
  }
  log("clipboardAPI ok");

  // 加载历史
  try {
    const history = await window.clipboardAPI.getHistory();
    log("getHistory:", history ? history.length : 0, "items");
    allItems = history || [];
    render(filterItems(searchQuery));
  } catch (e) {
    log("getHistory error:", e.message);
  }

  // 监听主进程推送
  window.clipboardAPI.onHistoryUpdate((data) => {
    log("history update:", data ? data.length : 0, "items");
    allItems = data || [];
    render(filterItems(searchQuery));
  });

  // 搜索输入
  searchInput.addEventListener("input", (e) => {
    searchQuery = e.target.value;
    log("search:", searchQuery);
    render(filterItems(searchQuery));
  });

  // 清空按钮
  btnClear.addEventListener("click", async () => {
    log("clear button clicked");
    await window.clipboardAPI.clearAll();
    allItems = [];
    render([]);
  });

  startTimeUpdater();
  log("init done");
}

document.addEventListener("DOMContentLoaded", init);
log("app.js loaded");
