/* ===========================================================================
   动效系统 · 对外唯一入口
   ---------------------------------------------------------------------------
   业务只从这里 import，不直接碰 runtime / presets / controllers。
   这样以后重构内部结构（换动画后端、拆文件）不影响调用方。

   典型用法：
     1) 程序化播放
        import { play } from "./animations/index.js";
        play(el, "fade-up");
        play(el, "slide-in", { axis: "y", from: -1 });

     2) 页面/栏目切换（方向自动）
        swap(container, { axis: "x", index: 2, group: "settings" });
        // 首次调用只记索引、从默认侧进来；之后按索引差自动定方向

     3) 幂等挂载滑动指示块（新渲染的 DOM 要再调一次）
        initMotion(view);

   新增一个动效要改哪里：
     · 只是「又一处要浮起来」     → 复用 play(el, "fade-up")，不改代码
     · 需要新的动作形态           → presets.js 加一个 preset（关键帧 + 选令牌）
     · 需要新的时长/缓动档        → tokens.css 加令牌，presets 引用它
     · 需要新的有状态交互（如滑轨）→ controllers/ 加一个控制器，index.js 导出
   =========================================================================== */

import { mountSegmented } from "./controllers/segmented.js";
import { play, swap, shouldAnimate, tokens, resetSwap } from "./runtime.js";

/** 需要滑动指示块的容器：加新控件类型时在这里补选择器即可 */
const THUMB_HOSTS = ".seg, .settings-tabs";

/**
 * 扫描并挂载滑动指示块。**幂等**：已挂载的容器会直接跳过，
 * 所以设置页每次重绘后可以无脑调一次，不必记「哪些已经挂过」。
 *
 * @param {ParentNode} root 扫描范围，默认整个文档
 * @returns {number} 本次新挂载的数量（便于调试/断言）
 */
export function initMotion(root = document) {
  const hosts = root.querySelectorAll?.(THUMB_HOSTS);
  if (!hosts) return 0;
  let n = 0;
  hosts.forEach((el) => {
    if (mountSegmented(el)) n += 1;
  });
  return n;
}

export { play, swap, shouldAnimate, tokens, resetSwap, mountSegmented };
