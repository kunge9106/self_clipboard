/**
 * self_clipboard — 渲染进程逻辑
 * 
 * 负责剪贴板历史列表的渲染、点击复制交互、时间实时更新等前端逻辑。
 * 所有系统级操作（读取/写入剪贴板）均通过 window.clipboardAPI
 * 桥接到主进程执行，确保安全性。
 * 
 * @module renderer/app
 */

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
    // 超过当天 0 点，显示为固定时间格式
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

// ========== 渲染历史列表 ==========

/** 当前展示的历史数据快照，用于时间更新时快速访问 */
let currentItems = [];

/**
 * 将历史数据渲染到列表视图
 * 支持文本条目的多行缩略显示和图片条目的缩略图展示
 * 每条记录都附带时间标签和点击复制功能
 * 
 * @param {Array|null} items - 历史记录数组，为空或空数组时显示空状态
 */
function render(items) {
  currentItems = items || [];

  if (!items || items.length === 0) {
    listEl.innerHTML = "";
    emptyState.classList.add("visible");
    btnClear.style.opacity = "0.3";
    btnClear.style.pointerEvents = "none";
    return;
  }

  // 有数据时隐藏空状态，启用清空按钮
  emptyState.classList.remove("visible");
  btnClear.style.opacity = "1";
  btnClear.style.pointerEvents = "auto";

  // 使用 DocumentFragment 批量构建 DOM，减少重排次数
  const fragment = document.createDocumentFragment();

  items.forEach((item) => {
    const div = document.createElement("div");
    div.className = "item item-enter";
    div.dataset.id = item.id;

    // 条目图标：图片用 🖼️，文本用 📝
    const iconDiv = document.createElement("div");
    iconDiv.className = "item-icon";
    iconDiv.textContent = item.type === "image" ? "🖼️" : "📝";

    // 条目内容区域
    const bodyDiv = document.createElement("div");
    bodyDiv.className = "item-body";

    if (item.type === "text") {
      // 文本：限制最多显示 2 行，超出省略
      const textEl = document.createElement("div");
      textEl.className = "item-text";
      textEl.textContent = item.content;
      bodyDiv.appendChild(textEl);
    } else if (item.type === "image") {
      // 图片：等比缩略图展示，最大高度 120px
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

    // 复制成功对勾标记（默认隐藏，点击后短暂显示）
    const checkEl = document.createElement("div");
    checkEl.className = "item-check";
    checkEl.textContent = "✓";

    div.appendChild(iconDiv);
    div.appendChild(bodyDiv);
    div.appendChild(checkEl);

    // 点击条目时将内容写回系统剪贴板
    div.addEventListener("click", async () => {
      try {
        await window.clipboardAPI.copyToClipboard(item);
        // 短暂显示 ✓ 动画和高亮边框，800ms 后自动消失
        checkEl.classList.add("show");
        div.classList.add("item-copied");
        setTimeout(() => {
          checkEl.classList.remove("show");
          div.classList.remove("item-copied");
        }, 800);
      } catch (e) {
        console.error("复制失败", e);
      }
    });

    fragment.appendChild(div);
  });

  // 替换整个列表内容
  listEl.innerHTML = "";
  listEl.appendChild(fragment);
}

// ========== 时间自动更新 ==========

/** 秒级时间刷新定时器句柄 */
let timeUpdateTimer = null;

/**
 * 启动时间自动更新定时器
 * 每秒遍历所有条目的时间标签，更新相对时间显示
 * 只更新时间文本，不重新渲染整个列表
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
}

// ========== 初始化入口 ==========

/**
 * 应用启动初始化：
 * 1. 加载现有历史记录
 * 2. 注册主进程的实时推送更新
 * 3. 绑定清空按钮事件
 * 4. 启动时间自动更新
 */
async function init() {
  try {
    const history = await window.clipboardAPI.getHistory();
    render(history);
  } catch (e) {
    console.error("获取历史记录失败", e);
  }

  // 主进程检测到剪贴板新内容时，会主动推送最新历史列表
  window.clipboardAPI.onHistoryUpdate((data) => {
    render(data);
  });

  // 绑定清空按钮
  btnClear.addEventListener("click", async () => {
    await window.clipboardAPI.clearAll();
    render([]);
  });

  startTimeUpdater();
}

document.addEventListener("DOMContentLoaded", init);
