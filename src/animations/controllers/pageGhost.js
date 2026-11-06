/* ===========================================================================
   动效系统 · 控制器：页面切换的「幽灵层」（交叉淡出）
   ---------------------------------------------------------------------------
   问题：设置页与侧栏是 `view.innerHTML = ...` **整体替换** —— 旧节点在赋值
   那一刻就被销毁，没有「淡出」的时间窗口，所以切换永远是「旧内容瞬间消失、
   新内容淡入」，缺少空间连续性。

   解法：替换**之前**把旧内容克隆一份，绝对定位覆盖在原位（幽灵层），
   让它与新内容同时动 —— 旧内容淡出并退开，新内容淡入并归位。两边同时开始、
   同时结束，总时长仍是 280ms（串行的「先淡出再淡入」会翻倍，桌面点击会觉得粘）。

   四个必须守住的细节（都是踩过或差点踩的）：

   1. **`pointer-events: none`** —— 幽灵层是视觉残留，不能吃鼠标。少了它，
      淡出的 280ms 里点什么都点不动。
   2. **`position: fixed` + 视口坐标** —— 不用 `inset: 0`。后者依赖某个祖先
      是 `position: relative`，而本项目的 `.view` 祖先并不保证。fixed 与祖先
      无关，应用窗口本身不滚动，280ms 内坐标不会失效。
   3. **追加到 `document.body` 末尾** —— 保证文档顺序在真实 `view` **之后**。
      克隆层带着和原页面一样的 id，若插在前面，`document.getElementById("set-theme")`
      之类的查询会命中假的幽灵元素（本项目有多处这种查询）。
   4. **单例** —— 连点切换时旧的立即移除，否则会叠出多层半透明页面。
   =========================================================================== */

import { play } from "../runtime.js";

const GHOST_CLASS = "view-ghost";
/** 同一时刻最多一层 */
let active = null;

/** 立刻丢弃当前幽灵层（切换失败、重复捕获时调用） */
export function discardGhost() {
  if (active) {
    active.remove();
    active = null;
  }
}

/**
 * 克隆当前页面做幽灵层。**必须在 `view.innerHTML = ...` 之前调用。**
 *
 * @param {Element} view 页面容器
 * @param {{scrollSelector?: string}} opts 需要同步滚动位置的内部容器
 * @returns {Element|null} 幽灵层；首屏无旧内容时返回 null
 */
export function captureGhost(view, opts = {}) {
  discardGhost();
  if (!view || !view.firstElementChild) return null;

  const rect = view.getBoundingClientRect();
  if (rect.width < 1 || rect.height < 1) return null;

  // 按设备像素对齐：`getBoundingClientRect()` 会返回分数像素（如 15.9997），
  // 幽灵层是 fixed 覆盖层，分数坐标在 125% / 150% 缩放下可能落到物理像素边界外，
  // 表现为边缘一条发虚的缝。量化到 1/DPR 的整数倍即可，代价是 4 次乘法。
  // 说明：幽灵层与新内容用的是同一份 rect，理论上本来就重合，这里是**防御性**
  // 处理，实际收益在常规缩放下几乎看不到。
  const dpr = window.devicePixelRatio || 1;
  const snap = (v) => Math.round(v * dpr) / dpr;

  const ghost = view.cloneNode(true);
  ghost.classList.add(GHOST_CLASS);
  // 纯装饰：读屏不该把同一份内容读两遍
  ghost.setAttribute("aria-hidden", "true");

  ghost.style.cssText = [
    "position: fixed",
    `left:${snap(rect.left)}px`,
    `top:${snap(rect.top)}px`,
    `width:${snap(rect.width)}px`,
    `height:${snap(rect.height)}px`,
    "z-index:5",
    "pointer-events:none",
  ].join(";");

  // 先入文档，再同步滚动。
  // ⚠ 顺序不能反：元素不在文档里时 `scrollTop` 赋值**不生效**（也没有报错），
  // 表现出来就是幽灵层永远从顶部开始，淡出瞬间内容「跳」一下。
  // 实测过：先赋值再 appendChild，读回来仍是 0。
  document.body.appendChild(ghost);

  const sel = opts.scrollSelector || ".settings-body";
  const fromBody = view.querySelector(sel);
  const toBody = ghost.querySelector(sel);
  if (fromBody && toBody) toBody.scrollTop = fromBody.scrollTop;

  active = ghost;
  return ghost;
}

/**
 * 让幽灵层退场并移除。与 `slide-in` 同时调用即为交叉淡出。
 *
 * @param {Element|null} ghost captureGhost 的返回值
 * @param {{axis?: "x"|"y", from?: number}} opts 与新内容入场**相同**的轴与方向
 */
export function releaseGhost(ghost, opts = {}) {
  if (!ghost || !ghost.isConnected) {
    if (ghost === active) active = null;
    return;
  }
  const { axis = "x", from = 1 } = opts;

  const done = () => {
    ghost.remove();
    if (active === ghost) active = null;
  };

  const anim = play(ghost, "ghost-out", { axis, from });
  // 降级（减少动效 / 页面不可见）时不播，直接移除 —— 不能留一层残影不动
  if (!anim) {
    done();
    return;
  }
  anim.finished.then(done).catch(done);
  // 兜底：finished 在极端情况下（元素被移除、动画被外力取消）可能不 resolve
  setTimeout(done, 600);
}
