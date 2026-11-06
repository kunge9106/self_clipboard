// 更新按钮状态的契约用例：安装包被手动删除后，失败路径必须重拉状态。
//
// 这两处曾经只调 renderUpdatePanels()，前端缓存里的 downloadedPath 不清空，
// 按钮停在「立即安装」，提示却让用户「重新下载」—— 界面上根本没有那个按钮。
// 另一种是下载态按钮 142px 宽 + overflow:hidden，长文案两端被裁。
//
// 这段逻辑内联在 main.js 且依赖 invoke / DOM，不便在 node 里直接跑行为，
// 所以用源码契约把三个不变量钉死，防止以后有人把长文案或旧写法改回来。
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("../src/main.js", import.meta.url), "utf8");

test("两条安装失败路径都重新读取更新状态", () => {
  const calls = src.split("await refreshUpdateState();").length - 1;
  assert.equal(calls, 2, "startDownload 与 installDownloaded 的失败分支都要刷新");
});

test("状态刷新只读缓存、不联网，且失败时有兜底", () => {
  const from = src.indexOf("async function refreshUpdateState");
  const to = src.indexOf('/** 点「下载最新版」', from);
  assert.ok(from > 0 && to > from, "找不到刷新函数");
  const body = src.slice(from, to);
  assert.match(body, /invoke\("update_state"\)/, "必须读本地缓存");
  assert.doesNotMatch(body, /check_update/, "失败路径不能触发联网检查");
  assert.match(body, /downloadedPath: ""/, "后端不可用时要本地清空，避免留下点不动的按钮");
});

test("下载态文案不超过按钮可容纳的字数", () => {
  const m = src.match(/const INSTALL_START_LABEL = "([^"]+)"/);
  assert.ok(m, "找不到共用的文案常量");
  const label = m[1];
  assert.ok([...label].length <= 10, `「${label}」超过 10 字，142px 按钮会裁字`);
  // 数标识符而不是字面量：调用处写的是常量名，注释里也可能提到这句文案
  assert.equal(src.split("INSTALL_START_LABEL").length - 1, 3, "常量定义一次、两处调用各一次");
  assert.doesNotMatch(
    src,
    /setDownloadDone\("下载完成|setDownloadDone\("正在启动安装程序/,
    "不允许再直接写长文案"
  );
});
