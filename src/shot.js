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
/**
 * 是否处于「本次截图已就绪」状态：拿到图并显示之后才是 true，收尾后立刻 false。
 *
 * 后端钩子会把每一次左键按下转发过来（它无法区分自己该不该管），所以这里必须有
 * 一道闸：否则截图结束之后你再点任何地方，都会凭空冒出一个选区 —— 看起来就像
 * 「点完完成又回到截图状态」。
 */
let armed = false;

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
  // 收尾即解除武装：之后任何转发来的按下都不再开新选区
  armed = false;
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
    drag = { mode: "resize", handle, x0: e.clientX, y0: e.clientY, base: { ...rect }, t: Date.now() };
    return;
  }
  if (rect && e.target.closest("#sel")) {
    // 选区内第二次快速按下 = 双击完成（`dblclick` 在这里合成不出来，自己判）
    const now = performance.now();
    if (now - lastSelDown < 400) {
      lastSelDown = 0;
      commit();
      return;
    }
    lastSelDown = now;
    drag = { mode: "move", x0: e.clientX, y0: e.clientY, base: { ...rect }, t: Date.now() };
    return;
  }
  // 空白处按下：开新框。此时旧选区先清掉，避免画出「两个框」
  // 记下按下瞬间的高亮窗口：松手时若几乎没动，就当成「单击截整窗」，
  // 这样点一下和拖一下各走一条路，不用二段式交互。
  drag = { mode: "new", x0: e.clientX, y0: e.clientY, hover: hoverRect, t: Date.now() };
  rect = { x: e.clientX, y: e.clientY, w: 0, h: 0 };
  render();
});

/** 把指针位置应用到当前拖拽上。DOM 的 `mousemove` 与钩子转发的 `shot-move`
 *  共用这一份 —— 覆盖层是 NOACTIVATE 窗口，DOM 的移动事件也不保证每次都到。 */
function updateDrag(cx, cy) {
  if (drag.mode === "new") {
    rect = boxOf(drag.x0, drag.y0, cx, cy);
  } else if (drag.mode === "move") {
    const dx = cx - drag.x0;
    const dy = cy - drag.y0;
    rect = {
      x: clamp(drag.base.x + dx, 0, window.innerWidth - drag.base.w),
      y: clamp(drag.base.y + dy, 0, window.innerHeight - drag.base.h),
      w: drag.base.w,
      h: drag.base.h,
    };
  } else {
    rect = resizeBy(drag.base, drag.handle, cx - drag.x0, cy - drag.y0);
  }
  render();
}

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
  updateDrag(e.clientX, e.clientY);
});

/** 松开左键的收尾。DOM 的 mouseup 与钩子转发的 shot-up 共用这一份，
 *  两边都到也不会重复（`drag` 为 null 就是已经收过了）。 */
function endDrag() {
  const wasNew = drag.mode === "new";
  const target = drag.hover;
  drag = null;
  // 只是点了一下（没拖动）：如果光标下正好有个窗口，就**直接截这个窗口**并完成 ——
  // 这就是「识别窗口、点一下就走」；没有窗口（比如点在桌面空白处）才当没框过，
  // 否则会留下一个 1×1 的选区，用户再双击就复制出一个像素点。
  if (wasNew && rect && (rect.w < 4 || rect.h < 4)) {
    if (target) {
      // 选中这个窗口，但**不直接提交**：给用户留一条「看清楚了再点绿勾」的路，
      // 也给「其实想拖框、只是手抖点了一下」留个反悔的机会（再拖一次即可）。
      rect = target;
      render();
      return;
    }
    rect = null;
  }
  render();
}

document.addEventListener("mouseup", () => {
  if (!drag) return;
  endDrag();
});

// 双击 = 复制。同样不能用 `dblclick`（同上：NOACTIVATE 窗口合成不出这个事件），
// 自己在选区内两次快速按下里判。
let lastSelDown = 0;

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    cancel();
  } else if (e.key === "Enter") {
    e.preventDefault();
    if (rect) commit();
  }
});

// 拦掉浏览器默认右键菜单（WebView 里一般是空白；挡一下更稳）
document.addEventListener("contextmenu", (e) => e.preventDefault());

// 右键取消也走 `mousedown`：`contextmenu` 同样是合成事件，NOACTIVATE 窗口里不可靠
document.addEventListener("mousedown", (e) => {
  if (e.button !== 2) return;
  // 在工具条上右击不该取消整次截图，避免误触
  if (e.target.closest(".bar")) return;
  e.preventDefault();
  cancel();
});

// 按钮一律用 `mousedown` 而不是 `click` —— 这不是风格选择，是必需的：
// 覆盖层是 `WS_EX_NOACTIVATE`，窗口从不被激活，Chromium 因此**不合成 click**
// （它认为这次点击没有"完整落在激活窗口上"）。表现就是：能拖能框，但点绿勾
// 毫无反应、图也进不了轻贴。`mousedown` 是最底层事件，一定派发。
//
// `preventDefault` 是为了不让这次按下落到 document 的框选逻辑里；
// `stopPropagation` 同理，别让它顺带把选区清掉。
document.getElementById("btn-ok").addEventListener("mousedown", (e) => {
  e.preventDefault();
  e.stopPropagation();
  commit();
});
document.getElementById("btn-cancel").addEventListener("mousedown", (e) => {
  e.preventDefault();
  e.stopPropagation();
  cancel();
});

// ── 与后端 ──────────────────────────────────────────────────────────────

/// 正在提交：双击会连发 mousedown/up/dblclick，手快点两下绿勾也会发两次
//  `click`。第一次提交成功后后端会收起覆盖层并清掉待处理截图，第二次再去提交
//  就只会拿到「没有待处理的截图」，然后在前端弹一条「复制失败」——用户以为没
//  截图成功，换个位置又点一次。所以这里必须挡住重入。
let committing = false;

/** 把前端的关键分支写进后端日志：出问题时不用靠猜，也不用开 devtools */
function trace(msg) {
  invoke("frontend_log", { level: "INFO", tag: "shot", msg }).catch(() => {});
}

async function commit() {
  // 「点完成没反应」这类问题只能靠这两行区分：是压根没走到这里，
  // 还是走到了但被判成「没有选区 / 正在提交」
  trace(`commit 调用：rect=${rect ? JSON.stringify(rect) : "null"} committing=${committing}`);
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
    trace(`commit 成功 ${payload.w}×${payload.h}`);
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
  // **从拉图这一刻就认按下**。早先是等 `shot_show` 返回后才置位，而窗口在
  // `shot_show` 执行时就已经显示出来了 —— 手快的话那一下正好落在「窗口已可见、
  // 标志还没置上」的缝里，被丢掉，于是「还得再点一次」。
  // 提前置位是安全的：钩子只在覆盖层**可见**时才转发按下，预热阶段不会有事件。
  armed = true;
  try {
    // 窗口列表与图一起取：两者都来自这次抓屏，缺一个就没法做悬停高亮
    const t0 = performance.now();
    const [url, win] = await Promise.all([invoke("shot_image"), invoke("shot_windows")]);
    const t1 = performance.now();
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
    const t2 = performance.now();
    await invoke("shot_show");
    const t3 = performance.now();
    // 分段计时：取图 / 解码 / 显示三段，哪一段慢一看就知道，不用再靠猜
    trace(
      `load 分段：取图 ${Math.round(t1 - t0)}ms · 解码 ${Math.round(t2 - t1)}ms · 显示 ${Math.round(t3 - t2)}ms`
    );
    // 用户按下快捷键后往往已经在拖了，而画面要 150–300ms 才出来 —— 那一下按在
    // 下层窗口上，等于白按，感觉就是「还得再点一次」。这里问一句「左键还按着吗」，
    // 按着就把当前光标位置当成框选起点，手感和「一出现就跟着我走」一致。
    try {
      // 1) 先取「窗口出现之前那一次按下」：这是「还得再点一次」的根因 ——
      //    按完快捷键手已经在拖，那一下落在下面的窗口上，等你看到画面早已松手。
      const p = await invoke("shot_pointer");
      if (p && !drag) {
        const [px, py, , held] = p;
        const { sx, sy } = imgScale();
        const x = (px - screenLeft) / sx;
        const y = (py - screenTop) / sy;
        const inside = x >= 0 && y >= 0 && x <= window.innerWidth && y <= window.innerHeight;
        if (inside) {
          if (held) {
            // 手还按着：直接用那次按下的位置当框选起点，拖动立刻跟手
            trace(`补上窗口出现前的按下 (${Math.round(x)},${Math.round(y)})`);
            drag = { mode: "new", x0: x, y0: y, hover: windowAt(x, y), t: Date.now() };
            rect = { x, y, w: 0, h: 0 };
            render();
          } else {
            // 已经松手了：**绝不能**建拖拽，否则会等一个永远不来的 mouseup，
            // `drag` 卡死后连按钮都点不动。当成一次单击（有窗口就选中它）。
            const target = windowAt(x, y);
            if (target) {
              trace("补上窗口出现前的单击：选中该窗口");
              hoverRect = target;
              rect = target;
              render();
            }
          }
        }
      }
    } catch (e) {
      trace(`shot_pointer 失败：${e}`);
    }
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

// 后端的「首次按下补偿」：WebView2 偶尔仍会把第一次按下当成「先聚焦页面」吞掉。
// 钩子把这次按下原样发过来，这里就地开始框选 —— DOM 的 pointerdown 若也到了，
// 会看到 `drag` 已存在而跳过，两条路谁先到都不重复。
listen("shot-down", (e) => {
  // `armed` 是必须的：钩子对所有左键按下都转发，截图结束后那一下不该再开选区
  if (!armed) return;
  const [px, py, t] = e.payload || [];
  if (!Number.isFinite(px)) return;
  // 迟到的事件（比如上一次拖拽结束后才排到）不能用来开新框
  if (Math.abs(Date.now() - t) > 400) return;
  const { sx, sy } = imgScale();
  const x = (px - screenLeft) / sx;
  const y = (py - screenTop) / sy;
  if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) return;

  // **按钮判定必须排在 `drag` 检查之前**。覆盖层是 NOACTIVATE 窗口，按钮的
  // mousedown/click 能不能到页面并不稳定（实测点绿勾毫无反应）；而万一某次
  // `mouseup` 也丢了，`drag` 就会一直卡在「拖拽中」，把后面每次点击都挡掉 ——
  // 那样绿勾永远点不动。钩子是系统级事件，坐标命中不依赖事件派发。
  const hit = document.elementFromPoint(x, y);
  if (hit && hit.closest(".bar")) {
    if (hit.closest(".is-ok")) {
      trace("工具条：完成（钩子命中）");
      commit();
    } else if (hit.closest(".is-cancel")) {
      trace("工具条：取消（钩子命中）");
      cancel();
    }
    return;
  }

  // 卡死的拖拽自愈：`drag` 存在但两秒内一个像素都没动过，说明对应的 `mouseup`
  // 丢了（钩子/页面事件偶尔不齐）。此时不能让它继续挡着后续所有按下。
  if (drag && Date.now() - (drag.t || 0) > 2000 && rect && rect.w < 4 && rect.h < 4) {
    trace("拖拽状态卡死，自动复位");
    drag = null;
  }
  if (drag) return;
  trace(`首次按下 (${Math.round(x)},${Math.round(y)}) 开始框选`);
  drag = { mode: "new", x0: x, y0: y, hover: windowAt(x, y), t };
  rect = { x, y, w: 0, h: 0 };
  render();
});

// 移动也由钩子兜一道（后端按 16ms 节流）：NOACTIVATE 窗口里 DOM 的 mousemove
// 同样可能丢，而拖动时只要漏几次，用户就觉得「按下没反应」。
listen("shot-move", (e) => {
  if (!armed || !drag) return;
  const [px, py] = e.payload || [];
  if (!Number.isFinite(px)) return;
  const { sx, sy } = imgScale();
  updateDrag((px - screenLeft) / sx, (py - screenTop) / sy);
});

// 抬起也由钩子兜一道：DOM 的 mouseup 若丢一次，`drag` 会一直卡着不放手，
// 表现为「拖完一次之后再也框不动、按钮也点不动」。
listen("shot-up", (e) => {
  if (!armed || !drag) return;
  // 抬起的时间必须**不早于**本次按下：钩子事件的到达顺序不保证，一个迟到的
  // `up`（比如上一次点击的）会把刚建好的拖拽立刻清掉，表现为「拖不动，得再点」。
  const t = e.payload;
  if (Number.isFinite(t) && Number.isFinite(drag.t) && t < drag.t) {
    trace("忽略迟到的抬起事件");
    return;
  }
  endDrag();
});

// 右键由钩子转发：`contextmenu` 在这里同样不合成
listen("shot-right", () => {
  if (!armed) return;
  cancel();
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
