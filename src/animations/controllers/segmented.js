/* ===========================================================================
   动效系统 · 控制器：互斥选择的滑动指示块（segmented）
   ---------------------------------------------------------------------------
   给「一排选项里只有一个选中」的控件加一块会滑动的底：
   选中项变化时，底块带着轻微回弹从旧位置滑到新位置（弹性吸附）。

   覆盖两种容器，靠选中类区分，业务零改动：
     · `.seg`            选中类 `.on`         （分段控件：深色/浅色/系统、截图方式…）
     · `.settings-tabs`  选中类 `.is-active`  （设置页栏目：常规/外观/数据/关于）

   为什么用注入元素而不是改业务 DOM：
     业务每次重绘都会重建按钮（设置页整块 innerHTML），如果让业务维护这块底，
     每个渲染函数都得记得带上它 —— 迟早漏一处。控制器自己挂、自己跟，
     容器被销毁时它跟着消失，业务完全无感。

   渐进增强：只有挂载成功才给容器加 `.has-seg-thumb`，
   选中项的背景由 CSS 在该类下让位给滑块；JS 没跑/出错时仍是原来的实底高亮。
   =========================================================================== */

import { shouldAnimate } from "../runtime.js";

/** 已挂载的容器，避免重复注入 */
const mounted = new WeakSet();

export function mountSegmented(container) {
  if (!container || mounted.has(container)) return null;

  const isTabs = container.classList.contains("settings-tabs");
  // 选中类：栏目用 is-active，分段用 on。以后新增同类控件只在这里加一条。
  const activeSel = isTabs ? ".is-active" : ".on";

  const thumb = document.createElement("i");
  thumb.className = "seg-thumb";
  thumb.setAttribute("aria-hidden", "true"); // 纯装饰，读屏不该读到
  container.appendChild(thumb);
  container.classList.add("has-seg-thumb");
  mounted.add(container);

  let firstPaint = true;

  /** 把底块对齐到当前选中项。animate=false 时瞬时归位（首帧/尺寸变化用） */
  const sync = (animate) => {
    const active = container.querySelector(activeSel);
    if (!active) {
      thumb.style.opacity = "0";
      return;
    }
    // 首帧和「还没动过」时不做过渡：否则挂载瞬间会从 0,0 滑到目标位置
    const motion = animate && !firstPaint && shouldAnimate();
    if (!motion) {
      thumb.style.transition = "none";
    } else {
      thumb.style.transition = "";
    }
    thumb.style.opacity = "1";
    thumb.style.width = `${active.offsetWidth}px`;
    thumb.style.height = `${active.offsetHeight}px`;
    // offsetLeft/Top 相对 offsetParent，与 thumb 的 absolute 起点一致
    thumb.style.transform = `translate(${active.offsetLeft}px, ${active.offsetTop}px)`;
    if (!motion) {
      // 强制回流后再恢复过渡，否则 next frame 的过渡会把这一步也动画了
      void thumb.offsetWidth;
      thumb.style.transition = "";
    }
    firstPaint = false;
  };

  sync(false);

  // 选中项变化：业务只改 class，这里跟着滑。监听 class 而不是 click，
  // 因为选中态也可能被程序改动（例如主题模式从系统变化同步过来）。
  const mo = new MutationObserver(() => sync(true));
  mo.observe(container, {
    subtree: true,
    attributes: true,
    attributeFilter: ["class"],
  });

  // 尺寸变化（窗口缩放、字体回退、内容换行）时瞬时归位，不要滑。
  //
  // ⚠ 这里有两个必须同时成立的约束，缺一个动画就永远不播：
  //
  // ① **只监听容器，不监听选中项**。选中项是每次切换都会变的东西，
  //    监听它等于「一切换就 sync(false)」，等于把滑动掐掉。
  // ② **小于 2px 的变化视为抖动，直接忽略**。切换选中项时字重从 400 变 600、
  //    选中块的内阴影消失，容器宽度会抖 1px（实测 184 → 185）。ResizeObserver
  //    对 1px 也会回调，于是刚启动的过渡当场被 sync(false) 覆盖。
  //
  // 现象就是「滑块一下子跳过去、傻快」：实测切换后 39ms 就到终点、
  // thumb.getAnimations() 为空 —— 不是曲线快，是动画根本没播。
  // 判定用 ≥2px 而不是 >0：窗口缩放、字体回退这类真实布局变化远大于 2px，
  // 不会被漏掉；而选中态引起的亚像素抖动不会误伤动画。
  let lastW = container.offsetWidth;
  let lastH = container.offsetHeight;
  const ro = new ResizeObserver(() => {
    const w = container.offsetWidth;
    const h = container.offsetHeight;
    if (Math.abs(w - lastW) < 2 && Math.abs(h - lastH) < 2) return;
    lastW = w;
    lastH = h;
    sync(false);
  });
  ro.observe(container);

  return {
    sync: () => sync(true),
    destroy() {
      mo.disconnect();
      ro.disconnect();
      thumb.remove();
      container.classList.remove("has-seg-thumb");
      mounted.delete(container);
    },
  };
}
