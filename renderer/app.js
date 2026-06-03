/**
 * self_clipboard — 渲染进程逻辑
 * 
 * 负责剪贴板历史列表的渲染、点击复制交互、时间实时更新等前端逻辑。
 * 所有系统级操作（读取/写入剪贴板）均通过 window.clipboardAPI
 * 桥接到主进程执行，确保安全性。
 * 
 * @module renderer/app
 */

// ========== 调试工具 ==========

/**
 * 输出调试日志到主进程控制台（通过 Electron 的 console-message 事件）
 * 也会将日志显示到页面上的调试区域（如果存在）
 * 
 * @param {string} msg - 调试消息
 */
function log(msg) {
  console.log("[app]", msg);
}

// ========== 时间格式化工具 ==========

/**
 * 根据时间戳和当前时间的差值，格式化为可读的中文时间描述
 * 
 * 时间显示规则：
 *   - 不到 60 秒 → "xx秒前"
 *   - 不到 60 分钟 → "xx分前"
 *   - 超过 60 分钟但仍在当天 → "xx时前"
 *   - 过了当天 0 点 → "xx:xx:xx"（时:分:秒）
 * 
 * @param {number} timestamp - 条目保存时的时间戳（毫秒）
 * @returns {string} 格式化后的时间文本
 */
function formatTime(timestamp) {
  const now = Date.now();
  const diff = now - timestamp;
  if (diff < 0) return "刚刚";

  const seconds = Math.floor(diff / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);

  // 判断条目时间是否还在当天（与当天 0 点比较）
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

  if (seconds < 60) {
    return `${Math.max(1, seconds)}秒前`;
  }
  if (minutes < 60) {
    return `${minutes}分前`;
  }
  return `${hours}时前`;
}

// ========== DOM 元素引用 ==========

/** 历史列表容器 */
const listEl = document.getElementById("list");

/** 空状态提示区域 */
const emptyState = document.getElementById("emptyState");

/** 清空全部按钮 */
const btnClear = document.getElementById("btnClear");

/** 当前展示的历史数据快照，用于时间更新时快速访问 */
let currentItems = [];

// ========== 渲染历史列表 ==========

/**
 * 将历史数据渲染到列表视图
 * 支持文本条目的多行缩略显示和图片条目的缩略图展示
 * 每条记录都附带时间标签和点击复制功能
 * 
 * @param {Array|null} items - 历史记录数组
 */
function render(items) {
  log("render() 被调用，数据条数:" + (items ? items.length : 0));

  currentItems = items || [];

  if (!items || items.length === 0) {
    listEl.innerHTML = "";
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3";
    btnClear.style.pointerEvents = "none";
    log("显示空状态");
    return;
  }

  emptyState.classList.remove("visible");
  btnClear.style.opacity = "1";
  btnClear.style.pointerEvents = "auto";

  const fragment = document.createDocumentFragment();

  items.forEach((item, index) => {
    const div = document.createElement("div");
    div.className = "item item-enter";
    div.dataset.id = item.id;
    div.dataset.index = index;

    // 条目图标
    const iconDiv = document.createElement("div");
    iconDiv.className = "item-icon";
    iconDiv.textContent = item.type === "image" ? "🖼️" : "📝";

    // 条目内容区域
    const bodyDiv = document.createElement("div");
    bodyDiv.className = "item-body";

    if (item.type === "text") {
      const textEl = document.createElement("div");
      textEl.className = "item-text";
      textEl.textContent = item.content;
      bodyDiv.appendChild(textEl);
    } else if (item.type === "image") {
      const imgEl = document.createElement("img");
      imgEl.className = "item-image";
      imgEl.src = item.content;
      imgEl.draggable = false;
      bodyDiv.appendChild(imgEl);
    }

    // 时间标签
    const timeEl = document.createElement("div");
    timeEl.className = "item-time";
    timeEl.textContent = formatTime(item.timestamp);
    bodyDiv.appendChild(timeEl);

    // 复制成功对勾标记
    const checkEl = document.createElement("div");
    checkEl.className = "item-check";
    checkEl.textContent = "✓";

    div.appendChild(iconDiv);
    div.appendChild(bodyDiv);
    div.appendChild(checkEl);

    // 点击复制
    div.addEventListener("click", async () => {
      try {
        log("点击复制条目:" + item.id);
        await window.clipboardAPI.copyToClipboard(item);
        checkEl.classList.add("show");
        div.classList.add("item-copied");
        setTimeout(() => {
          checkEl.classList.remove("show");
          div.classList.remove("item-copied");
        }, 800);
      } catch (e) {
        log("复制失败:" + e.message);
      }
    });

    fragment.appendChild(div);
  });

  listEl.innerHTML = "";
  listEl.appendChild(fragment);
  log("列表渲染完成");
}

// ========== 时间自动更新 ==========

/** 秒级时间刷新定时器句柄 */
let timeUpdateTimer = null;

/**
 * 启动时间自动更新定时器，每秒更新时间标签
 */
function startTimeUpdater() {
  if (timeUpdateTimer) clearInterval(timeUpdateTimer);
  timeUpdateTimer = setInterval(() => {
    const timeEls = listEl.querySelectorAll(".item-time");
    const items = currentItems;
    timeEls.forEach((el, index) => {
      if (items[index]) {
        el.textContent = formatTime(items[index].timestamp);
      }
    });
  }, 1000);
  log("时间更新器已启动");
}

// ========== 初始化入口 ==========

/**
 * 应用启动初始化
 */
async function init() {
  log("页面初始化开始");

  // 检查 clipboardAPI 是否可用
  if (!window.clipboardAPI) {
    log("错误: clipboardAPI 未定义，preload 可能未正确加载");
    return;
  }
  log("clipboardAPI 可用");

  // 加载现有历史记录
  try {
    const history = await window.clipboardAPI.getHistory();
    log("获取到历史记录:" + (history ? history.length : 0) + "条");
    render(history || []);
  } catch (e) {
    log("获取历史记录失败:" + e.message);
  }

  // 注册主进程推送的更新
  window.clipboardAPI.onHistoryUpdate((data) => {
    log("收到主进程推送的历史更新:" + (data ? data.length : 0) + "条");
    render(data || []);
  });

  // 清空按钮
  btnClear.addEventListener("click", async () => {
    log("点击清空按钮");
    await window.clipboardAPI.clearAll();
    render([]);
  });

  // 启动时间更新
  startTimeUpdater();

  log("页面初始化完成");
}

// 页面加载完成后执行
document.addEventListener("DOMContentLoaded", init);
log("app.js 已加载");
