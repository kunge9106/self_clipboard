// 截图覆盖层：帧画面 + 拖拽框选 + 复制。
//
// 坐标全部按 **CSS px** 计算，而窗口的 webview 缩放已被 Rust 端设成 1/scale，
// 所以这里的 1 CSS px 就等于 1 物理像素 —— 交给后端裁剪时不需要任何换算。
// 这是整个模块唯一必须守住的前提，换算一旦出错，截图会整体偏几像素且很难看出来。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// 页面与视口自检：截图链路出问题时，第一个要确认的就是「页面到底有没有加载」
// 和「1 CSS px 是不是真的等于 1 物理 px」。这两条留作常驻日志，不删。
invoke("frontend_log", {
  level: "INFO",
  tag: "shot",
  msg: `覆盖层就绪 href=${location.href} viewport=${window.innerWidth}x${window.innerHeight} dpr=${window.devicePixelRatio}`,
}).catch(() => {});

const img = document.getElementById("shot-img");
const sel = document.getElementById("sel");
const sizeEl = document.getElementById("sel-size");
const hint = document.getElementById("hint");

/** 当前选区；null = 还没框（或刚清空） */
let rect = null;
/** 拖拽状态：new 画新框 / move 平移 / resize 拽手柄 */
let drag = null;

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

/**
 * 视口 → 图片像素的缩放比。
 *
 * 正常情况（全屏窗口 + zoom=1/scale）两者相等，sx/sy 都是 1，坐标可以直接用。
 * 但窗口客户区有可能比屏幕小几个像素（不可见的 resize 边框），这时如果直接把
 * CSS 坐标当像素坐标，选出来的区域会整体偏移 —— 而且偏移量小到肉眼看不出来，
 * 只有粘贴出来才发现切歪了。所以统一走这个比例换算，宁可多一次乘法。
 */
function imgScale() {
  const iw = img.naturalWidth || window.innerWidth || 1;
  const ih = img.naturalHeight || window.innerHeight || 1;
  return {
    sx: iw / (window.innerWidth || 1),
    sy: ih / (window.innerHeight || 1),
  };
}

/** 两个对角点 → 左上角 + 宽高（支持往任意方向拖） */
function boxOf(x1, y1, x2, y2) {
  const x = clamp(Math.min(x1, x2), 0, window.innerWidth);
  const y = clamp(Math.min(y1, y2), 0, window.innerHeight);
  const r = clamp(Math.max(x1, x2), 0, window.innerWidth);
  const b = clamp(Math.max(y1, y2), 0, window.innerHeight);
  return { x, y, w: r - x, h: b - y };
}

function render() {
  if (!rect || rect.w < 1 || rect.h < 1) {
    sel.hidden = true;
    hint.hidden = false;
    return;
  }
  sel.hidden = false;
  hint.hidden = true;
  sel.style.left = `${rect.x}px`;
  sel.style.top = `${rect.y}px`;
  sel.style.width = `${rect.w}px`;
  sel.style.height = `${rect.h}px`;
  // 选区贴着屏幕顶端时，尺寸角标翻到下方，否则会被切掉
  sel.classList.toggle("is-tight", rect.y < 30);
  // 角标显示的是**将要从图里裁出的像素**，不是屏幕上的 CSS 尺寸
  const { sx, sy } = imgScale();
  sizeEl.textContent = `${Math.round(rect.w * sx)} × ${Math.round(rect.h * sy)}`;
}

function reset() {
  rect = null;
  drag = null;
  render();
}

/** 拽手柄：按 handle 名称里的方位词调整对应的边 */
function resizeBy(base, handle, dx, dy) {
  let { x, y, w, h } = base;
  if (handle.includes("w")) {
    const nx = clamp(x + dx, 0, x + w - 1);
    w += x - nx;
    x = nx;
  }
  if (handle.includes("e")) {
    w = clamp(w + dx, 1, window.innerWidth - x);
  }
  if (handle.includes("n")) {
    const ny = clamp(y + dy, 0, y + h - 1);
    h += y - ny;
    y = ny;
  }
  if (handle.includes("s")) {
    h = clamp(h + dy, 1, window.innerHeight - y);
  }
  return { x, y, w, h };
}

// ── 指针 ────────────────────────────────────────────────────────────────

document.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return; // 右键交给 contextmenu 处理（取消）
  e.preventDefault();

  const handle = e.target.dataset?.h;
  if (handle && rect) {
    drag = { mode: "resize", handle, x0: e.clientX, y0: e.clientY, base: { ...rect } };
    return;
  }
  if (rect && e.target.closest("#sel")) {
    drag = { mode: "move", x0: e.clientX, y0: e.clientY, base: { ...rect } };
    return;
  }
  // 空白处按下：开新框。此时旧选区先清掉，避免画出「两个框」
  drag = { mode: "new", x0: e.clientX, y0: e.clientY };
  rect = { x: e.clientX, y: e.clientY, w: 0, h: 0 };
  render();
});

document.addEventListener("mousemove", (e) => {
  if (!drag) return;
  if (drag.mode === "new") {
    rect = boxOf(drag.x0, drag.y0, e.clientX, e.clientY);
  } else if (drag.mode === "move") {
    const dx = e.clientX - drag.x0;
    const dy = e.clientY - drag.y0;
    rect = {
      x: clamp(drag.base.x + dx, 0, window.innerWidth - drag.base.w),
      y: clamp(drag.base.y + dy, 0, window.innerHeight - drag.base.h),
      w: drag.base.w,
      h: drag.base.h,
    };
  } else {
    rect = resizeBy(drag.base, drag.handle, e.clientX - drag.x0, e.clientY - drag.y0);
  }
  render();
});

document.addEventListener("mouseup", () => {
  if (!drag) return;
  const wasNew = drag.mode === "new";
  drag = null;
  // 只是点了一下（没拖动）就当没框过：否则会留下一个 1×1 的选区，
  // 用户再双击就会复制出一个像素点
  if (wasNew && rect && (rect.w < 4 || rect.h < 4)) {
    rect = null;
  }
  render();
});

// 双击 / 回车 = 复制；右键 / Esc = 取消
document.addEventListener("dblclick", () => {
  if (rect) commit();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    cancel();
  } else if (e.key === "Enter") {
    e.preventDefault();
    if (rect) commit();
  }
});

document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  cancel();
});

// ── 与后端 ──────────────────────────────────────────────────────────────

async function commit() {
  if (!rect) return;
  const { sx, sy } = imgScale();
  const payload = {
    x: Math.round(rect.x * sx),
    y: Math.round(rect.y * sy),
    w: Math.round(rect.w * sx),
    h: Math.round(rect.h * sy),
  };
  try {
    await invoke("shot_commit", payload);
    reset();
  } catch (e) {
    // 失败时**保留选区**并说明原因：清掉选区会变成「双击了，什么都没发生」，
    // 用户只能猜。保留着他可以再双击重试，或者 Esc 退出。
    console.error("shot_commit failed", e);
    hint.hidden = false;
    hint.textContent = `复制失败：${e}`;
  }
}

async function cancel() {
  reset();
  try {
    await invoke("shot_cancel");
  } catch (e) {
    console.error("shot_cancel failed", e);
  }
}

/** 拉图 → 解码完成后再让后端显示窗口（顺序反了会先闪一帧空白） */
async function load() {
  reset();
  try {
    img.src = await invoke("shot_image");
    invoke("frontend_log", { level: "INFO", tag: "shot", msg: "已取到位图，等待解码" }).catch(() => {});
  } catch (e) {
    console.error("shot_image failed", e);
    invoke("frontend_log", { level: "ERROR", tag: "shot", msg: `取图失败：${e}` }).catch(() => {});
    // 拿不到图也必须把窗口亮出来，否则按了快捷键像没反应
    invoke("shot_show").catch(() => {});
  }
}

img.addEventListener("load", () => invoke("shot_show").catch(() => {}));
img.addEventListener("error", () => invoke("shot_show").catch(() => {}));

// 两种进入方式都要覆盖：
// 1. 窗口已被复用 → 后端 emit `shot-open`，这里收到就拉图；
// 2. 窗口是刚创建的（首次截图）→ 页面脚本跑起来时 `shot-open` 早就发出去了，
//    事件已经丢了，必须在注册完之后主动拉一次。
// 用 `pending` 标记去重，避免两种情况同时成立时拉两遍。
let pending = false;
listen("shot-open", () => {
  pending = true;
  load();
}).then(() => {
  // 监听注册完成时还没收到过事件 → 这是首次创建的窗口，主动拉一次
  if (!pending) load();
});
