// 历史查询的前端封装：窗口取数、计数、id 分页、按筛选删除。
//
// 为什么不直接在每个调用点写 `invoke("get_history_window", …)`：
//   1. **锚点自校正**是一套有状态的处理（drift 要平移、missing 要清缓存重取），
//      散在 UI 代码里必然有人漏写其中一支；
//   2. **页缓存**要跟着「筛选指纹（filterKey）」失效，缓存键必须由一处统一拼；
//   3. 这个模块**不 import 任何东西**（`invoke` 由 `createHistoryApi` 注入），
//      所以 `node --test` 里可以直接 import 它、塞一个假后端来测分页与缓存逻辑 ——
//      不用起 WebView，也不用 mock 整个 Tauri。

/**
 * 拼筛选条件。搜索词**原样传**，不在这里 `toLowerCase()`。
 *
 * 为什么：后端已经做了「trim + 折叠 ASCII 大小写」，前端再折一遍会让
 * `filterKey` 因为大小写差异反复变化（同一个词，前一次传 "Git"、后一次传 "git"，
 * 后端归一后结果相同，但指纹不同）→ 页缓存被无谓地整批丢掉、列表闪一下。
 */
export function buildFilter({ view = "history", query = "", kindHint = null, subtypeHint = null } = {}) {
  return {
    view,
    query: query == null ? "" : String(query),
    kindHint: kindHint == null ? null : String(kindHint),
    subtypeHint: subtypeHint == null ? null : String(subtypeHint),
  };
}

/** 页缓存键：筛选指纹 + 起始下标。0 页单独记（`getTotal` 用）。 */
export function pageKey(filterKey, offset) {
  return `${filterKey}:${offset}`;
}

/**
 * 建一个查询客户端。
 *
 * @param {(cmd: string, args?: object) => Promise<any>} invoke  Tauri 的 invoke（注入，便于单测）
 * @param {{cachePages?: number}} [opts]
 */
export function createHistoryApi(invoke, { cachePages = 10 } = {}) {
  // filterKey → Map<offset, page>；key 是字符串，`Map` 保持插入顺序，正好当 LRU 用
  const cache = new Map();

  function pageFor(filterKey) {
    let page = cache.get(filterKey);
    if (!page) {
      page = new Map();
      cache.set(filterKey, page);
      // 超上限先丢最久没被碰过的那个筛选（切换视图/搜索词会造新的指纹）
      while (cache.size > cachePages) {
        cache.delete(cache.keys().next().value);
      }
    } else {
      // 命中就移到队尾，让 LRU 的「近用」语义成立
      cache.delete(filterKey);
      cache.set(filterKey, page);
    }
    return page;
  }

  /** 丢掉某个筛选（或全部）的页缓存。搜索词/视图一变就必须调用。 */
  function invalidate(filterKey) {
    if (filterKey === undefined) cache.clear();
    else cache.delete(filterKey);
  }

  async function callWindow({ filter, offset, limit, anchorId = null, wantTotal = false }) {
    return invoke("get_history_window", {
      filter,
      offset,
      limit,
      anchorId,
      wantTotal,
    });
  }

  /**
   * 取一页。返回 `{ items, offset, requested, anchor, total, pinnedCount, filterKey, epoch }`。
   *
   * `anchor.state` 的三条路都在这里收口：
   *   - `ok`      → 原样用；
   *   - `drift`   → 后端已经把这一页按锚点的真实位置取好了，调用方拿 `offset` 当准；
   *   - `missing` → 锚点条目没了：清掉这一页缓存、按**请求的** offset 重取一次。
   */
  async function fetchWindow({ filter, offset, limit, anchorId = null, wantTotal = false }) {
    let res = await callWindow({ filter, offset, limit, anchorId, wantTotal });
    if (res.anchor && res.anchor.state === "missing") {
      // 锚点消失 = 这一页的内容已经不可信（删除、或不再匹配当前筛选）
      invalidate(res.filterKey);
      res = await callWindow({ filter, offset, limit, anchorId: null, wantTotal });
    }
    const page = pageFor(res.filterKey);
    page.set(res.offset, { items: res.items, offset: res.offset, total: res.total, pinnedCount: res.pinnedCount });
    // 只留最近 10 页：窗口化渲染只需要当前视口附近的那几页
    while (page.size > cachePages) {
      page.delete(page.keys().next().value);
    }
    return res;
  }

  /** 过滤后的条数（用于状态栏与「全选当前视图」）。 */
  async function fetchCounts(filter) {
    return invoke("history_count", { filter });
  }

  /** 取一段 id（批量操作、文件失效检测用），不拉全量。 */
  async function fetchIds(filter, offset, limit) {
    return invoke("list_ids", { filter, offset, limit });
  }

  /**
   * 「清空当前视图」：把筛选条件交给后端，由它自己算 id 并删。
   *
   * `includePinned` / `includeFavorites` 默认 false = **保留**置顶与收藏，与
   * 「清空当前视图」弹窗里两个勾选项的默认值一致。保护逻辑必须在后端做：
   * 前端先筛一遍再传 id，就等于又回到「拉全量 id」的老路。
   */
  async function deleteByFilter(
    filter,
    deleteLocalFiles = false,
    { includePinned = false, includeFavorites = false } = {}
  ) {
    const res = await invoke("delete_by_filter", {
      filter,
      deleteLocalFiles,
      includePinned,
      includeFavorites,
    });
    invalidate(); // 删完之后所有页都可能变了
    return res;
  }

  /** 直接从缓存里取一页（渲染时用，避免同一页重复走 IPC）。 */
  function peek(filterKey, offset) {
    return cache.get(filterKey)?.get(offset) ?? null;
  }

  return {
    buildFilter,
    fetchWindow,
    fetchCounts,
    fetchIds,
    deleteByFilter,
    invalidate,
    peek,
    _cacheSize: () => cache.size,
  };
}
