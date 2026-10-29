// 虚拟列表的纯函数用例（`node --test test/`）。
//
// 只测纯函数：DOM 部分的行为（行池复用、探针实测）靠人工验收 —— 在没有浏览器的
// 环境下 mock 一个 getBoundingClientRect 只会把「我以为浏览器怎么算」写成断言，
// 那种测试只会给人虚假的安全感。
import test from "node:test";
import assert from "node:assert/strict";

import {
  CLASS_COUNT,
  heightClassOf,
  computeOffsets,
  findIndex,
  visibleRange,
  anchorFrom,
  scrollForAnchor,
  poolSizeFor,
} from "../src/virtual-list.js";

test("行高类：kind 优先，其次 subtype", () => {
  assert.equal(heightClassOf({ kind: "text" }), 0);
  assert.equal(heightClassOf({ kind: "text", subtype: "code" }), 1);
  assert.equal(heightClassOf({ kind: "text", subtype: "markdown" }), 2);
  assert.equal(heightClassOf({ kind: "text", subtype: "link" }), 0); // link 不改变行高
  assert.equal(heightClassOf({ kind: "files" }), 3);
  assert.equal(heightClassOf({ kind: "image" }), 4);
  assert.equal(heightClassOf(null), 0);
  assert.equal(CLASS_COUNT, 5);
});

test("偏移表：单调、间距只加在行之间、末行后不留白", () => {
  const o = computeOffsets({ total: 3, estimate: 80, gap: 8 });
  assert.deepEqual([...o], [0, 88, 176, 256]);
  // 末行高度 80，总高 = 176 + 80 = 256（没有再 +8）
  assert.equal(o[3], 256);
  for (let i = 1; i < o.length; i++) assert.ok(o[i] >= o[i - 1]);
});

test("偏移表：类高度与实测值优先于平均值", () => {
  const o = computeOffsets({
    total: 4,
    estimate: 100,
    gap: 0,
    classHeights: [50, 60, 70, 80, 90],
    rowClass: (i) => (i === 0 ? 4 : 0), // 第 0 行是图片（90），其余是文本（50）
    known: new Map([[1, 33]]), // 第 1 行实测 33
  });
  assert.deepEqual([...o], [0, 90, 123, 173, 223]);
});

test("二分定位：边界不差一行", () => {
  const o = computeOffsets({ total: 5, estimate: 100, gap: 0 });
  assert.deepEqual([...o], [0, 100, 200, 300, 400, 500]);
  assert.equal(findIndex(o, 0), 0);
  assert.equal(findIndex(o, 99), 0);
  assert.equal(findIndex(o, 100), 1); // 正好等于上沿 → 归下一行（不是上一行）
  assert.equal(findIndex(o, 150), 1);
  assert.equal(findIndex(o, 400), 4);
  assert.equal(findIndex(o, 99999), 4); // 夹在最后一行
  assert.equal(findIndex(computeOffsets({ total: 0, estimate: 100, gap: 0 }), 0), 0);
});

test("可见区间：overscan 生效、空列表返回空区间、底部对齐不多渲染一行", () => {
  const o = computeOffsets({ total: 100, estimate: 100, gap: 0 });
  const r = visibleRange({ offsets: o, scrollTop: 500, viewportH: 300, overscan: 2 });
  assert.equal(r.first, 3); // 首可见 5，减 overscan
  assert.equal(r.last, 9); // 底 800 → 第 8 行，加 overscan

  const empty = visibleRange({ offsets: computeOffsets({ total: 0, estimate: 100, gap: 0 }), scrollTop: 0, viewportH: 300 });
  assert.deepEqual(empty, { first: 0, last: -1 });

  // 底部正好落在第 3 行上沿（300）→ 第 3 行还没露出来
  const edge = visibleRange({ offsets: o, scrollTop: 0, viewportH: 300, overscan: 0 });
  assert.equal(edge.last, 2);
});

test("锚点往返：上方插入内容后视觉位置不变", () => {
  // 40 行 × 100px
  const before = computeOffsets({ total: 40, estimate: 100, gap: 0 });
  const scrollTop = 1234; // 停在第 12 行中间
  const index = findIndex(before, scrollTop);
  assert.equal(index, 12);
  const ids = Array.from({ length: 40 }, (_, i) => `id${i}`);
  const anchor = anchorFrom({ offsets: before, scrollTop, index, idOf: (i) => ids[i] });
  assert.deepEqual(anchor, { id: "id12", dy: 34 });

  // 上方插入 3 行 → 同一个 id 现在是第 15 行
  const after = computeOffsets({ total: 43, estimate: 100, gap: 0 });
  const newScroll = scrollForAnchor({ offsets: after, index: 15, dy: anchor.dy });
  assert.equal(newScroll, 1534); // 1500 + 34
  assert.equal(findIndex(after, newScroll), 15, "还原后视口顶端仍是同一条");
});

test("行池大小：随视口变化但夹在合理区间", () => {
  assert.ok(poolSizeFor(600, 80) >= 8);
  assert.ok(poolSizeFor(600, 80) <= 120);
  assert.ok(poolSizeFor(4000, 40) <= 120, "窗口再大也不无限建节点");
  assert.equal(poolSizeFor(100, 100, 0), 8, "最小 8 行");
});
