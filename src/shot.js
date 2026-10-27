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
const bar = document.getElementById("sel-bar");

/** 当前选区；null = 还没框（或刚清空） */
let rect = null;
/** 拖拽状态：new 画新框 / move 平移 / resize 拽手柄 */
let drag = null;

const hoverEl = document.getElementById("hover");
const hoverSizeEl = document.getElementById("hover-size");

/** 覆盖层所在显示器左上角在虚拟屏里的物理坐标（后端下发） */
let screenLeft = 0;
let screenTop = 0;
/**
 * 光标下的窗口候选（CSS 坐标，z 序从高到低）。
 *
 * 后端在覆盖层显示**之前**枚举好一次性下发：覆盖层一铺满整屏，
 * `WindowFromPoint` 只会返回它自己，之后就再也问不出底下是谁了。
 * 命中判断放在本地，是因为鼠标移动每秒上百次，一次 IPC 往返就够掉帧。
 */
let winRects = [];
/** 当前悬停窗口（CSS 坐标）；null = 没有可吸附的窗口 */
let hoverRect = null;

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

/**
 * 把鼠标位置换算成屏幕物理坐标，返回命中的窗口矩形（CSS 坐标）。
 *
 * 取**第一个**命中的：后端下发的顺序就是 z 序，第一个就是最上层那个 ——
 * 这正是用户眼里「点下去应该截到谁」。
 */
function windowAt(cx, cy) {
  const { sx, sy } = imgScale();
  const px = cx * sx + screenLeft;
  const py = cy * sy + screenTop;
  for (const [l, t, r, b] of winRects) {
    if (px >= l && px < r && py >= t && py < b) {
      // 物理 → CSS：先减屏幕原点，再除以缩放比
      const x = (l - screenLeft) / sx;
      const y = (t - screenTop) / sy;
      const w = (r - l) / sx;
      const h = (b - t) / sy;
      // 夹进视口：窗口可能有一半在另一块屏上
      const x2 = clamp(x, 0, window.innerWidth);
      const y2 = clamp(y, 0, window.innerHeight);
      const x3 = clamp(x + w, 0, window.innerWidth);
      const y3 = clamp(y + h, 0, window.innerHeight);
      if (x3 - x2 < 2 || y3 - y2 < 2) continue;
      return { x: x2, y: y2, w: x3 - x2, h: y3 - y2 };
    }
  }
  return null;
}

/** 两个对角点 → 左上角 + 宽高（支持往任意方向拖） */
function boxOf(x1, y1, x2, y2) {
  const x = clamp(Math.min(x1, x2), 0, window.innerWidth);
  const y = clamp(Math.min(y1, y2), 0, window.innerHeight);
  const r = clamp(Math.max(x1, x2), 0, window.innerWidth);
  const b = clamp(Math.max(y1, y2), 0, window.innerHeight);
  return { x, y, w: r - x, h: b - y };
}

function renderHover() {
  // 已经有选区（含正在拖）就不画高亮：选区自己会把下面这一层盖住，
  // 再更新只是白白多算一次
  if (!hoverRect || rect) {
    hoverEl.hidden = true;
    return;
  }
  hoverEl.hidden = false;
  hoverEl.style.left = `${hoverRect.x}px`;
  hoverEl.style.top = `${hoverRect.y}px`;
  hoverEl.style.width = `${hoverRect.w}px`;
  hoverEl.style.height = `${hoverRect.h}px`;
  hoverEl.classList.toggle("is-tight", hoverRect.y < 30);
  const { sx, sy } = imgScale();
  hoverSizeEl.textContent = `${Math.round(hoverRect.w * sx)} × ${Math.round(hoverRect.h * sy)}`;
}

function render() {
  renderHover();
  if (!rect || rect.w < 1 || rect.h < 1) {
    sel.hidden = true;
    bar.hidden = true;
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
  placeBar();
}

/// 工具条贴着选区：优先右下外侧，放不下就翻到上面，再不行压在选区底部内侧。
/// 不能让它盖住选区 —— 用户框住的最后一行被按钮挡住，粘贴出来会莫名觉得「少了一块」。
function placeBar() {
  bar.hidden = false;
  const bw = bar.offsetWidth || 78;
  const bh = bar.offsetHeight || 42;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  let left = clamp(rect.x + rect.w - bw, 4, Math.max(4, vw - bw - 4));
  let top = rect.y + rect.h + 8;
  if (top + bh > vh - 4) top = rect.y - bh - 8;      // 下面放不下 → 上面
  if (top < 4) top = clamp(rect.y + rect.h - bh - 4, 4, vh - bh - 4); // 都放不下 → 压在内侧
  bar.style.left = `${left}px`;
  bar.style.top = `${top}px`;
}

function reset() {
  rect = null;
  drag = null;
  hoverRect = null;
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
  // 点在工具条上：交给按钮自己处理，否则会被当成「在空白处重新框选」，
  // 表现为「按钮按下去选区就没了」
  if (e.target.closest(".bar")) return;
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
  // 记下按下瞬间的高亮窗口：松手时若几乎没动，就当成「单击截整窗」，
  // 这样点一下和拖一下各走一条路，不用二段式交互。
  drag = { mode: "new", x0: e.clientX, y0: e.clientY, hover: hoverRect };
  rect = { x: e.clientX, y: e.clientY, w: 0, h: 0 };
  render();
});

document.addEventListener("mousemove", (e) => {
  if (!drag) {
    // 没在拖：跟随光标找窗口，先框出来告诉用户「点一下会截到哪」
    const hit = windowAt(e.clientX, e.clientY);
    const changed =
      (hit === null) !== (hoverRect === null) ||
      (hit && hoverRect && (hit.x !== hoverRect.x || hit.y !== hoverRect.y ||
        hit.w !== hoverRect.w || hit.h !== hoverRect.h));
    if (changed) {
      hoverRect = hit;
      render();
    }
    return;
  }
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
  const target = drag.hover;
  drag = null;
  // 只是点了一下（没拖动）：如果光标下正好有个窗口，就**直接截这个窗口**并完成 ——
  // 这就是「识别窗口、点一下就走」；没有窗口（比如点在桌面空白处）才当没框过，
  // 否则会留下一个 1×1 的选区，用户再双击就复制出一个像素点。
  if (wasNew && rect && (rect.w < 4 || rect.h < 4)) {
    if (target) {
      rect = target;
      render();
      commit();
      return;
    }
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
  // 在工具条上右击不该取消整次截图，避免误触
  if (e.target.closest(".bar")) return;
  cancel();
});

document.getElementById("btn-ok").addEventListener("click", (e) => {
  e.stopPropagation();
  commit();
});
document.getElementById("btn-cancel").addEventListener("click", (e) => {
  e.stopPropagation();
  cancel();
});

// ── 与后端 ──────────────────────────────────────────────────────────────

/// 正在提交：双击会连发 mousedown/up/dblclick，手快点两下绿勾也会发两次
//  `click`。第一次提交成功后后端会收起覆盖层并清掉待处理截图，第二次再去提交
//  就只会拿到「没有待处理的截图」，然后在前端弹一条「复制失败」——用户以为没
//  截图成功，换个位置又点一次。所以这里必须挡住重入。
let committing = false;

async function commit() {
  if (!rect || committing) return;
  committing = true;
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
  } finally {
    // 成功时窗口已经被后端收起，这个标志留着也无所谓；失败时要放开，
    // 否则用户「再双击重试」会被自己挡住。
    committing = false;
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
    // 窗口列表与图一起取：两者都来自这次抓屏，缺一个就没法做悬停高亮
    const [url, win] = await Promise.all([invoke("shot_image"), invoke("shot_windows")]);
    screenLeft = win[0];
    screenTop = win[1];
    winRects = win[2];
    img.src = url;
    // **等解码完成再让窗口出现**。只等 `load` 事件是不够的：那时图片还没解码成
    // 可绘制的位图，窗口先出来后必定有一帧黑底/空白 —— 用户看到的就是「按快捷键
    // 屏幕闪一下」。`decode()` 返回时首帧已经可以画了。
    try {
      await img.decode();
    } catch (e) {
      console.warn("img.decode failed", e);
    }
    await invoke("shot_show");
  } catch (e) {
    // 「没有待处理的截图」是预热窗口/页面首次加载时的正常情况（后端还没抓屏），
    // 不是错误。若按 ERROR 记，每次启动都会写一条并触发异常计数 —— 真正的故障
    // 反而会被这条噪音淹掉。
    if (String(e).includes("没有待处理的截图")) return;
    console.error("shot_image failed", e);
    invoke("frontend_log", { level: "ERROR", tag: "shot", msg: `取图失败：${e}` }).catch(() => {});
    // **这里绝不能调 `shot_show`**。`start()` 会在页面加载时先跑一次（那时还没有
    // 待处理的截图），预热窗口更是每次启动都会走到这里 —— 一旦 show，就会把一个
    // 全屏纯黑的空窗口亮在用户面前。要不要显示由后端兜底（3 秒未就绪即取消）决定。
  }
}

// 只保留 error 兜底：正常路径由 `load()` 里 `decode()` 之后调 `shot_show`
img.addEventListener("error", () => invoke("shot_show").catch(() => {}));

// 覆盖层是 `WS_EX_NOACTIVATE`（永不获焦），页面收不到 keydown —— Enter 由后端的
// 全局键盘钩子转发过来。这里只做「有没有选区」的判断，提交动作与双击、绿勾同一条路。
listen("shot-enter", () => {
  if (rect) commit();
});

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
