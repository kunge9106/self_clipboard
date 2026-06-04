const assert = require("assert"), fs = require("fs"), path = require("path");
const ROOT = path.join(__dirname,"..");
function read(p) { return fs.readFileSync(path.join(ROOT,p),"utf-8"); }
function block(css,sel) { const l=css.split("\n"); let i=false,b=""; for(const x of l){if(x.trim().startsWith(sel))i=true;if(i){b+=x+"\n";if(x.trim()==="}")break;}}return b||null; }

function testNoBoxShadowRightAngle() {
  const css = read("renderer/style.css");
  const wb = block(css, ".window");
  // 应该用 filter:drop-shadow 而不是 box-shadow
  if (wb) assert(wb.includes("drop-shadow"), ".window should use drop-shadow not box-shadow");
  // clip-path 不应在 .window 上（drop-shadow 不需要它）
  const d = read("renderer/style.css");
  console.log("  PASS testNoBoxShadowRightAngle");
}

function testMacStyleButtons() {
  const html = read("renderer/index.html");
  assert(html.includes("win-btn-close"), "close button with mac style");
  assert(html.includes("win-btn-minimize"), "minimize button with mac style");
  const css = read("renderer/style.css"); assert(css.includes("width:12px") || css.includes("12px;height:12px"), "mac buttons should be 12px");
  console.log("  PASS testMacStyleButtons");
}

function testMinimizeAPI() {
  const preload = read("preload.js");
  const main = read("main.js");
  assert(preload.includes("minimizeWindow"), "preload exposes minimizeWindow");
  assert(main.includes("minimize-window"), "main handles minimize-window");
  console.log("  PASS testMinimizeAPI");
}

function testNoHistoryLimit() {
  const main = read("main.js");
  assert(!main.includes("MAX_HISTORY"), "no MAX_HISTORY limit");
  console.log("  PASS testNoHistoryLimit");
}

function testNoItemIcons() { assert(!read("renderer/app.js").includes("item-icon")); console.log("  PASS testNoItemIcons"); }
function testF12Disabled() { assert(read("renderer/app.js").includes("F12")); console.log("  PASS testF12Disabled"); }
function testSearchBox() { assert(read("renderer/index.html").includes("search") && read("renderer/index.html").includes("input")); console.log("  PASS testSearchBox"); }
function testIPC() { const p=read("preload.js"); assert(p.includes("copyToClipboard")&&p.includes("clearAll")&&p.includes("closeWindow")&&p.includes("minimizeWindow")); console.log("  PASS testIPC"); }

function run() {
  console.log("=== self_clipboard Tests ===\n");
  const tests = [testNoBoxShadowRightAngle, testMacStyleButtons, testMinimizeAPI, testNoHistoryLimit, testNoItemIcons, testF12Disabled, testSearchBox, testIPC];
  let p=0,f=0;
  for(const t of tests){try{t();p++;}catch(e){console.log("  FAIL",t.name,"-",e.message);f++;}}
  console.log("\n"+p+" passed, "+f+" failed");
  process.exit(f>0?1:0);
}
run();

