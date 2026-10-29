// 快捷面板：搜索 + 键盘选择 + 回车粘贴。
//
// 刻意做成独立小页面而不是复用 index.html：面板要在 100ms 内弹出，
// 主界面带着设置页、详情弹窗、拖拽逻辑，启动成本高得多。
// 样式与变量复用主界面那一套（panel.css @import styles.css），观感不会割裂。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
// 关闭键的匹配规则与设置页录制共用一份，两边各写一套必然对不上
import { matchesCombo } from "./hotkeys.js";
// 悬浮预览的内容口径与主界面共用（定位各自实现：这里窗口只有 620×460，只上下二选一）
import {
  createPreviewBox,
  fillFiles,
  guardPreviewText,
  hasMoreToPreview,
  pickSlot,
  resetPreviewBox,
} from "./preview.js";

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

/** 后端随每次弹出下发的行为偏好。
 *  目前只有 `closeKey`（设置页录的关闭键，默认 `esc`，Esc 始终兜底可用）。
 *  「点选后保持打开」不需要前端参与 —— 收不收面板由后端 `paste()` 按配置决定。 */
let prefs = { closeKey: "esc" };

/** 悬浮预览。面板是单行列表，长文本一律被省略号截掉，悬浮补一份完整内容。
 *
 * 延迟比主界面的 1s 短一些（600ms）：面板一弹出就是要挑东西，扫过两三行确认是哪个
 * 的时间通常不到 1 秒；面板只有 460 高，弹得太勤反而挡住相邻行，所以也不能更短。 */
let previewBox = null;
let hoverTimer = null;
const HOVER_DELAY_MS = 600;

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
      schedulePreview(row, item);
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

// ── 悬浮预览 ────────────────────────────────────────────────────────────
//
// 只在「行上确实看不全」时弹（`hasMoreToPreview` 的判断与主界面同一份）：文本按后端
// 给的 `preview_truncated`，文件只有 1 个时不弹，图片总是弹（缩略图必然被缩过）。

function previewBoxEl() {
  if (!previewBox) previewBox = createPreviewBox();
  return previewBox;
}

function cancelPreview() {
  if (hoverTimer) {
    clearTimeout(hoverTimer);
    hoverTimer = null;
  }
}

function hidePreview() {
  cancelPreview();
  if (previewBox && !previewBox.hidden) resetPreviewBox(previewBox);
}

function schedulePreview(row, item) {
  cancelPreview();
  if (!hasMoreToPreview(item)) {
    hidePreview();
    return;
  }
  hoverTimer = setTimeout(() => {
    hoverTimer = null;
    showPreview(row, item);
  }, HOVER_DELAY_MS);
}

async function showPreview(row, item) {
  if (!row.isConnected) return;
  const box = previewBoxEl();

  if (item.kind === "image") {
    let url;
    try {
      // 直接用 Rust 侧的缩略图（长边 ≤512）。主界面那套缓存 + 视野释放是为滚动列表
      // 准备的，面板一次只悬浮一个，不需要。
      url = await invoke("get_image_thumb", { id: item.id });
    } catch {
      return;
    }
    if (!row.isConnected) return;
    const { img } = pickSlot(box, "image");
    img.onload = () => {
      if (!box.hidden) placePreview(box, row);
    };
    img.src = url;
    box.hidden = false;
    placePreview(box, row);
    return;
  }

  if (item.kind === "files") {
    const { files } = pickSlot(box, "files");
    fillFiles(files, item);
    box.hidden = false;
    placePreview(box, row);
    return;
  }

  // 文本：行上只有一行摘要，这里给全文的前几行（显示行数由 CSS 的 line-clamp 卡住）
  let data;
  try {
    data = await invoke("get_full_content", { id: item.id });
  } catch {
    return;
  }
  if (!row.isConnected) return;
  const { text } = pickSlot(box, "text");
  text.textContent = guardPreviewText(data.text || "");
  box.hidden = false;
  placePreview(box, row);
}

/** 浮层位置：优先贴行下方，放不下翻到上方，最后夹进窗口。
 *
 * 与主界面的四方向试探不同 —— 面板只有 620×460，左右都没有富余（浮层最宽 460），
 * 只在上/下之间选；两者都放不下时贴着能放下的那一边，宁可压住相邻行。 */
function placePreview(box, row) {
  const r = row.getBoundingClientRect();
  const w = box.offsetWidth;
  const h = box.offsetHeight;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const EDGE = 8;
  const GAP = 8;

  let top = r.bottom + GAP;
  if (top + h > vh - EDGE) top = r.top - GAP - h;
  top = Math.max(EDGE, Math.min(top, Math.max(EDGE, vh - h - EDGE)));
  const left = Math.max(EDGE, Math.min(r.left, Math.max(EDGE, vw - w - EDGE)));

  box.style.left = `${left}px`;
  box.style.top = `${top}px`;
}

// ── 键盘 ────────────────────────────────────────────────────────────────
document.addEventListener("keydown", (e) => {
  // 输入法组字期间的按键不算数：组字用 Esc 取消候选是常见操作
  if (e.isComposing) return;
  // 关闭键：设置里可以改，Esc 始终兜底。放在最前，否则 Shift+Esc 之类会落到
  // 下面的方向键分支里（那些分支只比 e.key，不看修饰键）
  if (e.key === "Escape" || matchesCombo(e, prefs.closeKey)) {
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

listEl.addEventListener("mouseleave", hidePreview);
listEl.addEventListener("scroll", hidePreview, { passive: true });

searchEl.addEventListener("input", () => {
  sel = 0;
  hidePreview();
  applyFilter();
});
searchEl.addEventListener("keydown", (e) => {
  // 搜索框里按方向键 / 回车由全局处理，这里只挡掉「输入法组字中」的误触
  if (e.isComposing) e.stopPropagation();
});

// 每次弹出：重拉数据、清空搜索、选中第一条
listen("panel-open", (ev) => {
  // payload 里带着当前设置：面板是常驻窗口，设置改过之后要靠这次推送才生效，
  // 而且不必为此多发一次 IPC（弹出路径上每一毫秒都要省）
  if (ev.payload && typeof ev.payload === "object") {
    prefs = { ...prefs, ...ev.payload };
  }
  hidePreview();
  reload();
  setTimeout(() => searchEl.focus(), 0);
});

reload().then(() => searchEl.focus());
