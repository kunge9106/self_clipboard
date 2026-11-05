/**
 * 提示气泡（tooltip）。
 *
 * 为什么自己画而不是用 `title`：
 *   1. `title` 由系统绘制 —— Windows 上是一块白底小方框，字体、圆角、阴影全不受控，
 *      在毛玻璃界面上像贴了张便签（实测反馈「很多地方气泡都没 UI」）；
 *   2. 延迟、样式、位置都改不了，也没法做「越界翻转」；
 *   3. 最重要的一条：**`disabled` 元素上 `title` 不触发**（浏览器不给禁用元素发鼠标
 *      事件），而「这个按钮为什么点不了」恰恰是最需要提示的场景。
 *
 * 但它也不是万能的：挂在 `disabled` 元素上的 `data-tip` 同样收不到鼠标事件。
 * 需要提示禁用原因时，把 `data-tip` 放在**外层容器**上（见设置页的下载按钮）。
 *
 * 与右键菜单 / toast 同一族外观（`--solid-bg` + `--elev-2`），不另造一套令牌；
 * 也不复用 `.hover-preview` —— 那是有内容的大预览浮层，用奶白渐变表达「实体感」，
 * 单行文字提示套渐变反而显脏。
 */

/** 悬停多久才出现。太短会跟着鼠标乱跳，太长会让人以为没有提示。 */
const DELAY = 420;
/** 与目标的间距，和 .tip 的定位保持一致 */
const GAP = 8;
/** 贴边时留的安全距离 */
const EDGE = 6;

export function createTip() {
  const el = document.createElement("div");
  el.className = "tip";
  el.setAttribute("role", "tooltip");
  el.hidden = true;
  document.body.appendChild(el);

  let timer = 0;
  let current = null;

  function place(target) {
    const r = target.getBoundingClientRect();
    const t = el.getBoundingClientRect();

    // 默认放下面、水平居中；放不下就翻到上方
    let top = r.bottom + GAP;
    if (top + t.height > window.innerHeight - EDGE) {
      const above = r.top - t.height - GAP;
      // 上方也放不下（元素占满高度）就贴着视口顶
      top = above >= EDGE ? above : Math.max(EDGE, window.innerHeight - t.height - EDGE);
    }
    // 水平夹在视口内：靠近左右边缘的按钮（窗口控制）不会把气泡挤出屏幕
    const left = Math.max(
      EDGE,
      Math.min(r.left + r.width / 2 - t.width / 2, window.innerWidth - t.width - EDGE)
    );

    el.style.top = `${Math.round(top)}px`;
    el.style.left = `${Math.round(left)}px`;
  }

  function show(target) {
    current = target;
    el.textContent = target.dataset.tip || "";
    el.hidden = false;
    // 先显示再量尺寸：宽度由文字长度决定，量完才知道该不该翻转
    place(target);
    requestAnimationFrame(() => el.classList.add("is-on"));
  }

  function hide() {
    if (timer) {
      clearTimeout(timer);
      timer = 0;
    }
    current = null;
    el.classList.remove("is-on");
    // 等淡出播完再 hidden，否则会看到「啪」地消失
    setTimeout(() => {
      if (!el.classList.contains("is-on")) el.hidden = true;
    }, 160);
  }

  // 事件委托：任何后加的 [data-tip] 都自动生效，不用逐个绑定
  document.addEventListener("mouseover", (e) => {
    const target = e.target?.closest?.("[data-tip]");
    if (!target || target === current) return;
    clearTimeout(timer);
    timer = setTimeout(() => show(target), DELAY);
  });

  document.addEventListener("mouseout", (e) => {
    const target = e.target?.closest?.("[data-tip]");
    if (!target) return;
    // 移到自己的子元素上不算离开（图标 / 文字节点之间移动会触发 mouseout）
    if (e.relatedTarget && target.contains(e.relatedTarget)) return;
    hide();
  });

  // 滚动、按下、按键都收起：气泡是 fixed 定位，滚起来会和目标脱节
  window.addEventListener("scroll", hide, true);
  document.addEventListener("mousedown", hide, true);
  window.addEventListener("keydown", hide, true);
  window.addEventListener("blur", hide);
}
