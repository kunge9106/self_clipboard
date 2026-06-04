const a=require("assert"),fs=require("fs"),p=require("path");
const R=p.join(__dirname,"..");
function rd(f){return fs.readFileSync(p.join(R,f),"utf-8");}
function bl(c,s){const l=c.split("\n");let i=!1,b="";for(const x of l){if(x.trim().startsWith(s))i=!0;if(i){b+=x+"\n";if(x.trim()==="}")break;}}return b||null;}

function t1_shadow() {
  const c=rd("renderer/style.css"),html=rd("renderer/index.html");
  a.ok(c.includes(".window-inner"),"has .window-inner");
  a.ok(html.includes("window-inner"),"HTML has window-inner");
  a.ok(c.includes("box-shadow"),"shadow exists");
  const ib=bl(c,".window-inner");
  if(ib) a.ok(ib.includes("overflow:hidden"),"window-inner has overflow:hidden");
  console.log("  PASS t1_shadow");
}

function t2_radius() {
  const c=rd("renderer/style.css");
  const r=c.match(/--radius-window:\s*(\d+)px/);
  a.ok(r,"radius variable exists");
  a.ok(parseInt(r[1])<=20&&parseInt(r[1])>=14,"radius 14-20px");
  console.log("  PASS t2_radius");
}

function t3_buttons() {
  const c=rd("renderer/style.css");
  a.ok(c.includes("width:16px"),"buttons ~16px");
  console.log("  PASS t3_buttons");
}

function t4_minSize() {
  const m=rd("main.js");
  a.ok(m.includes("minWidth"),"minWidth set");
  a.ok(m.includes("minHeight"),"minHeight set");
  a.ok(m.includes("width: 300")||m.includes("width:300"),"default width matches min");
  console.log("  PASS t4_minSize");
}

function t5_textTruncate() {
  const c=rd("renderer/style.css");
  a.ok(c.includes("text-align:left")||c.includes("text-align: left"),"text left-aligned");
  a.ok(c.includes("ellipsis"),"text-overflow ellipsis");
  console.log("  PASS t5_textTruncate");
}

function t6_noItemIcons(){a.ok(!rd("renderer/app.js").includes("item-icon"));console.log("  PASS t6_noItemIcons");}
function t7_F12(){a.ok(rd("renderer/app.js").includes("F12"));console.log("  PASS t7_F12");}
function t8_search(){a.ok(rd("renderer/index.html").includes("search"));console.log("  PASS t8_search");}
function t9_IPC(){const p=rd("preload.js");a.ok(p.includes("copyToClipboard")&&p.includes("clearAll")&&p.includes("closeWindow")&&p.includes("minimizeWindow"));console.log("  PASS t9_IPC");}
function t10_noHistoryLimit(){a.ok(!rd("main.js").includes("MAX_HISTORY"));console.log("  PASS t10_noHistoryLimit");}

function run(){
  console.log("=== self_clipboard Tests ===\n");
  const ts=[t1_shadow,t2_radius,t3_buttons,t4_minSize,t5_textTruncate,t6_noItemIcons,t7_F12,t8_search,t9_IPC,t10_noHistoryLimit];
  let p=0,f=0;
  for(const t of ts){try{t();p++;}catch(e){console.log("  FAIL",t.name,"-",e.message);f++;}}
  console.log("\n"+p+" passed, "+f+" failed");
  process.exit(f>0?1:0);
}
run();
