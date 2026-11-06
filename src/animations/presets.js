/* ===========================================================================
   动效系统 · L2 具名效果（presets）
   ---------------------------------------------------------------------------
   每个 preset 是纯函数：`(opts, t) => { keyframes, dur, ease }`

     opts 业务参数（轴线、方向、距离倍率…）—— 只放「这次想怎么动」
     t    令牌读取器 { ms, px, ease } —— 所有数值都从 L1 的 CSS 变量取

   **关键帧只在 L1/L2 这一层出现**。业务永远不写关键帧、不写时长、
   不写缓动，只写效果名。要调「进场有多快」，改 tokens.css 一处，全站生效。

   与 animations.css 里 `.fx-*` 的分工：
     · `.fx-*`  class  — 纯声明式，HTML 里写死、不需要方向或重播的场景
     · 本文件的 preset  — 需要程序控制（方向、串联、重播、中途取消）的场景
   两者共享同一套 tokens，所以调数值只有一处。名字刻意保持对应（fade-up / pop-in），
   看到 class 就知道对应的 preset 叫什么。
   =========================================================================== */

/** 生成位移表达式。axis 决定横向还是纵向，sign 决定从哪一侧进来。 */
function shift(axis, sign, px) {
  const fn = axis === "y" ? "translateY" : "translateX";
  return `${fn}(${sign * px}px)`;
}

/** 方向归一：业务可能传 1 / -1 / 0（0 或未传按「从右/从下」处理） */
function dir(from, fallback = 1) {
  if (from === -1) return -1;
  if (from === 1 || from === 0 || from == null) return 1;
  return from > 0 ? 1 : -1;
}

export const PRESETS = {
  /** 只淡入。用于「已经在原位、只需要出现」的东西 */
  "fade-in": () => ({
    keyframes: [{ opacity: 0 }, { opacity: 1 }],
    dur: "--dur-base",
    ease: "--ease-out",
  }),

  /** 淡入 + 小幅上浮。内容进场默认档 */
  "fade-up": (opts, t) => ({
    keyframes: [
      { opacity: 0, transform: shift("y", 1, t.px("--fx-shift-near")) },
      { opacity: 1, transform: "none" },
    ],
    dur: "--dur-base",
    // glide 而不是 out：out 曲线前端太陡，短位移看起来是「啪」地落位
    ease: "--ease-glide",
  }),

  /** 淡入 + 轻微放大。弹层/浮层用，带一点点回弹 */
  "pop-in": () => ({
    keyframes: [
      { opacity: 0, transform: "scale(0.96)" },
      { opacity: 1, transform: "none" },
    ],
    dur: "--dur-slow",
    ease: "--ease-spring",
  }),

  /**
   * 整块内容滑入 —— 栏目切换、侧栏分类切换的主效果。
   * axis: "x" 横向（设置页栏目） / "y" 纵向（侧栏分类）
   * from: +1 从右/下进来，-1 从左/上进来
   * 只做入场、不做离场：旧内容直接换掉再让新内容滑入，
   * 比「双轨同时动」少一半状态，也不会有两段动画抢同一个容器的问题。
   */
  "slide-in": (opts, t) => {
    const axis = opts.axis === "y" ? "y" : "x";
    const sign = dir(opts.from);
    const px = t.px("--fx-shift-far") * (opts.scale || 1);
    return {
      keyframes: [
        { opacity: 0, transform: shift(axis, sign, px) },
        { opacity: 1, transform: "none" },
      ],
      // 整块内容滑入用 slow(0.34) + glide：距离缩短到 12px 之后，
      // 0.22s 会显得「一闪而过」。位移越小、时长越要够，才看得出是滑进去的。
      dur: "--dur-slow",
      ease: "--ease-glide",
    };
  },

  /**
   * 离场：方向与入场相反、时长更短。
   * 目前只有弹层关闭用得到；保留它是因为「进场 base / 离场 fast」是通例，
   * 等到别处需要时不必再想一遍用哪档。
   */
  "slide-out": (opts, t) => {
    const axis = opts.axis === "y" ? "y" : "x";
    const sign = -dir(opts.from);
    const px = t.px("--fx-shift-near") * (opts.scale || 1);
    return {
      keyframes: [
        { opacity: 1, transform: "none" },
        { opacity: 0, transform: shift(axis, sign, px) },
      ],
      dur: "--dur-fast",
      ease: "--ease-in",
    };
  },
};

/** preset 名字白名单，供调用方做参数校验（也方便以后做调试面板） */
export const PRESET_NAMES = Object.keys(PRESETS);
