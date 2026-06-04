const a=require('assert'),fs=require('fs'),p=require('path');
const R=p.join(__dirname,'..');
function rd(f){return fs.readFileSync(p.join(R,f),'utf-8');}
function t1_oneLayer(){const c=rd('renderer/style.css'),h=rd('renderer/index.html');
  a.ok(!h.includes('window-inner'),'no inner wrapper');
  const wl=c.match(/\.window {[^}]+}/);
  if(wl){const block=wl[0];a.ok(block.includes('border-radius'),'.window has radius');a.ok(block.includes('overflow:hidden'),'.window has overflow');}
  console.log('  PASS t1_oneLayer');}
function t2_radius(){const m=rd('renderer/style.css').match(/--radius-window:\s*(\d+)px/);a.ok(m&&parseInt(m[1])<=20);console.log('  PASS t2_radius');}
function t3_buttons(){a.ok(rd('renderer/style.css').includes('width:16px'));console.log('  PASS t3_buttons');}
function t4_minSize(){const m=rd('main.js');a.ok(m.includes('minWidth')&&m.includes('minHeight')&&m.includes('minHeight: 408'));console.log('  PASS t4_minSize');}
function t5_text(){a.ok(rd('renderer/style.css').includes('ellipsis'));console.log('  PASS t5_text');}
function t6_noIcons(){a.ok(!rd('renderer/app.js').includes('item-icon'));console.log('  PASS t6_noIcons');}
function t7_F12(){a.ok(rd('renderer/app.js').includes('F12'));console.log('  PASS t7_F12');}
function t8_search(){a.ok(rd('renderer/index.html').includes('search'));console.log('  PASS t8_search');}
function t9_IPC(){const p=rd('preload.js');a.ok(p.includes('copyToClipboard')&&p.includes('clearAll')&&p.includes('closeWindow')&&p.includes('minimizeWindow'));console.log('  PASS t9_IPC');}
function t10_noLimit(){a.ok(!rd('main.js').includes('MAX_HISTORY'));console.log('  PASS t10_noLimit');}
function run(){
  console.log('=== Tests ===\n');
  const ts=[t1_oneLayer,t2_radius,t3_buttons,t4_minSize,t5_text,t6_noIcons,t7_F12,t8_search,t9_IPC,t10_noLimit];
  let p=0,f=0;for(const t of ts){try{t();p++;}catch(e){console.log('  FAIL',t.name,'-',e.message);f++;}}
  console.log('\n'+p+' passed, '+f+' failed');process.exit(f>0?1:0);}
run();

