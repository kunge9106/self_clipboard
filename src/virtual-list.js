// 窗口化列表：只渲染视口附近的那几十行，其余用「骨架行」占位。
//
// 为什么必须做：列表的 DOM 节点数直接决定渲染进程内存与滚动性能。旧的渐进渲染
// 「每 60 条一批、用完不回收」意味着滚到底会积累 10 万个节点 —— 这才是内存随使用
// 时间上涨的主因（`content-visibility` 只省 layout/paint，节点还在）。
//
// 行高策略：**不硬编码**。卡片高度只由 kind/subtype 决定（这是样式契约，见
// styles.css 里 `.card` 的注释），所以启动时渲染 5 个隐藏探针，用
// `getBoundingClientRect()` 实测出「类高度表」；CSS 改了字号/行高/DPI 缩放，
// 表会自动跟着变。已渲染行再挂 `ResizeObserver`：实测与类高度差 >0.5px 就更新
// 类表并重排（正常不会触发，属于「样式被后人改坏了也不至于错位」的保险丝）。
//
// 未加载区域的高度只能用**平均值**估（后端没有下发每条的高度类）。代价是滚动条
// 长度在滚动过程中会逐步收敛而不是一开始就精确；换来的是不必为「滚到 60%」再
// 多开一个接口。估算值变化超过阈值才重排，避免每加载一页都抖一次。
//
// 纯函数（偏移表、二分定位、可见区间、锚点换算）全部导出，供 `node --test` 直接测。

/** 行高类：0 文本 / 1 代码 / 2 Markdown / 3 文件 / 4 图片。 */
export const CLASS_COUNT = 5;

/** 一条记录属于哪个行高类。与 `styles.css` 的卡片排版一一对应。 */
export function heightClassOf(item) {
  if (!item) return 0;
  if (item.kind === "image") return 4;
  if (item.kind === "files") return 3;
  if (item.subtype === "code") return 1;
  if (item.subtype === "markdown") return 2;
  return 0;
}

/**
 * 算偏移表：`offsets[i]` 是第 i 行的**上沿**，`offsets[total]` 是总高。
 *
 * @param {{total:number, estimate:number, gap:number, known?:Map<number,number>, classHeights?:number[], rowClass?: (i:number)=>number}} p
 *   `known`：下标 → 实测高度（只包含真正渲染过的行，通常几十条）
 *   `rowClass`：下标 → 行高类（只对已加载的行有值；没有就用 `estimate`）
 */
export function computeOffsets({ total, estimate, gap, known, classHeights, rowClass }) {
  const out = new Float64Array(Math.max(0, total) + 1);
  let y = 0;
  for (let i = 0; i < total; i++) {
    out[i] = y;
    let h = known && known.get(i);
    if (h === undefined) {
      const c = rowClass ? rowClass(i) : -1;
      h = c >= 0 && classHeights ? classHeights[c] : estimate;
    }
    y += h + gap;
  }
  out[total] = Math.max(0, y - (total > 0 ? gap : 0)); // 末行后不留间距
  return out;
}

/**
 * 二分：`y` 落在第几行。返回值夹在 `[0, total-1]`。
 *
 * 调用点里最常见的是「scrollTop → 首行」，所以边界要定死：刚好等于某行上沿时
 * 返回那一行（不是上一行），否则滚动到正好对齐的位置会闪一下上一行。
 */
export function findIndex(offsets, y) {
  const total = offsets.length - 1;
  if (total <= 0) return 0;
  let lo = 0;
  let hi = total - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offsets[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/**
 * 可见区间（含 overscan）。返回的 `last` 是**闭区间**的最后一个下标。
 *
 * `total === 0` 时返回 `{ first: 0, last: -1 }`（空区间），调用方据此不渲染任何行。
 */
export function visibleRange({ offsets, scrollTop, viewportH, overscan = 8 }) {
  const total = offsets.length - 1;
  if (total <= 0) return { first: 0, last: -1 };
  const top = Math.max(0, scrollTop);
  const firstVisible = findIndex(offsets, top);
  const first = Math.max(0, firstVisible - overscan);
  const bottom = top + Math.max(0, viewportH);
  let lastVisible = findIndex(offsets, bottom);
  // 视口底部正好落在某行上沿时，那一行其实还没露出来
  while (lastVisible > 0 && offsets[lastVisible] >= bottom) lastVisible--;
  const last = Math.min(total - 1, lastVisible + overscan);
  return { first, last: Math.max(first, last) };
}

/**
 * 锚点：`{ id, dy }` —— 视口顶端那条内容的 id，以及它被卷上去的像素。
 *
 * 为什么用 id 而不是 scrollTop 数值：列表上方插入/删除条目时 scrollTop 要重新
 * 补偿（旧代码那两处 `first.offsetHeight + gap` 就是干这个的），而补偿量一旦算错
 * 就会「跳一下」。用「id + 像素偏移」表示视觉位置，重绘后按 id 找回那一行即可，
 * 不需要任何补偿计算。
 */
export function anchorFrom({ offsets, scrollTop, index, idOf }) {
  const i = Math.max(0, Math.min(index, offsets.length - 2));
  return { id: idOf(i), dy: Math.max(0, scrollTop - offsets[i]) };
}

/** 锚点还原：把 `{id, dy}` 换算回 scrollTop。`index` 是 id 当前所在行。 */
export function scrollForAnchor({ offsets, index, dy }) {
  const i = Math.max(0, Math.min(index, offsets.length - 2));
  return Math.max(0, offsets[i] + Math.max(0, dy));
}

/** 行池大小：可见行数 + 上下 overscan，再留两倍余量给「一屏半」的平移。 */
export function poolSizeFor(viewportH, rowH, overscan = 8) {
  const visible = Math.ceil(viewportH / Math.max(1, rowH)) + 1;
  return Math.max(8, Math.min(120, visible + overscan * 2));
}

// ---------------------------------------------------------------------------
// DOM 部分
// ---------------------------------------------------------------------------

/**
 * 窗口化列表。
 *
 * 数据侧只提供「连续的一段已加载条目」，其余位置画骨架：
 *   `setTotal(n)` → `setWindow(start, items)` → `render()`
 * 需要新数据时通过 `onNeedRange(first, last, 原因)` 通知调用方去取。
 */
export class VirtualList {
  /**
   * @param {object} o
   * @param {HTMLElement} o.scroller        滚动容器（`.history`）
   * @param {(el: HTMLElement, item: object, index: number) => void} o.fillRow  行池节点复用时的填充函数
   * @param {(item: object) => string} o.keyOf  取 id
   * @param {(first: number, last: number, why: string) => void} [o.onNeedRange] 需要数据的回调
   * @param {number} [o.overscan]
   * @param {number} [o.gap]  行间距（CSS 改了就跟着改；默认 8）
   */
  constructor({ scroller, fillRow, keyOf, onNeedRange, onRendered, overscan = 8, gap = 8 }) {
    this.scroller = scroller;
    this.fillRow = fillRow;
    this.keyOf = keyOf;
    this.onNeedRange = onNeedRange || (() => {});
    // 每渲染一帧后通知调用方：图片懒加载要重新登记、失效徽章要补。
    // 放在这里而不是让调用方轮询，是因为「哪些行被复用了」只有列表自己知道。
    this.onRendered = onRendered || (() => {});
    this.overscan = overscan;
    this.gap = gap;

    this.total = 0;
    this.start = 0; // 已加载窗口的起始下标
    this.items = []; // 已加载窗口内的条目（连续）
    this.idToIndex = new Map();
    // 兜底值只影响「第一帧还没量到真实高度」的那一瞬间，量到之后立刻被实测值替换。
    // 取 52 量级是因为**实测文本卡约 50px**（设计稿按 1.5×DPI 估的 78 偏大一倍，
    // 曾导致首页每行按 86px 排布、一屏只看到 4 条）。
    this.classHeights = [52, 52, 54, 54, 112];
    // 下标 → 实测高度。**同时记下这条高度是哪一条内容量的**（knownIds）：
    // 行池节点会被复用，同一个下标在翻页后可能装着另一条内容，那时这条高度就是
    // 别人的，拿来算偏移会让后面的行叠在一起。
    this.known = new Map();
    this.knownIds = new Map(); // 下标 → 当时量到的条目 id
    this.estimate = 52;
    this.calibrated = false; // 是否已经从真实行反推过类高度
    this.offsets = computeOffsets({ total: 0, estimate: this.estimate, gap: this.gap });
    this.pool = [];
    this.free = [];
    this.rendered = new Map(); // 下标 → 行元素（当前这一帧渲染出来的）
    this.observer = null;
    this.lastAnchor = null;

    this.inner = document.createElement("div");
    this.inner.className = "vl-inner";
    scroller.appendChild(this.inner);

    this.resize = null;
  }

  /** 行高类从条目推出来；未加载的下标返回 -1（= 用平均值估）。 */
  rowClass(i) {
    const item = this.items[i - this.start];
    return item ? heightClassOf(item) : -1;
  }

  /**
   * 用**真实渲染出来的行**反推 5 个类高度。
   *
   * 为什么不再用隐藏探针：探针的宽度取决于它的宿主（绝对定位 + 负偏移时宽度是
   * shrink-to-fit，与列表不等宽），`visibility:hidden` 与 `display:none` 的差别、
   * 容器还没进入布局等等，任何一种都会让 `getBoundingClientRect()` 量出一个
   * 与真实卡片无关的数。实测踩过：探针量出 0 / 量出偏大的值，整列按 86px 排布，
   * 一屏只放得下 4 条。
   *
   * 真实行不会有这个问题 —— 它就是用户看到的那一张卡。所以：首帧渲染完量一次，
   * 之后每填一批新行就顺手补测，值变了才重排（并保住视口顶端那一条的位置）。
   */
  calibrate(indices) {
    if (!indices.length) return;
    // 先记住视口顶端的视觉位置：改偏移表会移动内容，得把它还原回去
    const anchor = this.getAnchor();
    let changed = false;
    for (const index of indices) {
      const el = this.rendered.get(index);
      const item = this.items[index - this.start];
      if (!el || !item) continue;
      const h = el.getBoundingClientRect().height;
      if (!(h > 0)) continue; // 窗口隐藏 / 还没布局 → 这一轮量不到，下次再量
      this.known.set(index, h);
      this.knownIds.set(index, this.keyOf(item));
      const c = heightClassOf(item);
      if (Math.abs(this.classHeights[c] - h) > 0.5) {
        this.classHeights[c] = h;
        changed = true;
      }
    }
    if (!changed && this.calibrated) return;
    this.calibrated = true;
    if (!changed) return;
    // 未加载的行没有类高度可用，用「文本/文件/图片」三类的均值当估值
    this.estimate = Math.round((this.classHeights[0] + this.classHeights[3] + this.classHeights[4]) / 3);
    this.rebuild();
    this.inner.style.height = `${Math.round(this.offsets[this.total])}px`;
    if (anchor) this.restoreAnchor(anchor);
  }

  setTotal(total) {
    if (total === this.total) return;
    const prevTotal = this.total;
    this.total = Math.max(0, total | 0);
    // 总高变了：正在看的行要保住（列表上方插入/删除时，scrollTop 不能自己跳）
    const anchor = prevTotal > 0 ? this.getAnchor() : null;
    this.rebuild();
    this.inner.style.height = `${Math.round(this.offsets[this.total])}px`;
    if (anchor && anchor.id) this.restoreAnchor(anchor);
    this.scheduleRender();
  }

  /** 更新已加载窗口（连续片段）。 */
  setWindow(start, items) {
    this.start = Math.max(0, start | 0);
    this.items = items || [];
    this.idToIndex = new Map();
    for (let i = 0; i < this.items.length; i++) {
      const k = this.keyOf(this.items[i]);
      if (k != null) this.idToIndex.set(k, this.start + i);
    }
    // 已加载窗口内的行不再需要「实测覆盖」，让类高度/估算说话（实测值会随渲染补进来）
    for (const idx of [...this.known.keys()]) {
      if (idx < this.start || idx >= this.start + this.items.length) {
        this.known.delete(idx);
        this.knownIds.delete(idx);
      }
    }
    this.rebuild();
    this.inner.style.height = `${Math.round(this.offsets[Math.max(0, this.total)])}px`;
    this.scheduleRender();
  }

  rebuild() {
    // 只采用「量到的就是当前这条内容」的实测高度。
    //
    // 翻页后行池节点会被复用：下标 30 上一轮装的是一条图片卡（高 112），这一轮装的是
    // 一条文本卡（高 50）。若沿用旧值，偏移表会给这行多留 62px（出现大片空隙）；
    // 反过来则是少留，下面的行直接叠上来。id 对不上的一律丢弃，退回类高度。
    const knownOk = new Map();
    for (const [index, h] of this.known) {
      const item = this.items[index - this.start];
      if (!item) continue;
      if (this.knownIds.get(index) !== this.keyOf(item)) continue;
      knownOk.set(index, h);
    }
    this.offsets = computeOffsets({
      total: this.total,
      estimate: this.estimate,
      gap: this.gap,
      known: knownOk,
      classHeights: this.classHeights,
      rowClass: (i) => this.rowClass(i),
    });
  }

  /** 视口内第一条可见行（不含 overscan），用于「滚动到某条」与锚点。 */
  firstVisible() {
    return findIndex(this.offsets, this.scroller.scrollTop);
  }

  getAnchor() {
    const index = this.firstVisible();
    const item = this.items[index - this.start];
    if (!item) {
      // 视口顶端落在未加载区域：只能记 scrollTop（重绘后按偏移表还原）
      return { id: null, dy: this.scroller.scrollTop, index };
    }
    return anchorFrom({
      offsets: this.offsets,
      scrollTop: this.scroller.scrollTop,
      index,
      idOf: (i) => this.keyOf(this.items[i - this.start]),
    });
  }

  restoreAnchor(anchor) {
    if (!anchor) return;
    const at = anchor.id != null ? this.idToIndex.get(anchor.id) : undefined;
    if (at === undefined) {
      // 那条不在已加载窗口里（滚得太远 / 刚切视图）：退回按像素还原，
      // 至少不会跳回顶部
      if (typeof anchor.dy === "number") this.scroller.scrollTop = anchor.dy;
      return;
    }
    this.scroller.scrollTop = scrollForAnchor({
      offsets: this.offsets,
      index: at,
      dy: anchor.dy,
    });
  }

  /** 滚动到某条（按 id）。返回是否找到。 */
  scrollToId(id, align = "start") {
    const at = this.idToIndex.get(id);
    if (at === undefined) return false;
    const top = this.offsets[at];
    const h = this.offsets[at + 1] - top - this.gap;
    this.scroller.scrollTop = align === "center" ? Math.max(0, top - (this.scroller.clientHeight - h) / 2) : top;
    return true;
  }

  /** 视口那一屏需要的数据区间（调用方据此决定要不要取下一页/上一页）。 */
  needRange() {
    const vh = this.scroller.clientHeight || 600;
    return visibleRange({
      offsets: this.offsets,
      scrollTop: this.scroller.scrollTop,
      viewportH: vh,
      overscan: this.overscan * 2, // 提前两屏
    });
  }

  scheduleRender() {
    if (this._raf) return;
    this._raf = requestAnimationFrame(() => {
      this._raf = null;
      this.render();
    });
  }

  /** 渲染当前视口：复用行池节点，只改内容与 transform。 */
  render() {
    if (!this.scroller.contains(this.inner)) return;
    const { first, last } = visibleRange({
      offsets: this.offsets,
      scrollTop: this.scroller.scrollTop,
      viewportH: this.scroller.clientHeight || 600,
      overscan: this.overscan,
    });

    // 先算要哪些行，再「先回收、后分配」—— 顺序反了会让一次跨屏跳转把行池撑到
    // 「旧行 + 新行」那么大，而回收其实就在几行之后。
    const want = Math.max(0, last - first + 1);
    const maxRows = Math.min(want, poolSizeFor(this.scroller.clientHeight || 600, this.estimate, this.overscan));
    const keep = new Set();
    for (let n = 0; n < maxRows; n++) {
      const index = first + n;
      if (index > last || index >= this.total) break;
      keep.add(index);
    }

    for (const [index, el] of this.rendered) {
      if (keep.has(index)) continue;
      el.style.display = "none";
      el.dataset.vlFilled = "";
      this.free.push(el);
      this.rendered.delete(index);
    }

    for (const index of keep) {
      if (this.rendered.has(index)) continue;
      const el = this.free.pop() || this.makeRow();
      this.rendered.set(index, el);
    }

    // 填充：**只填变了的那几行**。
    //
    // 每帧无脑重建所有卡片有两个后果：图片要重新加载（会闪）、鼠标悬停状态被清掉。
    // 用一个 `vlFilled` 标记记住「这一行装的是哪一条」，没变就只更新位置。
    let filled = 0;
    const fresh = []; // 这一帧新填内容的行：它们的真实高度最可信，用来校准
    for (const index of keep) {
      const el = this.rendered.get(index);
      const item = this.items[index - this.start];
      el.style.display = "";
      el.style.transform = `translateY(${Math.round(this.offsets[index])}px)`;
      const want = item ? `i${index}` : `s${index}`;
      if (el.dataset.vlFilled === want) continue;
      el.dataset.vlFilled = want;
      el.dataset.vlIndex = String(index);
      filled++;
      if (item) fresh.push(index);
      if (item) {
        el.classList.remove("vl-skeleton");
        this.fillRow(el, item, index);
      } else {
        // 骨架行：高度按类高度估，先把位置占住，滚动条就不会跳
        el.classList.add("vl-skeleton");
        el.innerHTML = `<div class="vl-skeleton-bar"></div>`;
      }
    }

    // 用刚填好的真实行校准类高度；值变了会重排并还原视口位置
    if (fresh.length) this.calibrate(fresh);

    // 需要数据就喊一声（first/last 都是闭区间）
    this.onNeedRange(first, last, "render");
    // 只有真的换过内容才通知（图片观察、失效徽章都只需在有新节点时重做）
    if (filled > 0) this.onRendered(filled);
  }

  makeRow() {
    const el = document.createElement("div");
    el.className = "vl-row";
    this.inner.appendChild(el);
    // 实测高度校正：只对真的渲染出内容的行有效
    if (typeof ResizeObserver !== "undefined") {
      if (!this.resize) {
        this.resize = new ResizeObserver((entries) => this.onRowResize(entries));
      }
      this.resize.observe(el);
    }
    return el;
  }

  /**
   * 保险丝：已渲染行的实测高度与类高度不一致时更新类表并重排。
   *
   * 正常情况永远不触发（卡片高度只由 kind/subtype 决定）。真触发了说明
   * 「样式契约」被后来人破坏了 —— 这里兜住不至于错位，但根因得在样式层修。
   */
  onRowResize(entries) {
    let changed = false;
    for (const en of entries) {
      const el = en.target;
      const index = Number(el.dataset.vlIndex);
      if (!Number.isFinite(index) || el.classList.contains("vl-skeleton")) continue;
      const h = en.target.getBoundingClientRect().height;
      if (!h) continue;
      const prev = this.known.get(index);
      if (prev === undefined || Math.abs(prev - h) > 0.5) {
        this.known.set(index, h);
        const item = this.items[index - this.start];
        if (item) this.knownIds.set(index, this.keyOf(item));
        if (item) {
          const c = heightClassOf(item);
          if (Math.abs(this.classHeights[c] - h) > 0.5) {
            this.classHeights[c] = h;
            changed = true;
          }
        }
        changed = true;
      }
    }
    if (changed) {
      this.rebuild();
      this.inner.style.height = `${Math.round(this.offsets[this.total])}px`;
      this.scheduleRender();
    }
  }

  destroy() {
    if (this._raf) cancelAnimationFrame(this._raf);
    this.resize?.disconnect();
    this.observer?.disconnect();
    this.inner.remove();
    this.pool = [];
    this.free = [];
    this.rendered.clear();
  }
}
