// 删除前「要不要连本地文件一起处理」的决策用例。
//
// 这段逻辑静默错过三次，每次都是「确认框没弹、文件没删、界面还报已删除」：
//   1. 索引不完整 → 判成「没有文件条目」（F1）
//   2. 布尔返回值把「用户取消」和「只删记录」混成一个 false（取消被吞）
//   3. 探针失败被当成「没有文件」
// 所以把它抽成纯函数，用表驱动把每个分支钉死，不再依赖人肉实测。
import test from "node:test";
import assert from "node:assert/strict";

import { decidePurgeAction } from "../src/purge-decision.js";

test("设置没开：永远只删记录，不碰文件", () => {
  const r = decidePurgeAction({
    enabled: false,
    idsCount: 3,
    busy: false,
    probe: { files: 3, alive: 3 },
  });
  assert.deepEqual(r, { action: "records", reason: "disabled" });
});

test("没选任何东西：不弹窗、不探针", () => {
  const r = decidePurgeAction({ enabled: true, idsCount: 0, busy: false, probe: null });
  assert.deepEqual(r, { action: "records", reason: "empty" });
});

test("已有确认框在等用户：后来的调用中止，不抢 purgeResolve 槽", () => {
  const r = decidePurgeAction({ enabled: true, idsCount: 2, busy: true, probe: null });
  assert.deepEqual(r, { action: "abort", reason: "busy" });
});

test("还没探针：先让调用方去问后端", () => {
  const r = decidePurgeAction({ enabled: true, idsCount: 2, busy: false, probe: null });
  assert.deepEqual(r, { action: "probe", reason: "need-probe" });
});

test("探针说这批 id 里没有文件条目：不弹确认框", () => {
  const r = decidePurgeAction({
    enabled: true,
    idsCount: 2,
    busy: false,
    probe: { files: 0, alive: 0 },
  });
  assert.deepEqual(r, { action: "records", reason: "no-files" });
});

test("有文件条目但文件全失效：不弹窗（问了也只能报「未能处理」）", () => {
  const r = decidePurgeAction({
    enabled: true,
    idsCount: 2,
    busy: false,
    probe: { files: 2, alive: 0 },
  });
  assert.deepEqual(r, { action: "records", reason: "all-missing" });
});

test("部分文件还在：仍要问用户，按实际存活数说明范围", () => {
  const r = decidePurgeAction({
    enabled: true,
    idsCount: 5,
    busy: false,
    probe: { files: 5, alive: 2 },
  });
  assert.deepEqual(r, { action: "ask", reason: "ok" });
});

test("全部文件都在：弹窗", () => {
  const r = decidePurgeAction({
    enabled: true,
    idsCount: 1,
    busy: false,
    probe: { files: 1, alive: 1 },
  });
  assert.deepEqual(r, { action: "ask", reason: "ok" });
});
