// 用一层极小的 DOM 替身跑 VirtualList 的真实渲染逻辑。
//
// 为什么不引 jsdom：仓库纪律是「不新增前端依赖」，而这层替身只有百来行。
// 它测不了「看起来对不对」，但能测到真正容易错的东西：行池复用、回收、骨架行、
// 锚点还原、只填变化行（这一条错了会导致图片反复重载、滚动时闪烁）。
import test from "node:test";
import assert from "node:assert/strict";

// ── 最小 DOM 替身 ───────────────────────────────────────────────────────
class CL {
  constructor(el) {
    this.el = el;
  }
  _set() {
    return new Set((this.el.className || "").split(/\s+/).filter(Boolean));
  }
  add(...c) {
    const s = this._set();
    for (const x of c) s.add(x);
    this.el.className = [...s].join(" ");
  }
  remove(...c) {
    const s = this._set();
    for (const x of c) s.delete(x);
    this.el.className = [...s].join(" ");
  }
  contains(c) {
    return this._set().has(c);
  }
  toggle(c, on) {
    if (on === undefined ? !this.contains(c) : on) this.add(c);
    else this.remove(c);
  }
}

class El {
  constructor(tag = "div") {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.parent = null;
    this.className = "";
    this.classList = new CL(this);
    this.dataset = {};
    this.style = { cssText: "" };
    this._html = "";
    this.h = 80; // 假的布局高度
  }
  set innerHTML(v) {
    this._html = v;
    this.children = [];
  }
  get innerHTML() {
    return this._html;
  }
  appendChild(c) {
    c.parent = this;
    this.children.push(c);
    return c;
  }
  remove() {
    if (!this.parent) return;
    const i = this.parent.children.indexOf(this);
    if (i >= 0) this.parent.children.splice(i, 1);
    this.parent = null;
  }
  contains(n) {
    if (n === this) return true;
    return this.children.some((c) => c.contains(n));
  }
  querySelectorAll() {
    return [];
  }
  querySelector() {
    return null;
  }
  getBoundingClientRect() {
    return { height: this.h, width: 100, top: 0, bottom: this.h, left: 0, right: 100 };
  }
}

class Scroller extends El {
  constructor() {
    super("div");
    this.clientHeight = 600;
    this.scrollTop = 0;
    this.scrollHeight = 0;
  }
}

globalThis.document = {
  createElement: (tag) => new El(tag),
};
globalThis.requestAnimationFrame = (fn) => {
  fn();
  return 1;
};
globalThis.cancelAnimationFrame = () => {};

const { VirtualList, heightClassOf, imgPending, findIndex: findIndexFor } = await import("../src/virtual-list.js");

/** 造一个列表：heightFor 决定每一行渲染后的高度（模拟 CSS 的真实结果）。 */
function makeList({ total, itemsPerPage = 200, viewport = 600, heightFor }) {
  const scroller = new Scroller();
  scroller.clientHeight = viewport;
  const need = [];
  let fillCalls = 0;
  const vl = new VirtualList({
    scroller,
    fillRow: (el, item) => {
      fillCalls++;
      el.h = heightFor ? heightFor(item) : 80;
    },
    keyOf: (item) => item.id,
    onNeedRange: (first, last) => need.push([first, last]),
  });
  const items = Array.from({ length: total }, (_, i) => ({
    id: `id${i}`,
    kind: i % 10 === 0 ? "image" : "text",
    subtype: null,
  }));
  vl.setTotal(total);
  vl.setWindow(0, items.slice(0, itemsPerPage));
  vl.render();
  return { vl, scroller, items, need, fillCalls: () => fillCalls };
}

test("渲染只创建一屏多的行，不是全部", () => {
  const { vl } = makeList({ total: 100_000 });
  const rows = vl.inner.children.length;
  assert.ok(rows > 0, "至少要渲染出几行");
  assert.ok(rows <= 40, `行池应在一屏量级，实际 ${rows}`);
});

test("滚动后行池复用：节点总数收敛到一屏量级，不随滚动增长", () => {
  const { vl, scroller } = makeList({ total: 100_000 });
  for (const y of [2000, 8000, 20000, 60000]) {
    scroller.scrollTop = y;
    vl.render();
  }
  const settled = vl.inner.children.length;
  assert.ok(settled <= 40, `行池应收敛到一屏量级，实际 ${settled}`);
  // 再滚十几屏，节点数一分不涨
  for (let k = 0; k < 12; k++) {
    scroller.scrollTop = 60_000 + k * 1200;
    vl.render();
  }
  assert.equal(vl.inner.children.length, settled, "继续滚动不该再新建节点");
});

test("同一窗口内重复渲染不会重填（避免图片重复加载/闪烁）", () => {
  const { vl, fillCalls } = makeList({ total: 10_000 });
  const first = fillCalls();
  vl.render();
  vl.render();
  assert.equal(fillCalls(), first, "没有换内容就不该重新填行");
});

test("滚到未加载区域时画骨架行而不是空白", () => {
  const { vl, scroller } = makeList({ total: 10_000, itemsPerPage: 200 });
  scroller.scrollTop = 500 * 80; // 远超已加载的 200 条
  vl.render();
  const skeletons = vl.inner.children.filter((el) => el.classList.contains("vl-skeleton"));
  assert.ok(skeletons.length > 0, "未加载区域应有骨架行");
});

test("锚点还原：上方插入内容后，视口顶端仍是同一条", () => {
  const { vl, scroller, items } = makeList({ total: 1000 });
  // 行高不等（每 10 行一张图），所以「第几行」必须按偏移表算
  scroller.scrollTop = 1000 + 5;
  const anchor = vl.getAnchor();
  const expected = Math.max(0, Math.min(999, findIndexFor(vl.offsets, scroller.scrollTop)));
  assert.equal(anchor.id, `id${expected}`, `锚点应是第 ${expected} 行`);
  assert.equal(anchor.dy, scroller.scrollTop - vl.offsets[expected]);
  const beforeIndex = expected;

  // 上方插入 3 条 → 同一个 id 现在在第 15 行
  const grown = [{ id: "n0" }, { id: "n1" }, { id: "n2" }, ...items];
  vl.setTotal(1003);
  vl.setWindow(0, grown.slice(0, 203));
  vl.restoreAnchor(anchor);
  assert.equal(vl.firstVisible(), beforeIndex + 3, "还原后视口顶端仍是同一条");
});

test("删除当前行后：视口顶端落到下一条，不跳到顶上", () => {
  const { vl, scroller, items } = makeList({ total: 1000 });
  scroller.scrollTop = 80 * 20;
  const before = vl.getAnchor();
  // 删掉锚点那条
  const rest = items.filter((i) => i.id !== before.id);
  vl.setTotal(999);
  vl.setWindow(0, rest.slice(0, 199));
  vl.restoreAnchor(before); // 找不到 id → 退回像素还原
  assert.ok(scroller.scrollTop > 0, "不该被弹回顶部");
});

test("行高类：图片行比文本行高，偏移表跟着变", () => {
  const { vl } = makeList({ total: 100, heightFor: (item) => heightClassOf(item) === 4 ? 100 : 78 });
  const rows = vl.inner.children.filter((el) => !el.classList.contains("vl-skeleton"));
  const imgRow = rows.find((el) => el.classList.contains("vl-row") && el.dataset.vlIndex === "0");
  assert.ok(imgRow, "第 0 行应已渲染");
  assert.equal(imgRow.style.transform, "translateY(0px)");
});

test("按真实行高校准：首帧之后偏移表用实测值，不再用兜底常量", () => {
  // 兜底常量是 52；真实卡片 50（文本类）——差 2px 也要被纠回来
  const { vl } = makeList({ total: 100, heightFor: () => 50 });
  assert.equal(vl.classHeights[0], 50, "类高度应被实测值覆盖");
  assert.equal(vl.offsets[1], 58, "偏移步长应为 实高 50 + 间距 8");
  assert.equal(vl.offsets[2], 116);
});

test("图片行与文本行各自取自己的实测高度", () => {
  const { vl } = makeList({
    total: 100,
    heightFor: (item) => (heightClassOf(item) === 4 ? 120 : 50),
  });
  assert.equal(vl.classHeights[0], 50);
  assert.equal(vl.classHeights[4], 120);
  // 第 0 行是图片（i%10===0）
  assert.equal(vl.offsets[1], 128, "图片行之后应多出 120 + 8");
});

test("空列表不崩", () => {
  const { vl } = makeList({ total: 0, itemsPerPage: 0 });
  assert.doesNotThrow(() => vl.render());
  assert.equal(vl.inner.children.length, 0);
});

// ── 图片未解码时不能参与标定 ──────────────────────────────────────────────
//
// 实测反馈：「复制新内容时，上边那条置顶会闪一下」。带图的那张卡在缩略图赋 src 之前
// 高度是「没有图」的高度，曾被当成图片类高度写进类表 → 图一解码完整列先压紧再弹开。

test("imgPending 只看当前这条图的解码状态", () => {
  assert.equal(imgPending({ querySelector: () => null }), false, "没有缩略图的行永远算就绪");
  const pending = { complete: false, naturalHeight: 0 };
  assert.equal(imgPending({ querySelector: () => pending }), true, "还没开始解码");
  assert.equal(
    imgPending({ querySelector: () => ({ complete: true, naturalHeight: 0 }) }),
    true,
    "解码失败（有 complete 但没尺寸）也算没就绪"
  );
  assert.equal(
    imgPending({ querySelector: () => ({ complete: true, naturalHeight: 120 }) }),
    false
  );
});

test("缩略图没解码完就不参与标定，不污染图片类高度", () => {
  const { vl } = makeList({
    total: 100,
    heightFor: (item) => (heightClassOf(item) === 4 ? 120 : 50),
  });
  const before = vl.classHeights[4];
  assert.equal(before, 120, "首帧按真实图卡高度标定过");

  const row = vl.rendered.get(0); // 第 0 行是图片（i % 10 === 0）
  row.h = 40; // 模拟「图还没加载，这一行只有文字那么高」
  row.querySelector = () => ({ complete: false, naturalHeight: 0 });

  vl.calibrate([0]);
  assert.equal(vl.classHeights[4], before, "未解码的图不该把图片类高度量矮");
  assert.equal(vl.known.get(0), 120, "该行仍用上一次的可信实测值");
});

test("标定改了偏移表后同帧重排，不等下一帧", () => {
  const { vl } = makeList({ total: 50, heightFor: () => 50 });
  const row = vl.rendered.get(5);
  row.h = 200; // 这一行突然变高（例如内容变化）
  vl.calibrate([5]);
  assert.equal(
    row.style.transform,
    `translateY(${Math.round(vl.offsets[5])}px)`,
    "标定后应立即按新偏移表定位，否则会先按旧位置画一帧再挪"
  );
});

// ── 类高度表跨实例继承 ────────────────────────────────────────────────────
//
// 每次复制都新建列表。若类高度表从硬编码默认值起手，首帧就按错误高度排版：
// 图片类默认 112、真实约 74 → 新内容先出现在离置顶那条几十像素的位置，等标定完
// 再挪过去。用户看到的是「新内容从置顶里钻出来再滑下去」。

test("类高度表从外部继承，且是拷贝（不与调用方共享数组）", () => {
  const src = [40, 42, 44, 46, 70];
  const vl = new VirtualList({
    scroller: new Scroller(),
    fillRow: () => {},
    keyOf: (item) => item.id,
    classHeights: src,
  });
  assert.deepEqual(vl.classHeights, [40, 42, 44, 46, 70], "构造时就该采用继承来的表");
  assert.equal(vl.estimate, Math.round((40 + 46 + 70) / 3), "估值跟着继承的表算");

  vl.classHeights[0] = 999;
  assert.equal(src[0], 40, "必须拷贝，否则两个实例会互相改");
});

test("下标相关的度量不随类高度表一起继承", () => {
  const vl = new VirtualList({
    scroller: new Scroller(),
    fillRow: () => {},
    keyOf: (item) => item.id,
    classHeights: [40, 42, 44, 46, 70],
  });
  assert.equal(vl.known.size, 0, "known 是「下标 → 高度」，换数据后下标含义会平移");
  assert.equal(vl.knownIds.size, 0);
  assert.equal(vl.rendered.size, 0, "行池与映射必须随实例新建");
  assert.equal(vl.calibrated, false, "新实例仍要重新标定一次");
});

test("继承了类高度后，首帧偏移表就按真实高度排（不再先挤后弹）", () => {
  const scroller = new Scroller();
  scroller.clientHeight = 600;
  const vl = new VirtualList({
    scroller,
    fillRow: () => {},
    keyOf: (item) => item.id,
    classHeights: [50, 52, 54, 56, 74], // 上一张列表量到的真实值
  });
  const items = Array.from({ length: 10 }, (_, i) => ({ id: `a${i}`, kind: "image", subtype: null }));
  vl.setTotal(10);
  vl.setWindow(0, items);
  assert.equal(vl.offsets[1], 74 + 8, "首行是图片卡时，下一行应直接落在 74+gap");
});
