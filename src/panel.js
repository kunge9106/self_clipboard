// 快捷面板：搜索 + 键盘选择 + 回车粘贴。
//
// 刻意做成独立小页面而不是复用 index.html：面板要在 100ms 内弹出，
// 主界面带着设置页、详情弹窗、拖拽逻辑，启动成本高得多。
// 样式与变量复用主界面那一套（panel.css @import styles.css），观感不会割裂。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const KIND_LABEL = { text: "文本", image: "图片", files: "文件" };

/** 面板一次最多列多少条。再多也不会有人用键盘翻 —— 那时该用搜索或完整窗口。 */
const MAX_ROWS = 60;

const $ = (id) => document.getElementById(id);
const listEl = $("panel-list");
const searchEl = $("panel-search");
const statusEl = $("panel-status");

/** 全量历史（已按时间倒序） */
let all = [];
/** 当前过滤后的结果 */
let rows = [];
/** 当前选中下标 */
let sel = 0;

/** 重载数据。面板是常驻窗口，每次弹出都要重拉 —— 否则看到的是上次的列表。 */
async function reload() {
  try {
    all = await invoke("get_history");
  } catch (e) {
    all = [];
    setStatus("读取失败：" + e);
  }
  searchEl.value = "";
  sel = 0;
  applyFilter();
}

function applyFilter() {
  const q = searchEl.value.trim().toLowerCase();
  const matched = q
    ? all.filter(
        (i) =>
          (i.preview || "").toLowerCase().includes(q) ||
          (KIND_LABEL[i.kind] || "").includes(q) ||
          (i.kind || "").includes(q)
      )
    : all;
  rows = matched.slice(0, MAX_ROWS);
  if (sel >= rows.length) sel = Math.max(0, rows.length - 1);
  render();
  setStatus(
    rows.length === 0
      ? q
        ? "没有匹配的内容"
        : "剪贴板还是空的"
      : `${matched.length} 条${matched.length > MAX_ROWS ? `（显示前 ${MAX_ROWS} 条）` : ""}`
  );
}

function render() {
  listEl.replaceChildren();
  rows.forEach((item, idx) => {
    const row = document.createElement("div");
    row.className = "p-row" + (idx === sel ? " is-sel" : "");
    row.setAttribute("role", "option");

    const kind = document.createElement("span");
    kind.className = "p-kind";
    kind.textContent = KIND_LABEL[item.kind] || item.kind || "";
    row.appendChild(kind);

    const text = document.createElement("span");
    text.className = "p-text";
    // 预览里可能有换行：面板一行装不下，压成一行更好扫
    text.textContent = (item.preview || "").replace(/\s+/g, " ").trim() || "（空）";
    // 用 textContent 而不是 innerHTML —— 预览是用户内容，绝不当 HTML 解析
    row.appendChild(text);

    row.addEventListener("mouseenter", () => {
      if (sel !== idx) {
        sel = idx;
        paintSelection();
      }
    });
    row.addEventListener("click", () => paste(item.id));
    listEl.appendChild(row);
  });
  scrollToSel();
}

/** 只改选中样式，不重建整个列表 —— 键盘连按时重建会有明显卡顿。 */
function paintSelection() {
  [...listEl.children].forEach((el, i) => {
    el.classList.toggle("is-sel", i === sel);
  });
  scrollToSel();
}

function scrollToSel() {
  const el = listEl.children[sel];
  if (el) el.scrollIntoView({ block: "nearest" });
}

function setStatus(text) {
  statusEl.textContent = text || "";
}

async function paste(id) {
  try {
    // 后端：写剪贴板 → 收起面板 → 焦点还给原窗口 → 模拟 Ctrl+V
    await invoke("panel_paste", { id });
  } catch (e) {
    // 只有「复制」这一步失败时面板还没收起，这里提示才有意义
    setStatus("粘贴失败：" + e);
  }
}

// ── 键盘 ────────────────────────────────────────────────────────────────
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    invoke("panel_hide");
    return;
  }
  if (e.key === "ArrowDown") {
    e.preventDefault();
    if (sel < rows.length - 1) {
      sel++;
      paintSelection();
    }
    return;
  }
  if (e.key === "ArrowUp") {
    e.preventDefault();
    if (sel > 0) {
      sel--;
      paintSelection();
    }
    return;
  }
  if (e.key === "Enter") {
    e.preventDefault();
    const item = rows[sel];
    if (item) paste(item.id);
    return;
  }
  // 其他键：把焦点交回搜索框（用户可能已经点过列表）
  if (e.key.length === 1 && document.activeElement !== searchEl) {
    searchEl.focus();
  }
});

searchEl.addEventListener("input", () => {
  sel = 0;
  applyFilter();
});
searchEl.addEventListener("keydown", (e) => {
  // 搜索框里按方向键 / 回车由全局处理，这里只挡掉「输入法组字中」的误触
  if (e.isComposing) e.stopPropagation();
});

// 每次弹出：重拉数据、清空搜索、选中第一条
listen("panel-open", () => {
  reload();
  setTimeout(() => searchEl.focus(), 0);
});

reload().then(() => searchEl.focus());
