// 查询封装的用例：筛选拼装、锚点分支、页缓存与失效。
//
// 用注入的假 `invoke` 驱动 —— 这正是 `createHistoryApi(invoke)` 这么设计的原因：
// 分页与缓存逻辑（drift 平移、missing 重取、LRU 淘汰）全都能在 node 里测，
// 不必起 WebView。
import test from "node:test";
import assert from "node:assert/strict";

import { buildFilter, pageKey, createHistoryApi } from "../src/history-api.js";

/** 造一个假后端：按调用顺序记录，并按脚本返回。 */
function fakeBackend(script) {
  const calls = [];
  const invoke = async (cmd, args) => {
    calls.push({ cmd, args });
    const handler = script[cmd];
    if (!handler) throw new Error(`未预期的命令：${cmd}`);
    return handler(args, calls.length);
  };
  return { invoke, calls };
}

test("buildFilter：搜索词原样传，不在这里折大小写", () => {
  assert.deepEqual(buildFilter({ view: "text", query: "Git" }), {
    view: "text",
    query: "Git",
    kindHint: null,
    subtypeHint: null,
  });
  // 缺省值：view 回 history、query 回空串
  assert.deepEqual(buildFilter(), { view: "history", query: "", kindHint: null, subtypeHint: null });
  assert.deepEqual(buildFilter({ view: "files", kindHint: "files" }).kindHint, "files");
});

test("pageKey：按筛选指纹 + 起止下标拼键", () => {
  assert.equal(pageKey(42, 0), "42:0");
  assert.equal(pageKey(42, 200), "42:200");
});

const onePage = (offset) => ({
  items: [{ id: `i${offset}` }, { id: `i${offset + 1}` }],
  offset,
  requested: offset,
  anchor: { state: "ok" },
  total: 1000,
  pinnedCount: 3,
  filterKey: 7,
  epoch: 1,
});

test("fetchWindow：正常页直接返回并写缓存", async () => {
  const { invoke, calls } = fakeBackend({
    get_history_window: (args) => {
      assert.equal(args.offset, 200);
      assert.equal(args.wantTotal, false);
      return onePage(200);
    },
  });
  const api = createHistoryApi(invoke);
  const res = await api.fetchWindow({ filter: buildFilter(), offset: 200, limit: 200 });
  assert.equal(res.offset, 200);
  assert.equal(calls.length, 1);
  assert.ok(api.peek(7, 200), "应写进页缓存");
});

test("fetchWindow：drift 用后端校正后的 offset（不重取）", async () => {
  const { invoke, calls } = fakeBackend({
    get_history_window: () => ({
      ...onePage(260),
      requested: 200,
      anchor: { state: "drift", offset: 260 },
    }),
  });
  const api = createHistoryApi(invoke);
  const res = await api.fetchWindow({ filter: buildFilter(), offset: 200, limit: 200, anchorId: "i200" });
  assert.equal(calls.length, 1, "drift 不该触发第二次请求");
  assert.equal(res.offset, 260);
  assert.equal(res.requested, 200);
});

test("fetchWindow：missing 清缓存并按请求 offset 重取一次", async () => {
  const { invoke, calls } = fakeBackend({
    get_history_window: (args, n) => {
      if (n === 1) return { ...onePage(200), anchor: { state: "missing" } };
      // 第二次：anchorId 必须为 null，offset 用请求值
      assert.equal(args.anchorId, null);
      assert.equal(args.offset, 200);
      return { ...onePage(200), anchor: { state: "ok" } };
    },
  });
  const api = createHistoryApi(invoke);
  const res = await api.fetchWindow({ filter: buildFilter(), offset: 200, limit: 200, anchorId: "gone" });
  assert.equal(calls.length, 2, "missing 必须重取一次");
  assert.equal(res.anchor.state, "ok");
  assert.equal(res.offset, 200);
});

test("页缓存：只保留最近 10 页", async () => {
  const { invoke } = fakeBackend({
    get_history_window: (args) => ({ ...onePage(args.offset), filterKey: args.filter.view === "history" ? 1 : 2 }),
  });
  const api = createHistoryApi(invoke, { cachePages: 10 });
  for (let i = 0; i < 15; i++) {
    await api.fetchWindow({ filter: buildFilter(), offset: i * 200, limit: 200 });
  }
  // 最早那几页应被淘汰
  assert.equal(api.peek(1, 0), null, "第 0 页应被 LRU 淘汰");
  assert.ok(api.peek(1, 14 * 200), "最近的页还在");
});

test("页缓存：不同 filterKey 分开存，切换筛选不会串页", async () => {
  const { invoke } = fakeBackend({
    get_history_window: (args) => ({
      ...onePage(args.offset),
      filterKey: args.filter.view === "favorites" ? 99 : 7,
    }),
  });
  const api = createHistoryApi(invoke);
  await api.fetchWindow({ filter: buildFilter({ view: "history" }), offset: 0, limit: 200 });
  await api.fetchWindow({ filter: buildFilter({ view: "favorites" }), offset: 0, limit: 200 });
  assert.ok(api.peek(7, 0));
  assert.ok(api.peek(99, 0));
});

test("fetchCounts / fetchIds / deleteByFilter 透传并失效缓存", async () => {
  const { invoke, calls } = fakeBackend({
    history_count: () => 123,
    list_ids: (args) => ({ ids: ["a", "b"], offset: args.offset }),
    delete_by_filter: () => ({ deleted: 2, note: null }),
    get_history_window: () => onePage(0),
  });
  const api = createHistoryApi(invoke);
  await api.fetchWindow({ filter: buildFilter(), offset: 0, limit: 200 });
  assert.ok(api.peek(7, 0));

  assert.equal(await api.fetchCounts(buildFilter()), 123);
  const page = await api.fetchIds(buildFilter(), 40, 20);
  assert.deepEqual(page.ids, ["a", "b"]);
  assert.equal(page.offset, 40);

  const res = await api.deleteByFilter(buildFilter({ view: "text" }), true);
  assert.equal(res.deleted, 2);
  assert.equal(api.peek(7, 0), null, "删除后页缓存必须失效");
  assert.equal(calls.at(-1).args.deleteLocalFiles, true);
});

test("invalidate：清单个筛选与清全部", async () => {
  const { invoke } = fakeBackend({ get_history_window: () => onePage(0) });
  const api = createHistoryApi(invoke);
  await api.fetchWindow({ filter: buildFilter(), offset: 0, limit: 200 });
  api.invalidate(7);
  assert.equal(api.peek(7, 0), null);
  await api.fetchWindow({ filter: buildFilter(), offset: 0, limit: 200 });
  api.invalidate();
  assert.equal(api.peek(7, 0), null);
});
