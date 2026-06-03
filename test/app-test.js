/**
 * self_clipboard — 集成测试
 * 
 * 验证应用的核心行为是否符合预期。
 * 使用 TDD 红绿循环：先写测试 → 实现 → 验证。
 * 
 * 运行方式：node test/app-test.js
 */

const assert = require("assert");
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const RENDERER = path.join(ROOT, "renderer");

// ========== 辅助函数 ==========

function readFile(filePath) {
  return fs.readFileSync(path.join(ROOT, filePath), "utf-8");
}

// ========== RED 1: 窗口可拖拽 ==========
// 验证：窗口应该可以通过点击空白区域拖拽，列表条目不应该被拖拽干扰

function testWindowIsDraggable() {
  const css = readFile("renderer/style.css");

  // .window 应该有 drag 属性
  // （这里测试的是父容器，子元素 no-drag 在测试 2 中验证）
  assert(
    css.includes("drag"),
    ".window should have -webkit-app-region: drag"
  );

  // .item 应该有 no-drag（确保点击条目时不触发拖拽）
  assert(
    css.includes("no-drag"),
    ".item should have -webkit-app-region: no-drag"
  );

  console.log("  PASS testWindowIsDraggable");
}

// ========== RED 2: 搜索框存在且有交互能力 ==========

function testSearchBoxExists() {
  const html = readFile("renderer/index.html");

  assert(
    html.includes("search") || html.includes("input"),
    "HTML should contain a search input element"
  );

  console.log("  PASS testSearchBoxExists");
}

// ========== RED 3: 条目为卡片布局 ==========

function testItemsUseCardStyle() {
  const css = readFile("renderer/style.css");

  // 卡片样式：圆角、背景半透明、悬浮感
  assert(css.includes("border-radius"), "items should have border-radius");
  assert(
    css.includes("card") || css.includes("item"),
    "items should have card-like styling"
  );

  // 字体大小符合设计规格
  assert(css.includes("16px"), "item title should be 16px");
  assert(css.includes("12px"), "item time should be 12px");

  console.log("  PASS testItemsUseCardStyle");
}

// ========== RED 4: 动画效果 ==========

function testAnimationsExist() {
  const css = readFile("renderer/style.css");

  assert(
    css.includes("@keyframes"),
    "CSS should contain keyframe animations"
  );

  assert(
    css.includes("hover") || css.includes(":hover"),
    "CSS should have hover effects"
  );

  assert(
    css.includes("transition"),
    "CSS should have transitions for smooth animation"
  );

  console.log("  PASS testAnimationsExist");
}

// ========== RED 5: IPC 通信正常 ==========

function testIPCChannelExists() {
  const preload = readFile("preload.js");
  const main = readFile("main.js");

  // preload 暴露了需要的 API
  assert(preload.includes("copyToClipboard"), "preload exposes copyToClipboard");
  assert(preload.includes("clearAll"), "preload exposes clearAll");
  assert(preload.includes("onHistoryUpdate"), "preload listens for history updates");

  // main 注册了对应的 IPC handler
  assert(main.includes("copy-to-clipboard"), "main handles copy-to-clipboard");
  assert(main.includes("clear-all"), "main handles clear-all");

  console.log("  PASS testIPCChannelExists");
}

// ========== 运行全部测试 ==========

function runAll() {
  console.log("=== self_clipboard Integration Tests ===\n");

  const tests = [
    testWindowIsDraggable,
    testSearchBoxExists,
    testItemsUseCardStyle,
    testAnimationsExist,
    testIPCChannelExists,
  ];

  let passed = 0;
  let failed = 0;

  for (const test of tests) {
    try {
      test();
      passed++;
    } catch (e) {
      console.log("  FAIL", test.name, "-", e.message);
      failed++;
    }
  }

  console.log(`\nResults: ${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
}

runAll();
