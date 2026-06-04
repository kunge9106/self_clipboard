/**
 * self_clipboard — 集成测试
 * 运行方式：node test/app-test.js
 */
const assert = require("assert");
const fs = require("fs");
const path = require("path");
const ROOT = path.join(__dirname, "..");
function readFile(p) { return fs.readFileSync(path.join(ROOT, p), "utf-8"); }
function extractBlock(css, sel) {
  const lines = css.split("\n"); let inB = false, b = "";
  for (const l of lines) { if (l.trim().startsWith(sel)) inB = true; if (inB) { b += l + "\n"; if (l.trim() === "}") break; } }
  return b || null;
}

function testSearchAreaBorderDraggable() {
  const css = readFile("renderer/style.css");
  const sa = extractBlock(css, ".search-area");
  if (sa) assert(!sa.includes("no-drag"), ".search-area should NOT have no-drag");
  const sb = extractBlock(css, ".search-box");
  if (sb) assert(sb.includes("no-drag"), ".search-box should have no-drag");
  console.log("  PASS testSearchAreaBorderDraggable");
}

function testNoRightAngleShadow() {
  const css = readFile("renderer/style.css");
  assert(css.includes("border-radius"), "border-radius exists");
  assert(css.includes("clip-path"), "clip-path exists for shadow clipping");
  console.log("  PASS testNoRightAngleShadow");
}

function testCloseButtonExists() {
  const html = readFile("renderer/index.html");
  const js = readFile("renderer/app.js");
  const preload = readFile("preload.js");
  const main = readFile("main.js");
  assert(html.includes("close"), "HTML close button");
  assert(js.includes("close"), "app.js close handler");
  assert(preload.includes("closeWindow"), "preload closeWindow");
  assert(main.includes("close-window"), "main close-window handler");
  console.log("  PASS testCloseButtonExists");
}

function testNoItemIcons() {
  assert(!readFile("renderer/app.js").includes("item-icon"), "no item-icon");
  console.log("  PASS testNoItemIcons");
}

function testF12IsDisabled() {
  assert(readFile("renderer/app.js").includes("F12") && readFile("renderer/app.js").includes("preventDefault"));
  console.log("  PASS testF12IsDisabled");
}

function testSearchBoxExists() {
  assert(readFile("renderer/index.html").includes("search") && readFile("renderer/index.html").includes("input"));
  console.log("  PASS testSearchBoxExists");
}

function testIPCChannelExists() {
  const p = readFile("preload.js");
  assert(p.includes("copyToClipboard") && p.includes("clearAll") && p.includes("closeWindow"));
  console.log("  PASS testIPCChannelExists");
}

function runAll() {
  console.log("=== self_clipboard Tests ===\n");
  const tests = [testSearchAreaBorderDraggable, testNoRightAngleShadow, testCloseButtonExists, testNoItemIcons, testF12IsDisabled, testSearchBoxExists, testIPCChannelExists];
  let p = 0, f = 0;
  for (const t of tests) { try { t(); p++; } catch(e) { console.log("  FAIL", t.name, "-", e.message); f++; } }
  console.log("\n" + p + " passed, " + f + " failed");
  process.exit(f > 0 ? 1 : 0);
}
runAll();
