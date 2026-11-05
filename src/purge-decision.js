// 删除前「要不要处理本地文件」的决策，抽成纯函数。
//
// 为什么单独一个文件：这段逻辑历史上错过三次 —— 索引不全判成「没有文件条目」、
// 布尔返回值让「取消」退化成「只删记录」、探针失败被当成「没有文件」。
// 三次都是**静默**的：确认框不弹、文件不删，界面还报「已删除 N 项」。
// 靠人肉实测抓不住，所以把决策抽出来，用 node --test 直接钉住每个分支。

/**
 * @param {object} input
 * @param {boolean} input.enabled  设置里「删除条目时同时处理本地文件」是否开启
 * @param {number}  input.idsCount 本次要删的条目数
 * @param {boolean} input.busy     是否已有一次确认框在等用户（重入门闩）
 * @param {{files:number, alive:number}} input.probe 后端探针：几个文件条目 / 几个文件还在
 * @returns {{action: "purge"|"records"|"abort"|"probe"|"ask", reason: string}}
 *   `purge` 连文件一起删；`records` 只删记录；`abort` 整个操作中止；
 *   `probe` 需要先问后端「这批 id 里几个文件条目、几个还真的有文件」；
 *   `ask` 需要弹确认框，由调用方等用户选择。
 */
export function decidePurgeAction({ enabled, idsCount, busy, probe }) {
  // 设置没开：文件一律不碰。这是默认值，也是最该保守的分支。
  if (!enabled) return { action: "records", reason: "disabled" };
  if (idsCount === 0) return { action: "records", reason: "empty" };
  // 已经在问用户了：后来的调用直接中止，不能去抢 `purgeResolve` 那个单槽。
  if (busy) return { action: "abort", reason: "busy" };
  // 还没探：调用方据此决定要不要发那次 IPC。
  if (!probe) return { action: "probe", reason: "need-probe" };
  // 探针按 id 回查真源，说没有文件条目就是没有。
  if (probe.files === 0) return { action: "records", reason: "no-files" };
  // 有文件条目，但文件全都不在了：问「要不要一起删」没有意义，
  // 用户确认之后只会收到「未能处理」的提示。
  if (probe.alive === 0) return { action: "records", reason: "all-missing" };
  return { action: "ask", reason: "ok" };
}
