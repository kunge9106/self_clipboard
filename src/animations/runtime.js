/* ===========================================================================
   动效系统 · L3 运行时（runtime）
   ---------------------------------------------------------------------------
   业务与 presets 之间的唯一执行层。它负责四件**必须集中做**的事：

     1. 读令牌   —— 数值只在 CSS 里定义，JS 不复制第二份
     2. 降级     —— 减少动效 / 页面隐藏 / 不支持 WAAPI，三处判断只写一次
     3. 唯一播放入口 play()，同元素重复播放先取消旧动画（避免叠在一起抖）
     4. 方向记忆 swap()：按 group 记住上一次索引，自动算「从哪一侧进来」

   业务永远不直接调 `el.animate()`。原因：一旦散开写，降级就会漏 ——
   系统关掉动效后仍有几处照动，是最常见的无障碍回归。
   =========================================================================== */

import { PRESETS } from "./presets.js";

/** 记住每个元素上正在播的动画，重播时先取消 */
const running = new WeakMap();

/** 读 L1 令牌。每次现读：主题切换会改 `:root` 上的值，缓存反而会过期。 */
export function tokens() {
  const cs = getComputedStyle(document.documentElement);
  return {
    ms(name) {
      const v = parseFloat(cs.getPropertyValue(name));
      return Number.isFinite(v) ? v * 1000 : 0;
    },
    px(name) {
      const v = parseFloat(cs.getPropertyValue(name));
      return Number.isFinite(v) ? v : 0;
    },
    ease(name) {
      return cs.getPropertyValue(name).trim() || "linear";
    },
  };
}

/**
 * 现在是否允许动效 —— **全项目唯一判断点**。
 * 三个理由任一成立就不动：
 *   · 用户在系统里关掉了动画（无障碍基线，不是可选项）
 *   · 页面不可见（后台标签/最小化，动了也看不见，白烧 GPU）
 *   · 环境不支持 Web Animations（极旧内核的兜底）
 */
export function shouldAnimate() {
  if (typeof Element.prototype.animate !== "function") return false;
  if (document.hidden) return false;
  try {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return false;
  } catch {
    /* matchMedia 不可用的环境按「允许」处理，不让降级逻辑自己抛错 */
  }
  return true;
}

/**
 * 播放一个具名效果。返回 Animation 或 null（被降级时不播）。
 *
 * @param {Element} el
 * @param {string}  name  PRESETS 里的名字
 * @param {object}  opts  传给 preset 的参数（axis / from / delay …）
 */
export function play(el, name, opts = {}) {
  const factory = PRESETS[name];
  if (!el || !factory) {
    if (name) console.warn(`[motion] 未知效果：${name}`);
    return null;
  }
  // 降级时**什么都不做**：关键帧都是 from → to，元素的静态样式已经是 to 态，
  // 不播就是正确终态。不需要再手动补样式。
  if (!shouldAnimate()) return null;

  const t = tokens();
  const spec = factory(opts, t);

  // 同一元素上重播：先取消旧的，否则两段动画会互相拉扯（表现为抖一下）
  running.get(el)?.cancel();

  const anim = el.animate(spec.keyframes, {
    duration: t.ms(spec.dur),
    easing: t.ease(spec.ease),
    delay: opts.delay || 0,
    // both：起始态在 delay 期间也生效，避免先闪一帧「已显示」再动
    fill: "both",
  });

  running.set(el, anim);
  anim.finished
    ?.then(() => {
      if (running.get(el) === anim) running.delete(el);
    })
    .catch(() => {
      /* cancel() 会让 finished reject，这是正常路径，不算错误 */
    });
  return anim;
}

/** 每个切换组的「上一次索引」——方向由索引差推出，业务不用自己记 */
const lastIndex = new Map();

/**
 * 内容切换入场。
 *
 * 业务只需要说清楚三件事：动哪个容器、哪根轴线、现在是第几项。
 * 方向由上一次索引自动推出（往后翻从右/下进来，往前翻从左/上进来），
 * 所以新增一个栏目只要调一次，不用再写「if (newIdx > oldIdx)」。
 *
 * @param {Element} el     要动的内容容器
 * @param {{axis?: "x"|"y", index?: number, group?: string}} opts
 */
export function swap(el, opts = {}) {
  const { axis = "x", index, group = "default" } = opts;
  let from = 1;
  if (typeof index === "number") {
    const prev = lastIndex.get(group);
    if (typeof prev === "number" && prev !== index) from = index > prev ? 1 : -1;
    lastIndex.set(group, index);
  }
  return play(el, "slide-in", { axis, from });
}

/** 忘记某个切换组的记忆（切换主体被销毁/重建时调用，避免方向错乱） */
export function resetSwap(group = "default") {
  lastIndex.delete(group);
}
