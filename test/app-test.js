/**
 * self_clipboard — 集成测试
 * 
 * 运行方式：node test/app-test.js
 */

const assert = require("assert");
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");

function readFile(filePath) {
  return fs.readFileSync(path.join(ROOT, filePath), "utf-8");
}

// ========== 1: 列表空白区域可拖拽 ==========

function testListAreaIsDraggable() {
  const css = readFile("renderer/style.css");

  assert(css.includes("drag"), ".window should have -webkit-app-region: drag");

  // .list 不应有 no-drag，继承父级的 drag
  const listBlock = extractBlock(css, ".list");
  if (listBlock) {
    assert(
      !listBlock.includes("no-drag"),
      ".list should NOT have -webkit-app-region: no-drag"
    );
  }

  // .item 必须有 no-drag（点击条目不拖拽）
  assert(css.includes("no-drag"), ".item should have -webkit-app-region: no-drag");

  console.log("  PASS testListAreaIsDraggable");
}

// ========== 2: 条目无 icon ==========

function testNoItemIcons() {
  const js = readFile("renderer/app.js");
  // JS 中不应创建 item-icon 元素
  assert(
    !js.includes("item-icon"),
    "app.js should not create item-icon elements"
  );
  console.log("  PASS testNoItemIcons");
}

// ========== 3: F12 被禁用 ==========

function testF12IsDisabled() {
  const js = readFile("renderer/app.js");
  assert(
    js.includes("F12") && js.includes("preventDefault"),
    "app.js should prevent F12 keydown"
  );

  const main = readFile("main.js");
  assert(
    !main.includes('openDevTools') || main.includes('isDev'),
    "DevTools should only open with --dev flag"
  );

  console.log("  PASS testF12IsDisabled");
}

// ========== 4: 搜索框存在 ==========

function testSearchBoxExists() {
  const html = readFile("renderer/index.html");
  assert(
    html.includes("search") && html.includes("input"),
    "HTML should contain search input"
  );
  console.log("  PASS testSearchBoxExists");
}

// ========== 5: 卡片布局 ==========

function testItemsUseCardStyle() {
  const css = readFile("renderer/style.css");
  assert(css.includes("border-radius"), "items should have border-radius");
  assert(css.includes("16px"), "item title should be 16px");
  assert(css.includes("12px"), "item time should be 12px");
  console.log("  PASS testItemsUseCardStyle");
}

// ========== 6: IPC 通信 ==========

function testIPCChannelExists() {
  const preload = readFile("preload.js");
  const main = readFile("main.js");
  assert(preload.includes("copyToClipboard"), "preload exposes copyToClipboard");
  assert(preload.includes("clearAll"), "preload exposes clearAll");
  assert(preload.includes("onHistoryUpdate"), "preload listens for history updates");
  assert(main.includes("copy-to-clipboard"), "main handles copy-to-clipboard");
  assert(main.includes("clear-all"), "main handles clear-all");
  console.log("  PASS testIPCChannelExists");
}

// ========== 辅助 ==========

function extractBlock(css, selector) {
  const lines = css.split("\n");
  let inBlock = false;
  let block = "";
  for (const line of lines) {
    if (line.trim().startsWith(selector)) {
      inBlock = true;
    }
    if (inBlock) {
      block += line + "\n";
      if (line.trim() === "}") break;
    }
  }
  return block || null;
}

// ========== 运行全部 ==========

function runAll() {
  console.log("=== self_clipboard Integration Tests ===\n");
  const tests = [
    testListAreaIsDraggable,
    testNoItemIcons,
    testF12IsDisabled,
    testSearchBoxExists,
    testItemsUseCardStyle,
    testIPCChannelExists,
  ];

  let passed = 0, failed = 0;
  for (const test of tests) {
    try { test(); passed++; }
    catch (e) { console.log("  FAIL", test.name, "-", e.message); failed++; }
  }
  console.log(`\nResults: ${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
}

runAll();
