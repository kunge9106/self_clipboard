// 更新安装链路的契约用例。
//
// 这一块静默错过三次：包被删后按钮不刷新、下载态长文案被裁、
// 下载完自动弹 UAC 且点「否」被报成「下载失败」。逻辑内联在 main.js /
// about.rs 且依赖 invoke / Windows FFI，不便在 node 里跑行为，所以用源码
// 契约把不变量钉死，防止以后有人把旧写法改回来。
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("../src/main.js", import.meta.url), "utf8");
const about = readFileSync(new URL("../src-tauri/src/about.rs", import.meta.url), "utf8");
const nsi = readFileSync(
  new URL("../src-tauri/nsis/installer.nsi", import.meta.url),
  "utf8"
);

test("需要重算状态的路径都重新读取更新状态", () => {
  const calls = src.split("await refreshUpdateState();").length - 1;
  // 下载成功、下载失败、安装未启动 —— 三条路径都要重算 downloadedPath
  assert.equal(calls, 3);
});

test("状态刷新只读缓存、不联网，且失败时有兜底", () => {
  const from = src.indexOf("async function refreshUpdateState");
  const to = src.indexOf('/** 点「下载最新版」', from);
  assert.ok(from > 0 && to > from, "找不到刷新函数");
  const body = src.slice(from, to);
  assert.match(body, /invoke\("update_state"\)/, "必须读本地缓存");
  assert.doesNotMatch(body, /check_update/, "失败路径不能触发联网检查");
  assert.match(body, /downloadedPath: ""/, "后端不可用时要本地清空");
});

test("下载态文案不超过按钮可容纳的字数", () => {
  const m = src.match(/const INSTALL_START_LABEL = "([^"]+)"/);
  assert.ok(m, "找不到共用的文案常量");
  assert.ok([...m[1]].length <= 10, `「${m[1]}」超过 10 字，142px 按钮会裁字`);
  assert.equal(src.split("INSTALL_START_LABEL").length - 1, 2, "定义一次、安装路径用一次");
  assert.doesNotMatch(
    src,
    /setDownloadDone\("下载完成|setDownloadDone\("正在启动安装程序/,
    "不允许再直接写长文案"
  );
});

test("下载完成不自动拉起安装器，进入已就绪状态", () => {
  const from = src.indexOf("async function startDownload");
  const to = src.indexOf("async function installDownloaded", from);
  const body = src.slice(from, to);
  assert.match(body, /download_update/, "下载完成后要刷新状态");
  assert.doesNotMatch(body, /install_downloaded/, "下载路径不许再拉起安装器");
  assert.match(body, /点「立即安装」/, "要告诉用户下一步");
  assert.doesNotMatch(body, /setDownloadDone/, "下载完不进入安装态按钮");
});

test("UAC 取消单独识别，不报成下载失败", () => {
  const from = src.indexOf("async function installDownloaded");
  const to = src.indexOf("async function loadAbout", from);
  const inst = src.slice(from, to);
  assert.match(inst, /r\?\.cancelled/, "取消要单独识别");
  assert.match(inst, /已取消安装/, "取消要有对应提示");
  assert.doesNotMatch(inst, /toast\("下载失败/, "取消不能弹下载失败的提示");
  assert.match(inst, /r\?\.launched/, "只有确认启动才提示即将退出");
});

test("后端同步等 UAC 结果，取消时不退出主程序", () => {
  assert.match(about, /ShellExecuteExW/, "必须用能等 UAC 结果的 API");
  assert.match(about, /ERROR_CANCELLED/, "要能识别用户点「否」");
  assert.match(about, /launched: true/, "确认启动才返回 launched");
  const from = about.indexOf("pub fn install_downloaded");
  const body = about.slice(from, from + 7000);
  const cancelAt = body.indexOf("if ok == 0");
  const spawnAt = body.indexOf("std::thread::spawn");
  assert.ok(cancelAt > 0 && spawnAt > cancelAt, "spawn 必须在 UAC 成功的分支之后");
  assert.match(body.slice(cancelAt, spawnAt), /return Ok\(InstallOutcome/, "取消分支要先 return");
});

test("安装器模板不再强杀运行中的轻贴", () => {
  const installAt = nsi.indexOf("; Copy main executable");
  assert.ok(installAt > 0);
  // 卸载段仍保留 taskkill（合理），安装段不许再有
  assert.doesNotMatch(nsi.slice(0, installAt), /taskkill\.exe.*\/F \/IM/, "安装段不许强杀");
});
