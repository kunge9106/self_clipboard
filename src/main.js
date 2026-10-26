import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  cardPreviewHtml,
  hostOf,
  esc as escFmt,
  renderMarkdown,
  highlightCode,
  sanitizeRichHtml,
} from "./formats.js";

const appWindow = getCurrentWindow();

// ---------------------------------------------------------------------------
// 前端错误上报：JS 异常、未处理的 Promise 拒绝、console.error → Rust 日志。
//
// 2.1.10 的教训：WebView OOM 白屏后日志一片空白——渲染进程里发生的一切
// Rust 的 panic 钩子根本看不到。这个上报通道是排查前端侧问题的唯一线索。
//
// 防递归：reportFrontend 自身绝不再调 console.error（内部全 try/catch 吞掉），
// 否则上报失败→console.error→再上报→死循环。
// ---------------------------------------------------------------------------
function reportFrontend(level, tag, msg) {
  try {
    invoke("frontend_log", {
      level,
      tag,
      msg: String(msg).slice(0, 2000),
    }).catch(() => {});
  } catch {
    /* 上报失败就放弃，绝不能再抛 */
  }
}

window.addEventListener("error", (e) => {
  // 资源加载失败（img/script/link）与 JS 运行时错误都在这里
  const what = e.target && e.target !== window
    ? `资源加载失败: ${e.target.tagName || "?"} ${(e.target.src || e.target.href || "")}`
    : `${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`;
  reportFrontend("ERROR", "window.onerror", what);
});
window.addEventListener("unhandledrejection", (e) => {
  const r = e.reason;
  reportFrontend("ERROR", "unhandledrejection", r instanceof Error ? `${r.message}\n${r.stack || ""}` : String(r));
});
{
  const origError = console.error.bind(console);
  console.error = (...args) => {
    origError(...args);
    reportFrontend(
      "ERROR",
      "console.error",
      args.map((a) => (a instanceof Error ? `${a.message}\n${a.stack || ""}` : String(a))).join(" ")
    );
  };
}

/** 材质档位 → 实际可用的圆角上限（由 Rust 端判定，前端不自行猜测） */
const TIER_CLASS = {
  "system-backdrop": "tier-system-backdrop",
  mica: "tier-mica",
  acrylic: "tier-acrylic",
  solid: "tier-solid",
};

// 全局状态
let items = []; // 历史条目（内存中仅元数据，内容在磁盘）
let currentView = "history";

// 批量删除：由右键菜单进入。进入后点击条目即切换选中（不再复制回剪贴板），
// 顶部搜索栏让位给选中操作条（见 index.html 的 .selectbar）。
let selectMode = false;
const selectedIds = new Set();
let query = "";
let settings = null; // 后端配置（get_settings）

// 设置页当前激活的分类。改这个变量只会影响「下次渲染时显示哪一组」，
// 切换标签后重新 settingsHtml() + bindSettings() 即可（见 bindSettings 里的标签点击）。
let settingsTab = "general";

// 更新检查结果。Rust 在启动时会自检一次并（有新版本时）推 update-available；
// 前端其余时候只读缓存（update_state），**只有用户点按钮才重新联网**。
let updateInfo = null;
// 下载进度。存成模块状态是为了「切走再切回来」时还能把进度条画回去。
let dlState = { running: false, percent: 0, error: "" };

/** 窗口圆角上限（后端按材质档位下发，Win10 acrylic = 20px） */
let maxRadius = 20;

async function applyEffectInfo() {
  let info;
  try {
    info = await invoke("effect_info");
  } catch (e) {
    document.body.classList.add("tier-solid");
    document.getElementById("effect-debug").textContent =
      "读取材质档位失败（按 solid 处理）：" + e;
    return;
  }

  document.body.classList.add(TIER_CLASS[info.tier] || "tier-solid");

  maxRadius = info.tier === "system-backdrop" ? 12 : Math.min(info.maxRadius || 20, 20);
  // 圆角模式在启动时就同步，避免首帧闪一下模糊或直角
  applyRoundMode(info.round);

  document.getElementById("effect-debug").textContent =
    `build ${info.build} · tier ${info.tier} · radius ${maxRadius}px`;
}

function applyTheme(dark) {
  document.body.classList.toggle("dark", !!dark);
  syncThemeToggleLabel();
}

/// 侧栏那个切换按钮的文案跟着当前深浅变：深色时写「浅色」，点了就去浅色。
/// 放在 applyTheme 里统一刷，跟随系统变化、手动切换都不会出现「写着深色其实已是深色」。
function syncThemeToggleLabel() {
  const el = document.getElementById("theme-toggle-label");
  if (el) el.textContent = document.body.classList.contains("dark") ? "浅色" : "深色";
}

/// 设置页三选一的高亮。存在才动 —— 侧栏按钮与设置页可能不同时可见，
/// 用同一个函数刷新，两边就不会各刷各的、各说各话。
function markThemeSeg(mode) {
  const seg = document.getElementById("set-theme");
  if (!seg) return;
  seg.querySelectorAll("button[data-theme]").forEach((b) =>
    b.classList.toggle("on", b.dataset.theme === mode)
  );
}

/// 切换主题模式的**唯一入口**：侧栏快捷按钮与设置页三选一都走这里。
///
/// 同步关系是单向收敛的：手动点了任何一边 → 模式落到 light/dark（脱离 system），
/// 设置页选中项与侧栏文案一起更新；只有选了「系统」才重新跟随 Windows，
/// 此时侧栏按钮仍按**当前实际深浅**显示它点了会去哪边。
async function setThemeMode(mode) {
  try {
    await invoke("set_theme_mode", { value: mode });
  } catch (e) {
    // 不能静默：最常见的原因是跑着旧二进制（改了 Rust 没重新编译），
    // 命令根本不存在，用户看到的就是「点了没反应」。
    toast(`主题切换失败：${e}`);
    console.error("set theme mode failed", e);
    return;
  }
  settings = { ...(settings || {}), themeMode: mode };
  await applyThemeMode(mode);
  markThemeSeg(mode);
}

/// 侧栏深浅色快捷切换：当前是深就切浅、是浅就切深。
/// 只落 light/dark 两档 —— 「系统」是另一回事，留在设置 → 通用里选。
function bindThemeToggle() {
  const btn = document.getElementById("theme-toggle");
  if (!btn) return;
  syncThemeToggleLabel();
  btn.addEventListener("click", async () => {
    const mode = document.body.classList.contains("dark") ? "light" : "dark";
    try {
      await setThemeMode(mode);
    } catch (e) {
      console.error("set theme mode failed", e);
    }
  });
}

/**
 * 应用主题模式：浅色/深色直接钉住，system 才跟系统。
 *
 * 窗口主题在 Rust 侧已按设置钉好（`effects::apply_theme_mode`，启动时与切换时各一次），
 * 前端只负责把 `body.dark` 同步到位 —— 三档共用同一条 onThemeChanged 链路，
 * 所以手动钉住时系统主题变化不会再影响界面。
 */
async function applyThemeMode(mode) {
  if (mode === "light") return applyTheme(false);
  if (mode === "dark") return applyTheme(true);
  applyTheme((await appWindow.theme().catch(() => "light")) === "dark");
}

/**
 * 材质浓度 → 控制 CSS 层 --glass-alpha（窗口主体的不透明度）。
 *
 * 为什么不再调 Rust 的 tint：SetWindowRgn 对 acrylic 无效，acrylic 恒为矩形，
 * 四角切不掉。把 acrylic 的 tint 压到 8%（只提供模糊）后，四角的矩形残留几乎
 * 看不见；窗口的明暗改由 CSS 这一层负责，浓度也就可以在这里实时调。
 */
function applyGlassAlpha(pct) {
  // 兜底档（没有真材质）必须保持不透明，否则会直接看见桌面
  if (document.body.classList.contains("tier-solid")) return;
  const a = Math.max(10, Math.min(95, Number(pct) || 0)) / 100;
  document.documentElement.style.setProperty("--glass-alpha", String(a));
}

/**
 * 把浓度同步进模块级 `settings`（界面渲染读它）。
 *
 * 两个模式各记一档，所以要写的是**当前模式**那一档；
 * `materialAlpha` 是「当前生效值」，滑块与读数都读它，一并更新。
 */
function syncAlphaToSettings(v) {
  if (!settings) return;
  settings.materialAlpha = v;
  if (settings.roundMode) settings.alphaRound = v;
  else settings.alphaStraight = v;
}

/**
 * 圆角模式：**圆角 + 不用系统模糊**。
 *
 * Win10 的 acrylic 由 DWM 合成、恒为矩形，裁不掉圆角，所以「圆角 + 系统模糊」
 * 必然在四角留下一圈矩形残留。两种模式各自规避这个矛盾：
 *   - 开（圆角）：Rust 侧清掉 accent（无系统 blur），窗口为纯透明 layered
 *     窗口 → 四角真透明、100% 干净；
 *   - 关（直角）：保留系统 blur，窗口改用直角 → 窗口矩形与 acrylic 完全吻合，
 *     同样没有任何残留。
 * 这里只负责 CSS 侧：窗口圆角开关 + 圆角模式下的主体加厚（见 body.round）。
 */
function applyRoundMode(on) {
  document.body.classList.toggle("round", !!on);
  document.documentElement.style.setProperty("--radius", (on ? maxRadius : 0) + "px");
}

/** 最大化 ↔ 还原：图标与提示词跟随窗口状态（需求 #6） */
async function syncMaxState() {
  const maxBtn = document.getElementById("btn-maximize");
  if (!maxBtn) return;
  const max = await appWindow.isMaximized().catch(() => false);
  maxBtn.classList.toggle("is-max", max);
  maxBtn.title = max ? "还原" : "最大化";
}

/** 真正执行关闭：按「关闭时清理缓存 + 退出前提醒」决定要不要先确认。 */
function doCloseWindow() {
  if (settings && settings.cleanOnClose && settings.cleanConfirmExit) {
    openExitModal();
  } else {
    appWindow.close().catch((err) => toast("关闭失败：" + err));
  }
}

function bindWindowControls() {
  const guard = (label, p) =>
    p.catch((err) => toast(label + "失败：" + err));
  document.getElementById("btn-minimize").onclick = () =>
    guard("最小化", appWindow.minimize());
  document.getElementById("btn-maximize").onclick = () =>
    guard("最大化", appWindow.toggleMaximize());
  document.getElementById("btn-close").onclick = () => {
    // 首次关闭先问一次「后台运行 / 直接退出」（问过就不再问，见 firstrun-close 弹窗）
    if (settings && !settings.askedTrayOnClose) {
      const ask = document.getElementById("firstrun-close");
      if (ask) {
        ask.hidden = false;
        return;
      }
    }
    // 「关闭时清理缓存」+「退出前弹窗提醒」都开启时，先确认再退出（需求 #2）
    doCloseWindow();
  };

  // 窗口尺寸变化时同步图标
  appWindow.onResized(() => syncMaxState());
  // 从最小化 / 托盘恢复时尺寸未必变化 → onResized 不触发，图标会停在旧状态，这里补一次。
  // 刻意不在这里重放材质浓度/圆角模式：它们是「即时生效」的（改设置页当场就应用了），
  // 在聚焦时重设样式变量纯属多余，还有概率引起一次重绘闪烁。
  appWindow.onFocusChanged(({ payload: focused }) => {
    if (focused) syncMaxState();
  });
  syncMaxState();
}

/**
 * 悬浮态防残留。
 *
 * Chromium 的 `:hover` 是引擎内部状态，只在收到**真实鼠标移动**时才重算。窗口最小化
 * 或隐藏到托盘时，WebView 收不到 WM_MOUSELEAVE，于是「最小化前光标停着的那个元素」
 * 会一直保持 `:hover` —— 从任务栏/托盘恢复后，右上角那几个窗口按钮就带着背景色，
 * 直到鼠标真的动一下才消失。
 *
 * 做法：窗口失焦时给 `<html>` 挂上 `.hovers-off`，CSS 里所有 `:hover` 规则都写在
 * `html:not(.hovers-off)` 之下，于是整体被屏蔽；等鼠标真的在本窗口里移动/按下之后
 * 再摘掉 —— 那时 Chromium 会按新坐标重算 hover，得到的才是真实结果。
 *
 * 注意：只在**失焦**时挂载、不在聚焦时挂载。因为点击窗口的 `mousedown` 早于 `focus`，
 * 若在 focus 时挂载，会出现「先摘掉又挂上」→ hover 再也不亮。
 */
function bindHoverGuard() {
  const html = document.documentElement;
  const off = () => html.classList.add("hovers-off");
  const on = () => html.classList.remove("hovers-off");

  appWindow.onFocusChanged(({ payload: focused }) => {
    if (!focused) off();
  });
  window.addEventListener("mousemove", on, { passive: true });
  window.addEventListener("mousedown", on, { passive: true });
  document.addEventListener("mouseleave", off);
  on();
}

// 自定义拖动（不用系统移动循环，避免 Win10 acrylic 的幽灵窗口 & 拖动变色）
function bindDrag() {
  const bar = document.querySelector(".titlebar");
  if (!bar) return;
  bar.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    if (e.target.closest(".window-controls")) return; // 窗口按钮不参与拖动
    invoke("drag_start").catch((err) => console.error("drag failed", err));
  });
  // 双击标题栏空白处 → 最大化/还原
  bar.addEventListener("dblclick", (e) => {
    if (e.target.closest(".window-controls")) return;
    appWindow.toggleMaximize().catch(() => {});
  });
}

// 窗口缩放热区：系统缩放循环在 Win10 acrylic 下无法节流（缩放滞后），
// 改由边缘热区触发自定义缩放（Rust 侧 SetCapture + 16ms 节流）
function bindResize() {
  const maxed = () =>
    document.getElementById("btn-maximize")?.classList.contains("is-max");
  document.querySelectorAll(".rz").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || maxed()) return;
      e.preventDefault();
      invoke("resize_start", { edge: Number(el.dataset.edge) }).catch((err) =>
        console.error("resize failed", err)
      );
    });
  });
}

// ---------------------------------------------------------------------------
// 退出确认弹窗（需求 #2）
// ---------------------------------------------------------------------------

function openExitModal() {
  const modal = document.getElementById("exit-modal");
  const includePinned = settings && settings.cleanIncludePinned;
  document.getElementById("exit-modal-text").textContent = includePinned
    ? "将清理全部缓存（含置顶/收藏），确定退出？"
    : "将清理缓存（保留置顶/收藏），确定退出？";
  modal.hidden = false;
}

function bindExitModal() {
  const modal = document.getElementById("exit-modal");
  document.getElementById("exit-cancel").onclick = () => {
    modal.hidden = true;
  };
  document.getElementById("exit-confirm").onclick = async () => {
    modal.hidden = true;
    try {
      await invoke("exit_app");
    } catch (e) {
      toast("退出失败：" + e);
    }
  };
}

// ---------------------------------------------------------------------------
// 清空全部历史确认弹窗
// ---------------------------------------------------------------------------

// 后端的 clear_all() 是 hist.clear()，没有任何豁免 —— 置顶与收藏也会一并删掉，
// 且磁盘上的内容文件同时被删除，无法恢复。原先这个按钮点了就直接清空、连提示都没有，
// 一次误触就是整库没了，所以这里补上二次确认，并把条数与影响范围如实写明。
function openClearAllModal() {
  const n = items.length;
  if (n === 0) {
    toast("没有可清空的内容");
    return;
  }
  document.getElementById("clear-all-modal-text").textContent =
    `共 ${n} 条，含置顶与收藏，此操作不可撤销。`;
  document.getElementById("clear-all-modal").hidden = false;
}

// 弹窗是 index.html 里的常驻元素（不在 #view 内），所以只在这里绑一次；
// 设置页那个按钮只负责把弹窗打开 —— 这样切分类标签重绘时不会丢绑定。
function bindClearAllModal() {
  const modal = document.getElementById("clear-all-modal");
  document.getElementById("clear-all-cancel").onclick = () => {
    modal.hidden = true;
  };
  document.getElementById("clear-all-confirm").onclick = async () => {
    modal.hidden = true;
    try {
      await invoke("clear_all");
      items = [];
      render();
      toast("已清空全部历史");
    } catch (e) {
      toast("清空失败：" + e);
    }
  };
}

// ---------------------------------------------------------------------------
// 清空「已失效」的文件条目
//
// 失效 = 条目里引用的原文件被移动 / 改名 / 删除，这条记录**永远贴不出来**，
// 属于净负资产：留着占地方，每次渲染还多查一次存在性。
//
// 为什么不给「手动改路径」：那条路要文件选择器、多路径编辑、路径校验，还要覆盖
// 加密的 files_path 并刷新 preview —— 成本很高，而收益只有一个：文件挪了位置还想
// 复用这条历史。可用户重新复制一次那批文件只要两三秒，比走完这套流程快得多，
// 而且「改完指向另一批文件」本质上已经是新条目了，留着旧的时间戳反而是误导。
// 真正需要的是「把死的清掉」，不是「把死的救活」。
//
// 判定完全复用 check_files_alive（给一批 id、返回失效的那些），只是把 id 的来源
// 从「当前视图」换成「全部文件条目」，不为这件事新增后端命令。
// ---------------------------------------------------------------------------

let clearMissingIds = [];

async function openClearMissingModal() {
  const ids = items.filter((i) => i.kind === "files").map((i) => i.id);
  if (ids.length === 0) {
    toast("没有文件条目");
    return;
  }
  let dead;
  try {
    // 只做 exists()，不读内容 —— 几百条也是毫秒级
    dead = await invoke("check_files_alive", { ids });
  } catch (e) {
    toast("检测失败：" + e);
    return;
  }
  if (dead.length === 0) {
    toast("没有已失效的条目");
    return;
  }
  clearMissingIds = dead;
  document.getElementById("clear-missing-modal-text").textContent =
    `共 ${dead.length} 条文件条目的原文件已不存在（被移动、改名或删除），无法再粘贴。此操作不可撤销。`;
  document.getElementById("clear-missing-modal").hidden = false;
}

// ── 删除文件条目时的「本地文件」确认窗 ────────────────────────────────────
// 删除对象是用户磁盘上的真实文件，所以：
//   · 只在设置里开了「删除时同时删本地文件」之后才可能弹（默认关，文件不被动）
//   · 每次删除都弹，不做「不再询问」——误点一次就是真删了一次
//   · 文案必须写清「几个文件 + 用什么方式」，回收站与永久删除的后果完全不同
let purgeResolve = null;

/// 返回 true 表示「用户确认要连本地文件一起处理」。
/// 返回 false 的两条路径（设置没开 / 没有文件条目）都不弹窗，直接按旧行为删记录。
function askPurgeFiles(targets) {
  if (!settings?.deleteFilesWithItem) return Promise.resolve(false);
  const files = targets
    .filter((i) => i.kind === "files")
    .flatMap((i) => i.files || []);
  if (files.length === 0) return Promise.resolve(false);

  const perm = !!settings.deleteFilesPermanent;
  const how = perm ? "永久删除" : "移入回收站";
  const tail = perm ? "（不可恢复）" : "（之后可从回收站还原）";
  const names = files.map((p) => p.split(/[\\/]/).pop() || p);
  const head = names.slice(0, 4).map((n) => `· ${n}`).join("\n");
  const more = names.length > 4 ? `\n…另有 ${names.length - 4} 个` : "";

  document.getElementById("purge-modal-text").textContent =
    `将${how} ${files.length} 个本地文件${tail}：\n\n${head}${more}`;
  document.getElementById("purge-confirm").textContent = perm ? "永久删除" : "移入回收站";
  document.getElementById("purge-modal").hidden = false;
  return new Promise((resolve) => {
    purgeResolve = resolve;
  });
}

function bindPurgeModal() {
  const modal = document.getElementById("purge-modal");
  const done = (v) => {
    modal.hidden = true;
    // 先清空再 resolve：避免回调里再次弹窗时旧的 resolve 被覆盖
    const r = purgeResolve;
    purgeResolve = null;
    r?.(v);
  };
  document.getElementById("purge-cancel").onclick = () => done(false);
  document.getElementById("purge-confirm").onclick = () => done(true);
}

// 与清空全部历史弹窗同理：常驻元素，只绑一次，切分类重绘不会丢绑定
function bindClearMissingModal() {
  const modal = document.getElementById("clear-missing-modal");
  document.getElementById("clear-missing-cancel").onclick = () => {
    modal.hidden = true;
    clearMissingIds = [];
  };
  document.getElementById("clear-missing-confirm").onclick = async () => {
    modal.hidden = true;
    const ids = clearMissingIds;
    clearMissingIds = [];
    if (ids.length === 0) return;
    try {
      await invoke("delete_items", { ids, deleteLocalFiles: false });
      const drop = new Set(ids);
      items = items.filter((i) => !drop.has(i.id));
      // 失效集合整体作废，让下次渲染重新检测，避免残留已删除条目的 id
      missingFiles = new Set();
      render({ keepScroll: true });
      toast(`已清空 ${ids.length} 条失效条目`);
    } catch (e) {
      toast("清空失败：" + e);
    }
  };
}

// ---------------------------------------------------------------------------
// 工具栏垃圾桶：搜索时只清搜索结果，否则清当前视图（需求 #3）
// ---------------------------------------------------------------------------

// 本次弹窗待清理的三组条目；确认时按勾选状态拼出最终 id 列表
let clearViewTargets = null;

// ---------------------------------------------------------------------------
// 批量删除
// ---------------------------------------------------------------------------

function enterSelectMode() {
  selectMode = true;
  selectedIds.clear();
  render({ keepScroll: true });
}

function exitSelectMode() {
  if (!selectMode) return;
  selectMode = false;
  selectedIds.clear();
  render({ keepScroll: true });
}

/**
 * 切换一条的选中状态。
 *
 * 只改这张卡片的 class 并同步操作条，**不整表重绘** —— 重绘会把滚动位置和
 * 已渲染的批次全部丢掉，选几条第 200 行开外的内容就得重新滚回去。
 */
function toggleSelect(id, card) {
  if (selectedIds.has(id)) selectedIds.delete(id);
  else selectedIds.add(id);
  card.classList.toggle("is-selected", selectedIds.has(id));
  syncSelectBar();
}

function syncSelectBar() {
  const n = selectedIds.size;
  const count = document.getElementById("select-count");
  if (count) count.textContent = `已选 ${n} 项`;
  const del = document.getElementById("select-delete");
  if (del) del.disabled = n === 0;
}

async function deleteSelected() {
  const ids = [...selectedIds];
  if (ids.length === 0) return;
  // 先问「要不要连本地文件一起处理」——它是异步的，必须放在拿 ids 之后、
  // 调删除之前；用户取消就退化成普通删除（只删记录）。
  const purge = await askPurgeFiles(items.filter((i) => ids.includes(i.id)));
  try {
    const note = await invoke("delete_items", { ids, deleteLocalFiles: purge });
    if (note) toast(note);
  } catch (e) {
    toast("删除失败：" + e);
    return;
  }
  const gone = new Set(ids);
  items = items.filter((i) => !gone.has(i.id));
  selectMode = false;
  selectedIds.clear();
  // 保留滚动位置：删几条不该把人踹回列表顶部（与单条删除同一口径）
  render({ keepScroll: true });
  toast(`已删除 ${ids.length} 项`);
}

function bindSelectBar() {
  const cancel = document.getElementById("select-cancel");
  if (cancel) cancel.onclick = () => exitSelectMode();
  const del = document.getElementById("select-delete");
  if (del) del.onclick = () => deleteSelected();
}

// 「清空当前视图」：按钮 → 分组 → 弹窗或直接清

function bindClearView() {
  document.getElementById("btn-clear-view").onclick = () => {
    const scope = filtered();
    if (scope.length === 0) {
      toast("没有可清理的条目");
      return;
    }
    // 一条可能既置顶又收藏，所以分组按「置顶优先」划分、三组互不重叠；
    // 真正删除时再按 id 去重兜底。
    const pinned = scope.filter((i) => isPinnedAnywhere(i));
    const favs = scope.filter((i) => i.favorite && !isPinnedAnywhere(i));
    const normal = scope.filter((i) => !isPinnedAnywhere(i) && !i.favorite);

    // 范围内一件置顶/收藏都没有 → 直接清，不弹窗打扰（保持原来的顺手程度）
    if (pinned.length === 0 && favs.length === 0) {
      doClearView(normal.map((i) => i.id));
      return;
    }
    openClearViewModal({ normal, pinned, favs });
  };
}

async function doClearView(ids) {
  const uniq = [...new Set(ids)];
  if (uniq.length === 0) {
    toast("没有选中可清理的条目");
    return;
  }
  try {
    await invoke("delete_items", { ids: uniq, deleteLocalFiles: false });
    const drop = new Set(uniq);
    items = items.filter((i) => !drop.has(i.id));
    render({ keepScroll: true });
    toast(query ? `已清理 ${uniq.length} 条搜索结果` : `已清理 ${uniq.length} 条`);
  } catch (e) {
    toast("清理失败：" + e);
  }
}

function openClearViewModal({ normal, pinned, favs }) {
  clearViewTargets = { normal, pinned, favs };

  document.getElementById("clear-view-modal-text").textContent =
    normal.length > 0
      ? `将清除 ${normal.length} 条${query ? "搜索结果" : "内容"}，默认保留置顶与收藏。`
      : "范围内没有普通条目（全是置顶或收藏），勾选下方选项可一并清除。";

  const pinRow = document.getElementById("clear-view-pinned-row");
  const favRow = document.getElementById("clear-view-fav-row");
  pinRow.hidden = pinned.length === 0;
  favRow.hidden = favs.length === 0;
  document.getElementById("clear-view-pinned-label").textContent =
    `同时清除置顶（${pinned.length} 条）`;
  document.getElementById("clear-view-fav-label").textContent =
    `同时清除收藏（${favs.length} 条）`;
  document.getElementById("clear-view-pinned").checked = false;
  document.getElementById("clear-view-fav").checked = false;
  // 两组都不存在时，空容器仍会撑出两道 12px 的间隙，整块隐藏掉
  document.getElementById("clear-view-checks").hidden =
    pinned.length === 0 && favs.length === 0;

  document.getElementById("clear-view-modal").hidden = false;
}

// 弹窗是 index.html 的常驻元素（不在 #view 内），启动时绑一次即可，
// 与退出弹窗 / 清空全部历史弹窗同理。
function bindClearViewModal() {
  const modal = document.getElementById("clear-view-modal");
  document.getElementById("clear-view-cancel").onclick = () => {
    modal.hidden = true;
  };
  document.getElementById("clear-view-confirm").onclick = async () => {
    modal.hidden = true;
    const t = clearViewTargets;
    if (!t) return;
    const ids = t.normal.map((i) => i.id);
    if (document.getElementById("clear-view-pinned").checked)
      ids.push(...t.pinned.map((i) => i.id));
    if (document.getElementById("clear-view-fav").checked)
      ids.push(...t.favs.map((i) => i.id));
    await doClearView(ids);
  };
}

function bindNav() {
  // 监听整个侧栏：历史/收藏/图片/文件 与 底部「设置」都在 .sidebar 内
  document.querySelector(".sidebar").addEventListener("click", (e) => {
    // 只认带 data-view 的项。侧栏里「深色/浅色」按钮也用了 .nav-item 的样式，
    // 但它不是导航 —— 之前它被这个委托一起处理，`currentView` 被赋成 undefined，
    // 于是点一下深浅色，界面就跳回了别的页面（用户实测）。
    const item = e.target.closest(".nav-item[data-view]");
    if (!item) return;
    document
      .querySelectorAll(".nav-item")
      .forEach((n) => n.classList.remove("is-active"));
    item.classList.add("is-active");
    currentView = item.dataset.view;
    // 换分类 = 换了一批条目，选中集合不再对应，直接退出批量模式
    selectMode = false;
    selectedIds.clear();
    render();
  });
}

function bindSearch() {
  const el = document.getElementById("search");
  const clear = document.getElementById("search-clear");
  // × 按钮只在确实有输入时出现（按 trim 判断：只敲空格视为无输入）
  const sync = () => {
    if (clear) clear.hidden = el.value.trim() === "";
  };
  el.addEventListener("input", () => {
    query = el.value.trim().toLowerCase();
    sync();
    render();
  });
  if (clear)
    clear.addEventListener("click", () => {
      el.value = "";
      query = "";
      sync();
      render(); // 换回完整列表，不回顶部保留位置
      el.focus();
    });
  sync();
}

// ---------------------------------------------------------------------------
// 历史渲染
// ---------------------------------------------------------------------------

/** 「文件复制」是否开启。默认关闭：不采集文件、也不展示任何文件条目。 */
function fileCopyOn() {
  return !!(settings && settings.fileCopy);
}

/** `pinnedViews` 里的通配项：表示所有分类都置顶（设置里开了「置顶互通」时写入） */
const PIN_ALL = "*";

/** 这条内容是否在 view 这个分类里置顶。通配项对所有分类都成立。 */
function isPinnedIn(item, view) {
  const vs = item.pinnedViews;
  return Array.isArray(vs) && (vs.includes(PIN_ALL) || vs.includes(view));
}

/** 是否在任何分类里置顶过（用于清理豁免，避免误删别处置顶的内容） */
function isPinnedAnywhere(item) {
  return Array.isArray(item.pinnedViews) && item.pinnedViews.length > 0;
}

function filtered() {
  let list = items;
  if (currentView === "favorites") list = list.filter((i) => i.favorite);
  else if (currentView === "text") list = list.filter((i) => i.kind === "text");
  else if (currentView === "images") list = list.filter((i) => i.kind === "image");
  else if (currentView === "files") list = list.filter((i) => i.kind === "files");

  // 文件复制开关关闭（默认）→ 文件条目一律不展示。旧条目只是隐藏，数据仍在磁盘上，
  // 重新打开开关就回来了。
  if (!fileCopyOn()) list = list.filter((i) => i.kind !== "files");

  if (query) {
    list = list.filter(
      (i) =>
        (i.preview && i.preview.toLowerCase().includes(query)) ||
        (i.files && i.files.some((f) => f.toLowerCase().includes(query))) ||
        // 类型名也参与匹配。图片条目的 preview 固定是「[图片]」，所以搜「图片」一直
        // 能命中；但搜英文「image」命中不了 —— kind 原本不在匹配范围内，img_path
        // 也不在 i.files 里。把中文标签（图片/文本/文件）与英文 kind（image/text/files）
        // 都纳入后，「图片」和「image」就都能搜出图片条目。
        (KIND_LABEL[i.kind] || "").toLowerCase().includes(query) ||
        (i.kind || "").toLowerCase().includes(query) ||
        // 子类型也参与匹配：搜「代码」/「code」、「链接」/「link」都应命中
        (SUBTYPE_LABEL[i.subtype] || "").toLowerCase().includes(query) ||
        (i.subtype || "").toLowerCase().includes(query)
    );
  }
  // 置顶优先，其余按时间倒序。置顶是**按分类独立**的：只在当前这个分类里置顶过的
  // 才浮到最上面，「文本」里置顶不会影响它在「历史」里的位置。
  return [...list].sort((a, b) => {
    const ap = isPinnedIn(a, currentView);
    const bp = isPinnedIn(b, currentView);
    if (ap !== bp) return ap ? -1 : 1;
    return b.ts - a.ts;
  });
}

function timeStr(ts) {
  const now = Date.now();
  const diff = now - ts;
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  // 1 秒起 → 秒；满 60 秒 → 分钟；满 60 分 → 小时；满 3 小时 → 绝对时间
  if (s < 60) return `${Math.max(1, s)} 秒前`;
  if (m < 60) return `${m} 分钟前`;
  if (h < 3) return `${h} 小时前`;
  const d = new Date(ts);
  const t = new Date(now);
  const p = (n) => String(n).padStart(2, "0");
  const hm = `${p(d.getHours())}:${p(d.getMinutes())}`;
  const sameDay =
    d.getFullYear() === t.getFullYear() &&
    d.getMonth() === t.getMonth() &&
    d.getDate() === t.getDate();
  return sameDay ? hm : `${p(d.getMonth() + 1)}-${p(d.getDate())} ${hm}`;
}

/** 每秒刷新相对时间标签（只改文本，不重渲染列表，避免图片闪烁） */
function startTimeTicker() {
  setInterval(() => {
    document.querySelectorAll(".card-time").forEach((el) => {
      const ts = Number(el.dataset.ts);
      if (!ts) return;
      const next = timeStr(ts);
      if (el.textContent !== next) el.textContent = next;
    });
  }, 1000);
}

const KIND_LABEL = { text: "文本", image: "图片", files: "文件" };
// 文本的子类型标签：只用于卡片徽章与搜索匹配，**不是分类**
// （链接/代码仍然是文本条目，侧栏里不会单独出现）
const SUBTYPE_LABEL = { link: "链接", code: "代码", markdown: "Markdown" };
// 徽章样式：markdown 与 code 同属「源码」，共用一套配色，靠文字区分 ——
// 再开一个色相会让九个徽章挤在一起分不清（颜色已经排满了）。
const SUBTYPE_BADGE = {
  link: ["tag-link", "链接"],
  code: ["tag-code", "代码"],
  markdown: ["tag-code", "Markdown"],
};

/**
 * 重绘列表。
 *
 * @param {{keepScroll?: boolean}} [opts]
 *   keepScroll：保留当前滚动位置。适用于「列表就地变化」的场景——删除条目、
 *   置顶/取消置顶、收藏、后台来了新内容。这些都不会改变用户正在看的位置，
 *   跳回顶部属于纯粹的干扰。
 *   切视图、改搜索词时**不要**开：那是换了一批数据，本来就该从头看。
 *
 * 为什么需要这个参数：本函数会 `view.innerHTML = ""` 并新建 `.history` 容器，
 * 容器一换，scrollTop 必然归零。所以必须在重建**之前**把位置记下来。
 */
function render({ keepScroll = false } = {}) {
  hideHoverPreview(); // 重绘后旧卡片已销毁，避免浮层残留
  const prevTop = keepScroll && wrapEl ? wrapEl.scrollTop : 0;
  const view = document.getElementById("view");
  const toolbar = document.querySelector(".toolbar");
  const isSettings = currentView === "settings";
  // 设置页不需要搜索栏（需求 #1）
  toolbar.style.display = isSettings ? "none" : "";
  // 批量选中模式下搜索栏让位给选中操作条
  toolbar.classList.toggle("is-selecting", selectMode && !isSettings);
  syncSelectBar();
  // 设置页底部状态栏只留版本号：左侧条数与右侧垃圾桶按钮隐藏。
  // 用 visibility 而不是 display —— 网格是 1fr/auto/1fr 三列，display:none 会让版本号
  // 掉进第一列（1fr）而不再居中；隐藏但占位才能保持居中。
  document.querySelector(".statusbar").classList.toggle("is-settings", isSettings);
  if (isSettings) {
    view.innerHTML = settingsHtml();
    bindSettings(view);
    return;
  }

  // 「文件」入口保留，但文件复制关闭时（默认）只给一个说明页，不列任何条目
  if (currentView === "files" && !fileCopyOn()) {
    document.getElementById("status-count").textContent = "共 0 条";
    view.innerHTML = `<div class="empty">
      <div class="empty-title">文件复制已关闭</div>
      <div class="empty-sub">可在「设置 → 记录与缓存」里打开</div>
    </div>`;
    return;
  }

  const list = filtered();
  // 状态栏：搜索中显示「找到 X 条结果」，否则显示总数
  const countEl = document.getElementById("status-count");
  if (countEl)
    countEl.textContent = query
      ? `找到 ${list.length} 条结果`
      : `共 ${fileCopyOn() ? items.length : items.filter((i) => i.kind !== "files").length} 条`;

  if (list.length === 0) {
    view.innerHTML = emptyHtml();
    return;
  }

  // 渐进渲染：只先渲染第一批，滚到接近底部时再追加下一批（见 appendBatch）。
  // 这样 DOM 规模有上界，渲染进程内存不再随历史条数增长；而列表的排序、滚动、
  // 搜索、右键菜单、悬浮预览等行为与全量渲染完全一致。
  renderList = list;
  renderedUpTo = 0;
  wrapEl = document.createElement("div");
  wrapEl.className = "history";
  view.innerHTML = "";
  view.appendChild(wrapEl);
  setupMoreObserver(); // root 必须绑定到这个新的滚动容器
  appendBatch();
  restoreScroll(prevTop);
  refreshMissing(list); // 文件条目的原路径可能已被移动/删除 → 打「已失效」标记
}

/**
 * 还原滚动位置。
 *
 * 渐进渲染下第一批只有 PAGE_SIZE 条，撑不到原来的位置，所以边追加批次边还原，
 * 直到内容够高（或全部渲染完）。追加的批数正好等于重建前 DOM 里的卡片数，
 * 因此内存占用不会比渲染前更高。
 */
function restoreScroll(top) {
  if (!wrapEl || top <= 0) return;
  let guard = 0;
  while (
    wrapEl.scrollHeight - wrapEl.clientHeight < top &&
    renderedUpTo < renderList.length &&
    guard++ < 1000
  ) {
    appendBatch();
  }
  wrapEl.scrollTop = top;
}

// ---------------------------------------------------------------------------
// 渐进渲染
//
// 为什么需要：渲染进程的内存主要由「实际渲染出多少个卡片」决定（实测历史 272 条
// 时渲染进程专用工作集约 67MB，随条数线性上涨）。一次全量渲染会让内存随使用时间
// 持续增长，这与「内容走磁盘、不常驻内存」的设计目标相悖。
//
// 做法：DOM 中最多保留约一屏 + 一批的量，其余滚动到时再生成。
// ---------------------------------------------------------------------------
const PAGE_SIZE = 60; // 每批渲染的条数
let renderList = []; // 当前视图过滤后的完整列表（仅元数据，很轻）
let renderedUpTo = 0; // 已渲染到的下标
let wrapEl = null; // 当前列表滚动容器
let moreObserver = null; // 观察「加载更多」哨兵
let sentinelEl = null; // 哨兵元素本身

function setupMoreObserver() {
  if (moreObserver) moreObserver.disconnect();
  moreObserver = new IntersectionObserver(
    (entries) => {
      if (entries.some((e) => e.isIntersecting)) appendBatch();
    },
    { root: wrapEl, rootMargin: "600px 0px" } // 提前 600px 预渲染，滚动不卡顿
  );
}

/** 追加下一批卡片；若仍有剩余，在末尾放一个不可见哨兵，滚到附近时触发下一批 */
function appendBatch() {
  if (!wrapEl || renderedUpTo >= renderList.length) return;

  if (sentinelEl) {
    moreObserver.unobserve(sentinelEl);
    sentinelEl.remove();
    sentinelEl = null;
  }

  const end = Math.min(renderedUpTo + PAGE_SIZE, renderList.length);
  const frag = document.createDocumentFragment();
  for (let i = renderedUpTo; i < end; i++) {
    frag.appendChild(cardEl(renderList[i]));
  }
  wrapEl.appendChild(frag);
  renderedUpTo = end;

  // 只为本批新卡片里尚未加载的缩略图登记观察（已加载的已摘掉 data-img-id 标记）
  observeImages(wrapEl);
  applyMissingMarks(); // 新追加的批次也要补上失效标记

  if (renderedUpTo < renderList.length) {
    sentinelEl = document.createElement("div");
    sentinelEl.className = "load-more";
    wrapEl.appendChild(sentinelEl);
    moreObserver.observe(sentinelEl);
  }
}

/** 各视图的空态文案（需求 #5），搜索时显示「暂无 xxx 相关内容」（需求 #3） */
function emptyHtml() {
  const trunc = (s, n = 12) =>
    s.length > n ? s.slice(0, n) + "..." : s;
  if (query) {
    return `<div class="empty"><div class="empty-title">暂无 ${trunc(
      query
    )} 相关内容</div></div>`;
  }
  const titles = {
    history: ["暂无内容", "复制点东西试试，会自动记录到这里"],
    text: ["暂无文本", "复制的文本会出现在这里"],
    favorites: ["暂无收藏", "右键条目选「收藏」即可"],
    images: ["暂无图片", "复制的图片会出现在这里"],
    files: ["暂无文件", "复制的文件会出现在这里"],
  };
  const [title, sub] = titles[currentView] || titles.history;
  // 历史视图补第三条操作提示：巡览是「事先讲」，这里是「当场讲」——
  // 用户第一次真正需要知道「还能右键」的时刻，就是他盯着这张空列表的时候。
  // 其余分类不加：那里已经在讲自己的事了，再堆一句反而乱。
  const tip =
    !query && currentView === "history"
      ? `<div class="empty-tip">右键卡片有更多操作 · 悬停 1 秒看更多内容</div>`
      : "";
  return `<div class="empty"><div class="empty-title">${title}</div>
    <div class="empty-sub">${sub || ""}</div>${tip}</div>`;
}

function cardEl(item) {
  const card = document.createElement("div");
  const pinned = isPinnedIn(item, currentView); // 置顶按分类独立，只看当前分类
  card.className =
    "card" +
    (pinned ? " is-pinned" : "") +
    (selectMode && selectedIds.has(item.id) ? " is-selected" : "");
  card.dataset.id = item.id;

  // 内容区
  const body = document.createElement("div");
  body.className = "card-body";
  const meta = document.createElement("div");
  meta.className = "card-meta";
  // 徽章统一用 .tag（几何一致、颜色区分语义）。
  // 「视图本身已经隐含」的标签不再显示，否则整列重复同一个词反而更吵：
  //   文本/图片/文件 视图 → 不显示类型徽章；收藏视图 → 不显示「已收藏」。
  //   历史视图是混合的，全部显示；置顶是按分类独立的，任何视图都显示。
  const hideKind =
    currentView === "text" || currentView === "images" || currentView === "files";
  const hideFav = currentView === "favorites";
  meta.innerHTML =
    (hideKind
      ? ""
      : `<span class="tag tag-kind tag-kind-${item.kind}">${KIND_LABEL[item.kind] || item.kind}</span>`) +
    // 子类型徽章：任何视图都显示（它不是分类，而是「这条内容是什么」的提示，
    // 在「文本」视图里尤其有用 —— 那里所有条目都叫文本，靠它才分得出代码和链接）
    (SUBTYPE_BADGE[item.subtype]
      ? `<span class="tag ${SUBTYPE_BADGE[item.subtype][0]}">${SUBTYPE_BADGE[item.subtype][1]}</span>`
      : "") +
    (pinned ? '<span class="tag tag-pinned">已置顶</span>' : "") +
    (item.favorite && !hideFav ? '<span class="tag tag-fav">已收藏</span>' : "") +
    // 「已编辑」= 在全文编辑里改过并保存后落成的新条目。原文条目始终保留不动，
    // 两条内容相近、挨在一起，靠这个徽章才能一眼分清哪条是自己改过的。
    (item.edited ? '<span class="tag tag-edited">已编辑</span>' : "") +
    // 时间放最后，靠 CSS 的 margin-left:auto 推到最右
    `<span class="card-time" data-ts="${item.ts}">${timeStr(item.ts)}</span>`;
  body.appendChild(meta);

  if (item.kind === "image") {
    // 图片条目：内容区放小缩略图（不显示「[图片]」占位字），
    // 大图仍由鼠标悬浮时的浮层给出（见 bindHoverPreview）
    const img = document.createElement("img");
    img.className = "card-image";
    img.alt = "图片";
    // 不在此处读图：交给 observeImages() 等卡片滚入可视区再加载，
    // 否则一次渲染几十张图会同时产生几十个大 BMP 字符串（内存峰值飙升）
    img.dataset.imgId = item.id;
    body.appendChild(img);
  } else {
    const preview = document.createElement("div");
    // 按子类型给不同排版：代码/Markdown 用等宽字体并着色，链接整体染成链接色。
    // 富文本（html_path）在卡片上仍显示纯文本 —— 小卡片里排版 HTML 收益低、
    // 而且高度会失控；完整排版放在「查看」里做。
    const sub = item.subtype;
    preview.className = "card-preview" + (sub ? ` is-${sub}` : "");
    if (sub === "code" || sub === "markdown") {
      // cardPreviewHtml 内部对每一段都做了转义，可以安全赋给 innerHTML
      preview.innerHTML = cardPreviewHtml(item.preview || "", sub);
    } else if (sub === "link") {
      preview.textContent = item.preview || "";
      // 域名单独提出来做视觉锚点（不改变文本内容）
      const host = hostOf(item.preview || "");
      if (host) preview.title = host;
    } else {
      preview.textContent = item.preview || "";
    }
    body.appendChild(preview);
  }
  card.appendChild(body);

  // 悬浮 1s 弹补充内容：图片给大图、文本给更多行、文件给路径清单。
  // 三种类型共用同一个浮层 —— 定位 / 显隐 / 延迟这套逻辑与类型无关，只该有一份。
  bindHoverPreview(card, item);

  // 左键 → 复制回剪贴板；批量选中模式下改为切换选中
  card.addEventListener("click", () => {
    if (selectMode) {
      toggleSelect(item.id, card);
      return;
    }
    copyBack(item);
  });
  card.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e.clientX, e.clientY, item);
  });
  return card;
}

// ---------------------------------------------------------------------------
// 悬浮补充信息浮层（图片 / 文本 / 文件 共用一份）
//
// 卡片本体只能放一张小缩略图或 2 行摘要，鼠标停留 1s 后按类型补更多内容：
//   图片 → 卡片旁弹大图
//   文本 → 比卡片预览多几行，超出截断
//   文件 → 前 5 条路径，其余折叠成「…另有 N 个」
//
// 只做**一个**浮层盒子：定位、显隐、延迟这套逻辑跟内容类型无关。
// 若按类型各造一个浮层，这份逻辑就会变成三份，日后改延迟 / 改边界必然不同步。
// ---------------------------------------------------------------------------

/**
 * 图片缓存：key = 条目 id，value = **降采样后**的小尺寸 JPEG dataURL。
 *
 * ⚠️ 关键约束：`get_image` 返回的是**未压缩的 BMP**（1080p 一张 ≈ 8MB，
 * base64 后 ≈ 11MB）。若把原图直接缓存或塞进 <img>，几十张图就能吃掉几百 MB
 * —— 实测 WebView2 峰值 528MB，元凶就在这里。所以一律先用 canvas 缩到
 * 最长边 ≤ IMAGE_MAX_EDGE 再缓存，单张降到几十 KB；原始字符串只是局部变量，
 * 函数返回后即可回收，绝不常驻。
 */
const imageCache = new Map();
const IMAGE_CACHE_MAX = 16;
// 卡片缩略图最大 160×60、悬浮预览最大 340×260；512 对 1.5x 高 DPI 也够用。
// 注意：dataURL 字符串很小，但 Chromium 会为每张图保留**解码后的位图**
// （RGBA 约 边长²×4 字节），512 时约 1MB/张、30 张约 30MB；若放到 720
// 则约 2MB/张、60 张可达 120MB —— 这才是图片真正吃掉的内存。
const IMAGE_MAX_EDGE = 512;
let previewBox = null;
let previewTimer = null;

/** 把原始 dataURL 降采样成小尺寸 JPEG（最长边 ≤ maxEdge） */
function shrinkImage(dataUrl, maxEdge, quality = 0.82) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => {
      try {
        const w = img.naturalWidth;
        const h = img.naturalHeight;
        if (!w || !h) return reject(new Error("图片尺寸无效"));
        const scale = Math.min(1, maxEdge / Math.max(w, h));
        const canvas = document.createElement("canvas");
        canvas.width = Math.max(1, Math.round(w * scale));
        canvas.height = Math.max(1, Math.round(h * scale));
        canvas.getContext("2d").drawImage(img, 0, 0, canvas.width, canvas.height);
        resolve(canvas.toDataURL("image/jpeg", quality));
      } catch (e) {
        reject(e);
      }
    };
    img.onerror = () => reject(new Error("图片解码失败"));
    img.src = dataUrl;
  });
}

/** 读取图片（返回降采样后的 dataURL）；命中缓存则不再走 IPC / 磁盘。缩略图与预览共用。 */
async function loadImage(id) {
  const hit = imageCache.get(id);
  if (hit) return hit;
  // 2.1.10 灾难修复：这里原先走 get_image——整张未压缩 BMP（微信原图可达
  // 上百 MB）base64 后经 IPC 进来，<img> 解码峰值直接打爆 WebView2（页面
  // 「内存不足」白屏，且每次启动列表都会重演）。现改用 Rust 侧生成的缩略图
  // （PNG 长边 ≤512，几十 KB），原图永远不进前端；回贴走 copy_back 在
  // Rust 侧读文件，不受影响。
  const raw = await invoke("get_image_thumb", { id: id });
  // Rust 已保证 ≤512px；shrinkImage 保留为兜底（万一未来返回大图）
  const url = await shrinkImage(raw, IMAGE_MAX_EDGE).catch(() => raw);
  imageCache.set(id, url);
  if (imageCache.size > IMAGE_CACHE_MAX) {
    imageCache.delete(imageCache.keys().next().value);
  }
  return url;
}

/**
 * 缩略图：按需加载 + 视野外释放解码位图。
 *
 * 为什么必须做「释放」：渲染进程的内存大头不是 DOM 节点，而是**图片的解码位图**
 * （canvas 降采样后仍是 ≤512px 的位图，每张约 0.5–1MB；实测 54 张图滚过一遍后
 * 渲染进程专用工作集从 ~40MB 涨到 ~110MB）。Chromium 只要 <img> 还挂着 src，
 * 就会一直持有解码数据，所以必须主动清掉。
 *
 * 释放前先把图片盒子的实际尺寸写进行内样式：清掉 src 后盒子不会塌陷，
 * **布局与滚动条位置完全不变**，观感零影响。再次滚回来时从 imageCache 命中
 * dataURL（不再走 IPC/磁盘），重新解码即可。
 */
let imgObserver = null;
function observeImages(rootEl) {
  if (imgObserver) imgObserver.disconnect();
  imgObserver = new IntersectionObserver(
    (entries) => {
      for (const en of entries) {
        const img = en.target;
        if (!img.isConnected) continue;
        const state = img.dataset.imgLoaded;

        if (en.isIntersecting) {
          // 已加载 / 正在加载 → 无需处理
          if (state === "1" || state === "loading") continue;
          const id = img.dataset.imgId;
          if (!id) continue;
          img.dataset.imgLoaded = "loading";
          loadImage(id)
            .then((url) => {
              if (!img.isConnected) return;
              img.src = url;
              img.dataset.imgLoaded = "1";
            })
            .catch((e) => {
              img.dataset.imgLoaded = "0";
              // 失败两种可能：文件缺失/损坏（老行为，移除）；或图片超大被
              // get_image 拒绝（2.1.10 灾难条目，需保留占位让用户能选中删除）
              if (String(e).includes("过大")) {
                img.removeAttribute("src");
                img.textContent = "图片过大，无法显示\n可右键删除";
                img.classList.add("is-oversize");
              } else {
                img.remove(); // 文件缺失/损坏 → 不占位
              }
            });
        } else if (state === "1") {
          // 远离视野（>700px）→ 固化尺寸后释放解码位图
          const r = img.getBoundingClientRect();
          if (r.width > 0 && r.height > 0) {
            img.style.width = r.width + "px";
            img.style.height = r.height + "px";
          }
          img.removeAttribute("src");
          img.dataset.imgLoaded = "0";
        }
      }
    },
    { root: rootEl, rootMargin: "700px 0px" }
  );
  rootEl
    .querySelectorAll(".card-image[data-img-id]")
    .forEach((el) => imgObserver.observe(el));
}

// 浮层里三个内容槽常驻，按类型显示其中一个（切换时把另外两个清空，
// 避免上一次的文本 / 清单在 DOM 里留着）。
//
// 定位是**预告片**，不是替代「查看」：
//   文本只给 4 行、文件只给 3 个文件名 —— 够判断「是不是这条」就停，
//   想读全、想看完整路径就走右键「查看」。浮层把内容全摊开的话，
//   「查看」这个入口就白做了。
//
// 这两个数值只是**载荷护栏**，不是显示行数：正文真正被截到 4 行靠 CSS 的
// `-webkit-line-clamp`（见 .hp-text），否则一条长行折行后会变成十几行。
const PREVIEW_TEXT_GUARD_LINES = 40;
const PREVIEW_TEXT_GUARD_CHARS = 1200;
const PREVIEW_FILES_MAX = 3;

function ensurePreviewBox() {
  if (previewBox) return previewBox;
  const box = document.createElement("div");
  box.className = "hover-preview";
  box.hidden = true;
  const img = document.createElement("img");
  img.alt = "预览";
  const text = document.createElement("div");
  text.className = "hp-text";
  const files = document.createElement("ul");
  files.className = "hp-files";
  box.append(img, text, files);
  document.body.appendChild(box);
  previewBox = box;
  return box;
}

/**
 * 只留 kind 对应的内容槽，其余清空。
 *
 * 清空而不是只隐藏：图片槽留着上一次的 src 会一直占着解码后的位图内存，
 * 文本 / 清单留着也无意义。浮层是纯展示层，每次重建内容的成本可以忽略。
 */
function pickPreviewSlot(box, kind) {
  const img = box.querySelector("img");
  const text = box.querySelector(".hp-text");
  const files = box.querySelector(".hp-files");
  img.hidden = kind !== "image";
  text.hidden = kind !== "text";
  files.hidden = kind !== "files";
  if (kind !== "image") img.removeAttribute("src");
  if (kind !== "text") text.textContent = "";
  if (kind !== "files") files.replaceChildren();
  return { img, text, files };
}

function hideHoverPreview() {
  if (previewTimer) {
    clearTimeout(previewTimer);
    previewTimer = null;
  }
  if (previewBox && !previewBox.hidden) {
    previewBox.hidden = true;
    // 大图的 dataURL 松开引用，别让它在浮层里常驻
    previewBox.querySelector("img").removeAttribute("src");
  }
}

/** 浮层与卡片之间的间距 / 与窗口边缘的安全距离 */
const PREVIEW_GAP = 12;
const PREVIEW_EDGE = 8;

/**
 * 给浮层找位置：**四个方向挨个试，谁放得下就用谁**。
 *
 * 为什么不只做「右 → 左 → 夹回视口」：窗口只有 1060×690，卡片常常贴在列表底部。
 * 这时卡片右侧垂直方向根本放不下（`y + h` 会超出窗口），旧逻辑的做法是算出坐标
 * 再硬夹回视口 —— 结果是浮层压在卡片自己身上，用户正看着的那条被盖住了。
 *
 * 顺序按「离鼠标近、又不挡内容」排：
 *   右 → 左（与卡片垂直居中对齐，视线不用上下跳）
 *   → 下 → 上（横向与卡片左对齐；上下方向通常离鼠标更远，但至少不挡卡片）
 *   → 最后兜底：居中并夹进视口（宁可盖住一点也不跑到窗口外）
 *
 * 判定用「完整放得下」而不是「部分可见」：这个浮层是纯展示层，被裁掉一角就等于
 * 内容丢了，不像提示气泡那样可以容忍。
 */
function placePreview(box, card) {
  const r = card.getBoundingClientRect();
  const w = box.offsetWidth;
  const h = box.offsetHeight;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const midY = r.top + r.height / 2 - h / 2;
  const midX = r.left + r.width / 2 - w / 2;

  const candidates = [
    { x: r.right + PREVIEW_GAP, y: midY }, // 右
    { x: r.left - PREVIEW_GAP - w, y: midY }, // 左
    { x: r.left, y: r.bottom + PREVIEW_GAP }, // 下
    { x: r.left, y: r.top - PREVIEW_GAP - h }, // 上
    { x: midX, y: midY }, // 兜底
  ];

  const fits = ({ x, y }) =>
    x >= PREVIEW_EDGE &&
    y >= PREVIEW_EDGE &&
    x + w <= vw - PREVIEW_EDGE &&
    y + h <= vh - PREVIEW_EDGE;

  const pick = candidates.find(fits) || candidates[candidates.length - 1];
  const clamp = (v, min, max) => Math.max(min, Math.min(v, max));
  box.style.left = `${clamp(pick.x, PREVIEW_EDGE, Math.max(PREVIEW_EDGE, vw - w - PREVIEW_EDGE))}px`;
  box.style.top = `${clamp(pick.y, PREVIEW_EDGE, Math.max(PREVIEW_EDGE, vh - h - PREVIEW_EDGE))}px`;
}

/**
 * 卡片上已经显示全的内容不给浮层 —— 再弹一次同样的字只是噪音。
 *
 * 「是不是还有更多」一律问后端，前端不照抄一份判断规则：
 *   文本 → `preview_truncated`，就是后端 `text_preview_parts` 在生成卡片摘要时
 *          得出的「有没有丢弃内容」。照抄「2 行 / 140 字」到前端，日后改一边必漂移。
 *   文件 → 只有 1 个时卡片已经写着文件名了，浮层给不出任何新信息。
 *   图片 → 缩略图必然是缩过的，大图永远有增量。
 */
function hasMoreToPreview(item) {
  if (item.kind === "text") return !!item.preview_truncated;
  if (item.kind === "files") return (item.files || []).length > 1;
  return true;
}

function bindHoverPreview(card, item) {
  if (!hasMoreToPreview(item)) return;
  // 悬浮多久才弹浮层。
  //
  // 演进：120ms（一碰就弹，扫过列表连弹十几次，晃眼且白触发缩略图加载）
  //   → 2000ms（用户嫌长）→ 1000ms。
  // 1s 是个平衡点：快速扫过列表不会触发，停下来看一眼时也不觉得迟。
  // 图片 / 文本 / 文件统一 1s，同一份列表里弹得快慢不一会显得很随意。
  const HOVER_DELAY_MS = 1000;
  card.addEventListener("mouseenter", () => {
    if (previewTimer) clearTimeout(previewTimer);
    previewTimer = setTimeout(() => showHoverPreview(card, item), HOVER_DELAY_MS);
  });
  card.addEventListener("mouseleave", hideHoverPreview);
}

/**
 * 文本载荷护栏（**不是**显示截断）。
 *
 * 显示层面的「4 行 + 省略号」交给 CSS 的 `-webkit-line-clamp`：
 * 只有浏览器知道自己把一行折成了几个视觉行，JS 按 `\n` 数行数是数不准的
 * （一行十万字符的日志流会被算成「1 行」，照样撑爆浮层）。
 * 这里只做一件事 —— 别把几万字塞进 DOM 让排版计算变慢。
 */
function guardPreviewText(text) {
  let s = text;
  const lines = s.split("\n");
  if (lines.length > PREVIEW_TEXT_GUARD_LINES) {
    s = lines.slice(0, PREVIEW_TEXT_GUARD_LINES).join("\n");
  }
  if (s.length > PREVIEW_TEXT_GUARD_CHARS) {
    s = s.slice(0, PREVIEW_TEXT_GUARD_CHARS);
  }
  return s;
}

/** 取文件名：浮层只给名字，完整路径留给右键「查看」 */
function baseName(path) {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i >= 0 ? path.slice(i + 1) : path;
}

/** 文件清单：前 N 条**文件名**，其余折叠成一行「…另有 N 个」 */
function fillFilePreview(ul, item) {
  const all = Array.isArray(item.files) ? item.files : [];
  const lis = all.slice(0, PREVIEW_FILES_MAX).map((p) => {
    const li = document.createElement("li");
    li.textContent = baseName(p);
    return li;
  });
  if (all.length > PREVIEW_FILES_MAX) {
    const li = document.createElement("li");
    li.className = "hp-more";
    li.textContent = `…另有 ${all.length - PREVIEW_FILES_MAX} 个`;
    lis.push(li);
  }
  // 极老的条目可能没有 files 字段 → 退回卡片上那条摘要，至少不是个空浮层
  if (lis.length === 0) {
    const li = document.createElement("li");
    li.textContent = item.preview || "";
    lis.push(li);
  }
  ul.replaceChildren(...lis);
}

async function showHoverPreview(card, item) {
  previewTimer = null;
  if (!card.isConnected || !card.matches(":hover")) return;

  const box = ensurePreviewBox();

  if (item.kind === "image") {
    let url;
    try {
      url = await loadImage(item.id);
    } catch {
      return; // 图片文件缺失/损坏 → 不弹预览
    }
    // 读取期间鼠标可能已移开
    if (!card.isConnected || !card.matches(":hover")) return;
    const { img } = pickPreviewSlot(box, "image");
    // 图片尺寸只有加载完才准，加载后重新定位一次
    img.onload = () => {
      if (!box.hidden) placePreview(box, card);
    };
    img.src = url;
    box.hidden = false;
    placePreview(box, card);
    return;
  }

  if (item.kind === "files") {
    const { files } = pickPreviewSlot(box, "files");
    fillFilePreview(files, item);
    box.hidden = false;
    placePreview(box, card);
    return;
  }

  // 文本：卡片上只有 2 行摘要，这里多给几行。
  // 必须读全文 —— `item.preview` 本身就是截断过的摘要，拿它渲染等于把卡片内容重复一遍。
  let data;
  try {
    data = await invoke("get_full_content", { id: item.id });
  } catch {
    return;
  }
  if (!card.isConnected || !card.matches(":hover")) return;
  const { text } = pickPreviewSlot(box, "text");
  text.textContent = guardPreviewText(data.text || "");
  box.hidden = false;
  placePreview(box, card);
}

/// `plain = true` 表示「只要文字、不要格式」。
///
/// 默认 `false` 走原格式：用户当初复制的是带格式的内容，粘回去理应是带格式的。
/// 纯文本入口放在右键菜单里（记事本、终端、代码编辑器这类场景才需要）。
async function copyBack(item, plain = false) {
  try {
    // copy_back 返回可选的提示串（文件条目：部分原文件已失效；文本条目：用了哪种格式）
    const warn = await invoke("copy_back", { id: item.id, plain });
    toast(warn || "已复制到剪贴板");
  } catch (e) {
    toast("复制失败：" + e);
  }
  // 复制完立刻重查这一条：原文件可能刚被删或挪走，而列表上的「已失效」是**渲染时**
  // 算出来的 —— 不在这里补一刀，用户看到的就是「提示已经不存在，但卡片上没有任何标记」，
  // 两个信息互相打架，还得手动触发重绘才一致。
  // 部分失效（只跳过其中一个）同样要标，因为失效的判定本来就是「有一个不存在就算」。
  if (item.kind === "files") await recheckMissing([item.id]);
}

// ---------------------------------------------------------------------------
// 文件条目失效检测
//
// 文件条目存的是**路径引用**（files/<id>.txt），不是文件副本。原文件被移动、改名或
// 删除后，条目还在，但粘贴不出来。这里在渲染后异步查一次存在性，给失效的卡片打标记。
// 只做 exists()（元数据查询，不读内容），开销可以忽略。
// ---------------------------------------------------------------------------

let missingFiles = new Set();

async function refreshMissing(list) {
  const ids = list.filter((i) => i.kind === "files").map((i) => i.id);
  if (ids.length === 0) {
    if (missingFiles.size) {
      missingFiles = new Set();
      applyMissingMarks();
    }
    return;
  }
  try {
    missingFiles = new Set(await invoke("check_files_alive", { ids }));
  } catch {
    return; // 查询失败就不标记，不能因此打断列表渲染
  }
  applyMissingMarks();
}

/**
 * 只重查这几个 id，把结论并进 missingFiles 并**增量**刷新徽章。
 *
 * 与 `refreshMissing` 的分工：那个是「整屏重算」，只在渲染之后跑一次；
 * 这个是「事后补一刀」，用于复制失败这类**刚刚才知道**状态变了的情形。
 *
 * 走增量而不是整屏重渲：用户正看着列表，不能因为一次复制失败就把滚动位置顶掉 ——
 * 这也正是「无感」的含义，标记自己浮出来，画面不动。
 */
async function recheckMissing(ids) {
  if (ids.length === 0) return;
  let dead;
  try {
    dead = await invoke("check_files_alive", { ids });
  } catch {
    return; // 查询失败就不动标记，不能因此打断复制流程
  }
  const deadSet = new Set(dead);
  for (const id of ids) {
    if (deadSet.has(id)) missingFiles.add(id);
    else missingFiles.delete(id); // 文件又回来了（撤销删除/移回来）也要能恢复
  }
  applyMissingMarks();
}

/** 把失效状态同步到已渲染的卡片上（新追加的批次也要过一遍） */
function applyMissingMarks() {
  document.querySelectorAll(".card").forEach((el) => {
    const missing = missingFiles.has(el.dataset.id);
    el.classList.toggle("is-missing", missing);
    const meta = el.querySelector(".card-meta");
    if (!meta) return;
    const tag = meta.querySelector(".tag-missing");
    if (missing && !tag) {
      const s = document.createElement("span");
      s.className = "tag tag-missing";
      s.textContent = "已失效";
      // 必须插到时间**之前**：时间是 margin-left:auto 推到最右的，
      // 若用 appendChild 它会落到时间右边，破坏「徽章在左、时间靠右」的布局。
      const time = meta.querySelector(".card-time");
      if (time) meta.insertBefore(s, time);
      else meta.appendChild(s);
    } else if (!missing && tag) {
      tag.remove();
    }
  });
}

// ---------------------------------------------------------------------------
// 右键上下文菜单
// ---------------------------------------------------------------------------

let ctxMenuEl = null;

/** 右键菜单图标（16×16 描边风格，与侧栏 .nav-icon 同一套语言）。
 *  颜色不写死 —— stroke 用 currentColor，于是危险项的红色图标自动跟着 `.danger` 走。 */
const CTX_ICONS = {
  // 眼睛 = 查看
  view: '<svg viewBox="0 0 16 16"><path d="M1.6 8s2.4-4.6 6.4-4.6S14.4 8 14.4 8s-2.4 4.6-6.4 4.6S1.6 8 1.6 8z"/><circle cx="8" cy="8" r="2"/></svg>',
  // 两张叠起来的纸 = 复制
  copy: '<svg viewBox="0 0 16 16"><rect x="5.6" y="5.6" width="8.4" height="8.4" rx="1.6"/><path d="M10.4 5.6V3.6A1.6 1.6 0 008.8 2H3.6A1.6 1.6 0 002 3.6v5.2a1.6 1.6 0 001.6 1.6h2"/></svg>',
  // 铅笔 = 编辑
  edit: '<svg viewBox="0 0 16 16"><path d="M2.2 13.8l.9-3.3 7.2-7.2a1.5 1.5 0 012.4 0l1 1a1.5 1.5 0 010 2.4l-7.2 7.2z"/><path d="M9.4 4.1l2.5 2.5"/></svg>',
  // 图钉 = 置顶
  pin: '<svg viewBox="0 0 16 16"><path d="M9.6 1.6l4.8 4.8-1.3 1.2-.7-.2-2.3 2.3.3 2.5-1.5 1.5-2.2-2.2-3.2 3.2-.9-.9 3.2-3.2-2.2-2.2 1.5-1.5 2.5.3 2.3-2.3-.2-.7z"/></svg>',
  // 星星 = 收藏
  star: '<svg viewBox="0 0 16 16"><path d="M8 1.9l1.9 4.2 4.4.5-3.3 3 .9 4.5L8 11.9l-3.9 2.2.9-4.5-3.3-3 4.4-.5z"/></svg>',
  // 方框打勾 = 批量选中
  select:
    '<svg viewBox="0 0 16 16"><rect x="2.2" y="2.2" width="11.6" height="11.6" rx="2.4"/><path d="M5.4 8.2l1.9 1.9 3.5-4"/></svg>',
  // 垃圾桶 = 删除
  trash:
    '<svg viewBox="0 0 16 16"><path d="M2.5 4h11M6.5 2h3M4 4l.7 9a1.5 1.5 0 001.5 1.4h3.6a1.5 1.5 0 001.5-1.4L12 4M6.7 7v4.5M9.3 7v4.5"/></svg>',
};

function closeContextMenu() {
  if (ctxMenuEl) {
    ctxMenuEl.remove();
    ctxMenuEl = null;
  }
}

/** 通用浮层菜单。entries = [{ label, act, icon?, danger?, disabled? } | { sep: true }] */
function showMenu(x, y, entries) {
  closeContextMenu();

  const menu = document.createElement("div");
  menu.className = "ctx-menu";

  for (const entry of entries) {
    if (entry.sep) {
      const sep = document.createElement("div");
      sep.className = "ctx-sep";
      menu.appendChild(sep);
      continue;
    }
    const btn = document.createElement("button");
    btn.className = "ctx-item" + (entry.danger ? " danger" : "");
    if (entry.icon) {
      const ic = document.createElement("span");
      ic.className = "ctx-icon";
      ic.innerHTML = entry.icon;
      btn.appendChild(ic);
    }
    // 文字单独占一格（flex:1）：这样整行都是可点区域，长文案也不会把图标挤扁
    const label = document.createElement("span");
    label.className = "ctx-label";
    label.textContent = entry.label;
    btn.appendChild(label);
    btn.disabled = !!entry.disabled;
    btn.onclick = () => {
      closeContextMenu();
      entry.act();
    };
    menu.appendChild(btn);
  }

  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  menu.style.left = Math.max(4, Math.min(x, window.innerWidth - rect.width - 8)) + "px";
  menu.style.top = Math.max(4, Math.min(y, window.innerHeight - rect.height - 8)) + "px";
  ctxMenuEl = menu;
}

// ---------------------------------------------------------------------------
// 全文详情弹窗（查看态 + 编辑态）
//
// 起因：列表只显示 preview（**开头几行的摘要**），长文本根本看不全。
// 方案参考 CopyQ 的「Show Item Content」：用一个独立窗口显示全文。
// 正文用 <pre> 保留换行与缩进，并且可选中 —— 这个弹窗的主要用途就是
// 「读一遍」或「挑一段复制」，两者都要求格式不乱。
//
// 编辑按「**编辑副本**」设计：保存 = 写剪贴板，**不改历史里那条记录**。
// 理由：历史必须忠实反映「当初复制了什么」，能原地改会让人怀疑
// 「我明明复制的是 A，怎么变成 B 了」；且剪贴板历史常被当安全网，改动不可逆是大事。
// ---------------------------------------------------------------------------

let detailItem = null;
/// 编辑缓冲的**原始内容**：用于「取消编辑」与判断是否有未保存改动
let detailOriginal = "";
/// 后端是否因过长而截断了正文。截断时**禁止编辑** —— 否则保存会把
/// 截断后的内容写回剪贴板，等于静默丢数据。
let detailTruncated = false;
/// 弹窗是不是**直接以编辑态打开的**（右键「编辑」进来）。
///
/// 决定「取消编辑」落到哪里：从查看态点「编辑」进来的，取消应当退回查看态
/// （刚才还在读，退回去才顺）；一进来就是编辑态的，压根没有「刚才读到的画面」
/// 可回，再退回查看态等于凭空多出一屏用户没要的内容 —— 所以取消直接关窗。
let detailEnteredInEdit = false;

const $detail = (id) => document.getElementById(id);

/** 当前详情条目能否编辑：只有文本可以。
 *
 * 与右键菜单同一条规则，两处必须一致 —— 菜单里不给「编辑」的文件条目，
 * 若在「查看」弹窗里又冒出个编辑按钮，等于留了条小路绕过规则。
 * 根因：文件条目存的是路径清单，编辑后写回剪贴板会退化成一段纯文本，
 * 粘出去不再是一批文件，语义整个丢了。 */
function canEditDetail() {
  return detailItem?.kind === "text";
}

/** 查看态 / 编辑态互斥切换 */
/// 当前详情弹窗的上下文（退出编辑态要重放渲染）
let detailCtx = null;

/// 按格式渲染详情正文，返回是否用了「排版」而不是纯文本。
///
/// 优先级刻意如此：
///   Markdown 排版 > 富文本白名单 > 代码高亮 > 纯文本
/// Markdown 条目即使带着 html 也优先排版源码 —— 用户的原话是「查看里怎么还是一堆
/// markdown 原符号」，他要的就是渲染后的样子。
/// 图片未显示时的说明条。
///
/// 三种原因分开讲，因为用户的处置方式完全不同：
///   external —— 网页外链图，我们不替用户发请求（隐私）；想保留就回原网页另存为
///   too-large —— 单张太大，超过保留上限
///   missing   —— 文件丢了（被清理或手动删过）
function richImageNote(drops) {
  const count = (k) => drops.filter((d) => d === k).length;
  const ext = count("drop-img:external");
  const big = count("drop-img:too-large");
  const lost = count("drop-img:missing");
  if (!ext && !big && !lost) return null;

  const box = document.createElement("div");
  box.className = "detail-note";

  const head = document.createElement("strong");
  const facts = [];
  if (ext) facts.push(`${ext} 张外链图片未显示`);
  if (big) facts.push(`${big} 张图片体积过大未保留`);
  if (lost) facts.push(`${lost} 张图片文件已不存在`);
  head.textContent = facts.join("，").trim();
  box.appendChild(head);

  const why = document.createElement("div");
  why.className = "detail-note-why";
  const lines = [];
  if (ext) {
    lines.push("外链图片来自网页。轻贴不会替你访问这些地址——那样等于告诉对方服务器你在读什么。");
    lines.push("需要这些图时：在原网页上右键图片 →「图片另存为」保存到本地，再把本地文件复制进轻贴。");
  }
  if (big) lines.push("过大的图片不落盘（单张上限约 3MB），以免一条记录占掉几十 MB。");
  if (lost) lines.push("图片文件可能已被清理或手动删除；文字与排版仍在，复制出去不会带图。");
  why.textContent = lines.join(" ");
  box.appendChild(why);

  return box;
}

function renderDetailBody(item, data) {
  const body = $detail("detail-body");
  if (!body || !data) return false;
  body.className = "detail-body";
  body.replaceChildren();

  const sub = item?.subtype;

  if (sub === "markdown" && data.text) {
    body.classList.add("is-rich");
    // renderMarkdown 内部对每一段都做了转义，可以安全赋给 innerHTML
    body.innerHTML = renderMarkdown(data.text);
    return true;
  }

  if (data.html) {
    const drops = [];
    // 内联图在存储时被拆成独立文件（`data-qt-img="n"`），这里按编号取回 data URL
    const richMap = new Map(Object.entries(data.rich_images || {}));
    const frag = sanitizeRichHtml(data.html, (kind) => drops.push(kind), richMap);
    // 丢弃项汇总成一条日志（复用现成的前端上报通道，不另造一套）。
    // 逐项上报会让一条内容刷几十行，把真正有用的上下文挤掉。
    if (drops.length) {
      const kinds = [...new Set(drops)].slice(0, 5).join(",");
      reportFrontend("WARN", "format", `富文本过滤丢弃 ${drops.length} 项：${kinds}`);
    }
    // 片段为空不能直接返回：那会摆出一个空白面板，用户以为内容丢了。
    // 典型来源是「只有 CF_HTML 头、正文缺失」的坏数据 —— 落到下面按纯文本显示，
    // 至少还能看到文字。
    if (frag.childNodes.length > 0) {
      body.classList.add("is-rich");
      body.appendChild(frag);
      // 有图没显示出来，要说明「为什么」以及「怎么办」——
      // 否则用户只会觉得「这软件把图弄丢了」。
      const note = richImageNote(drops);
      if (note) body.insertBefore(note, body.firstChild);
      return true;
    }
  }

  if (sub === "code") {
    body.classList.add("is-rich");
    body.innerHTML = `<pre class="md-code"><code>${highlightCode(data.text)}</code></pre>`;
    return true;
  }

  body.textContent = data.text;
  return false;
}

function setDetailMode(mode) {
  const editing = mode === "edit";
  $detail("detail-body").hidden = editing;
  $detail("detail-editor").hidden = !editing;
  $detail("detail-select").hidden = editing;
  // 不可编辑的条目连「编辑」按钮都不给，且**退出编辑态时也不能重新冒出来**
  $detail("detail-edit").hidden = editing || !canEditDetail();
  $detail("detail-copy").hidden = editing;
  $detail("detail-copy-plain").hidden = editing;
  $detail("detail-close").hidden = editing;
  $detail("detail-revert").hidden = !editing;
  $detail("detail-save").hidden = !editing;
  if (editing) {
    $detail("detail-editor").focus();
  } else {
    // 退出编辑时丢弃改动，视图回到原文
    $detail("detail-editor").value = detailOriginal;
    // 必须走渲染而不是直接塞纯文本：否则「编辑一次再退出」，Markdown / 富文本的
    // 排版就退回成源码，用户会以为渲染只生效一次。
    if (!detailCtx || !renderDetailBody(detailCtx.item, detailCtx.data)) {
      $detail("detail-body").textContent = detailOriginal;
    }
    window.getSelection()?.removeAllRanges();
    $detail("detail-select").textContent = "全选";
  }
}

function isDetailEditing() {
  const ed = $detail("detail-editor");
  return !!ed && !ed.hidden;
}

function enterDetailEdit() {
  if (!detailItem || !canEditDetail()) return;
  // 截断的内容不能编辑：保存会把截断结果写回去 → 静默丢数据
  if (detailTruncated) {
    toast("内容过长，不便在此编辑；可先「复制全文」到别处处理");
    return;
  }
  setDetailMode("edit");
}

async function openDetail(item, { startEdit = false } = {}) {
  const mask = $detail("detail-modal");
  const body = $detail("detail-body");
  const meta = $detail("detail-meta");
  const title = $detail("detail-title");
  if (!mask || !body || !meta || !title) return;

  title.textContent = item.kind === "files" ? "全部路径" : "全文";
  meta.textContent = "读取中…";
  body.textContent = "";

  let data;
  try {
    data = await invoke("get_full_content", { id: item.id });
  } catch (err) {
    toast("读取失败：" + err);
    return;
  }

  detailItem = item;
  detailOriginal = data.text;
  detailTruncated = !!data.truncated;
  detailCtx = { item, data };
  renderDetailBody(item, data);
  $detail("detail-editor").value = data.text;

  // 规模信息：帮助判断内容大小，也是「有没有被截断」的唯一提示
  const parts = [
    `共 ${data.totalChars.toLocaleString()} 字`,
    `${data.totalLines.toLocaleString()} 行`,
  ];
  if (data.truncated) parts.push("超出显示上限，仅显示前 20 万字");
  meta.textContent = parts.join(" · ");

  setDetailMode("view");
  mask.hidden = false;
  body.scrollTop = 0;
  // 聚焦正文：用户多半想紧接着 Ctrl+A / Ctrl+C 或直接滚动
  body.focus();
  // 每次打开都回到「未选中」状态 —— 上一次的选区可能还残留，
  // 那样按钮文案会显示成「取消全选」，与实际不符。
  window.getSelection()?.removeAllRanges();
  $detail("detail-select").textContent = "全选";

  // 右键「编辑」→ 打开即进入编辑态，省一次点击
  if (startEdit) enterDetailEdit();
  // 以**实际结果**为准：内容超长时 enterDetailEdit 会拒绝并停在查看态，
  // 那种情况不能算「以编辑态打开」，否则取消编辑会把窗口直接关掉。
  detailEnteredInEdit = isDetailEditing();
}

/**
 * 取消编辑：回到「进来时的样子」。
 *
 * 从查看态点「编辑」进来的 → 退回查看态（用户刚才在阅读，退回去是连续的）；
 * 右键「编辑」直接进来的 → 关窗（没有阅读上下文可回，凭空显示一屏查看界面
 * 反而像多走了一步）。
 */
function cancelDetailEdit() {
  if (detailEnteredInEdit) {
    closeDetail();
    return;
  }
  setDetailMode("view");
}

/**
 * 「请求关闭」：查看态下直接关；编辑态下要看是怎么进来的。
 *
 * - 从查看态进入编辑 → **先退回查看态**，第二次才关。编辑缓冲里可能有敲了一半的
 *   内容，直接关等于静默丢弃；多一道确认比丢内容好。
 * - 右键「编辑」直接进来 → 直接关。用户本来就没在看，关掉才是他要的，
 *   此时「先退回查看态」会平白多出一屏他没要的内容。
 */
function requestCloseDetail() {
  if (isDetailEditing() && !detailEnteredInEdit) {
    setDetailMode("view");
    return;
  }
  closeDetail();
}

function closeDetail() {
  const mask = $detail("detail-modal");
  if (mask && !mask.hidden) {
    mask.hidden = true;
    detailItem = null;
    detailOriginal = "";
    detailTruncated = false;
    detailEnteredInEdit = false;
  }
}

function bindDetailModal() {
  const mask = document.getElementById("detail-modal");
  const body = document.getElementById("detail-body");
  const btnSelect = document.getElementById("detail-select");
  if (!mask || !body || !btnSelect) return;

  document.getElementById("detail-close").onclick = requestCloseDetail;

  // 点遮罩关闭（点弹窗本体不关）——这是「查看类」弹窗的通用预期。
  // 编辑态下是否直接关，交给 requestCloseDetail 按「怎么进来的」判断。
  mask.addEventListener("mousedown", (e) => {
    if (e.target === mask) requestCloseDetail();
  });

  document.getElementById("detail-edit").onclick = enterDetailEdit;
  document.getElementById("detail-revert").onclick = cancelDetailEdit;

  document.getElementById("detail-save").onclick = async () => {
    const text = $detail("detail-editor").value;
    try {
      // 保存 = 写剪贴板 + **改过的内容记一条新条目**，原条目原样不动（编辑副本原则）。
      // 历史必须忠实反映「当初复制了什么」，所以不允许原地改原条目；
      // 但编辑后的内容若在历史里找不到就等于白改，因此落成新条目并带「已编辑」徽章。
      const changed = text !== detailOriginal;
      await invoke("set_clipboard_text", { text, changed });
      // 一个字没改就保存 ≡ 复制原文：只写剪贴板，不记历史，避免多出一模一样的一条。
      toast(changed ? "已保存并记入历史" : "已保存");
      closeDetail();
    } catch (err) {
      toast("保存失败：" + err);
    }
  };

  // 渲染出来的链接（Markdown / 富文本）用事件委托交给系统浏览器。
  // WebView 内的 <a href> 会被当作应用内导航（点了直接白屏），必须拦下 ——
  // 与关于页更新日志里的链接同一套处理。
  document.getElementById("detail-body")?.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-ext]");
    if (!a) return;
    e.preventDefault();
    e.stopPropagation();
    invoke("open_external", { url: a.dataset.ext });
  });

  // 「复制纯文本」：与右键菜单里的同名项走**同一条命令**（copy_back + plain），
  // 不另写一套读写逻辑 —— 否则格式处理规则会在两处漂移。
  document.getElementById("detail-copy-plain").onclick = async () => {
    if (!detailItem) return;
    try {
      await invoke("copy_back", { id: detailItem.id, plain: true });
      toast("已复制纯文本");
    } catch (e) {
      toast("复制失败：" + e);
    }
  };

  document.getElementById("detail-copy").onclick = async () => {
    if (!detailItem) return;
    const id = detailItem.id;
    try {
      // ⚠️ 走 copy_back：它在 Rust 侧**直接读盘**写剪贴板，不经过前端，
      // 所以即使上面的正文因为超长被截断显示，复制出去的仍是**完整原文**。
      const hint = await invoke("copy_back", { id, plain: false });
      toast(hint || "已复制全文");
      closeDetail();
    } catch (err) {
      toast("复制失败：" + err);
    }
  };

  // 「全选」是**切换**语义：全选后文案变「取消全选」，再点即取消选中。
  //
  // ⚠️ 状态**不靠布尔量硬记**。用户随时会手动拖选、或点正文别处取消选区，
  // 硬记的标记必然与真实选区失配（以为还在「全选」态，其实早变了）。
  // 直接问「当前选区是不是正好等于整个正文」—— O(1) 且永远准确：
  // 正文是单个文本节点，全选时 range 落在 <pre> 自身、起点 0、终点 = 子节点数。
  const isAllSelected = () => {
    const sel = window.getSelection();
    if (!sel || sel.rangeCount !== 1 || !body.childNodes.length) return false;
    const r = sel.getRangeAt(0);
    return (
      r.startContainer === body &&
      r.startOffset === 0 &&
      r.endContainer === body &&
      r.endOffset === body.childNodes.length
    );
  };
  const syncSelectLabel = () => {
    btnSelect.textContent = isAllSelected() ? "取消全选" : "全选";
  };

  // ⚠️ 点按钮会移动焦点、从而清掉选区 —— 必须在 mousedown 阶段
  // preventDefault 把它按住。否则「取消全选」会点了没反应：
  // 选区在 click 触发之前就已经被清空了，isAllSelected() 永远读到 false。
  btnSelect.addEventListener("mousedown", (e) => e.preventDefault());
  btnSelect.onclick = () => {
    if (isAllSelected()) {
      window.getSelection().removeAllRanges();
    } else {
      const range = document.createRange();
      range.selectNodeContents(body);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      body.focus();
    }
    syncSelectLabel();
  };

  // 手动拖选 / 点击别处改动选区时，同步按钮文案
  document.addEventListener("selectionchange", () => {
    if (!mask.hidden) syncSelectLabel();
  });
}

function openContextMenu(x, y, item) {
  const pinnedHere = isPinnedIn(item, currentView);
  const viewName = { history: "历史", text: "文本", images: "图片", files: "文件", favorites: "收藏" }[currentView] || currentView;
  // 带同步标记的条目是互通产生的「同步置顶」，它只有有/没有两个状态：取消即整条取消。
  // 文案不能带分类名，否则看起来像只动了当前分类。
  const synced = Array.isArray(item.pinnedViews) && item.pinnedViews.includes(PIN_ALL);
  const pinLabel = synced
    ? settings?.pinGlobal
      ? "取消置顶"
      : "取消同步置顶"
    : (pinnedHere ? "取消置顶" : "置顶") + (settings?.pinGlobal ? "" : `（${viewName}）`);
  // 菜单分组（顺序按「使用频次 + 语义亲疏」排，见分组间的分隔线）：
  //   一、阅读与复制：查看 → 复制 → 编辑
  //       「编辑」是从「复制」派生出来的动作，必须紧挨着「复制」，
  //       中间隔了别的项会让用户以为是另一类操作。
  //   二、状态变更：置顶（改排序）→ 收藏（改归属）
  //   三、危险操作：批量删除 → 删除
  // 文案只留动词：「查看」「编辑」本身就够清楚，加「全文」「全部路径」「并复制」
  // 只是把同一件事说得更长，菜单位宽反而被撑开（2026-10-23 用户要求精简）。
  const entries = [];
  if (item.kind === "text" || item.kind === "files") {
    entries.push({
      label: "查看",
      icon: CTX_ICONS.view,
      act: () => openDetail(item),
    });
  }
  // 「复制」对所有类型都有意义，永远排在阅读项之后、置顶之前。
  // 文案就是「复制」——不带「保留格式」这类前缀：主路径本来就是保留格式，
  // 不需要在菜单里解释一遍（用户 2026-10-26 要求去掉）。
  entries.push({
    label: "复制",
    icon: CTX_ICONS.copy,
    act: () => copyBack(item, false),
  });
  // 「编辑」只给文本条目。
  // 文件条目的内容是**路径清单**，编辑它再写回剪贴板会退化成一段纯文本
  // （文件语义丢失，粘贴出去不再是文件）—— 那不是用户要的。
  if (item.kind === "text") {
    entries.push({
      label: "编辑",
      icon: CTX_ICONS.edit,
      act: () => openDetail(item, { startEdit: true }),
    });
  }
  // 「复制为纯文本」只对带格式的文本条目出现，位置紧跟「编辑」——
  // 它和「复制」是同一件事的两种结果，让它们分处「编辑」两侧是刻意的：
  // 主路径在上、降级路径在下，中间隔着用户最常用的编辑入口。
  const rich = item.kind === "text" && (item.html_path || item.rtf_path);
  if (rich) {
    entries.push({
      label: "复制为纯文本",
      icon: CTX_ICONS.copy,
      act: () => copyBack(item, true),
    });
  }
  entries.push({ sep: true });
  entries.push(
    {
      label: pinLabel,
      icon: CTX_ICONS.pin,
      act: async () => {
        // 新集合由后端按当前模式算好回传，前端不再重复实现一遍置顶规则
        item.pinnedViews = await invoke("toggle_pin", { id: item.id, view: currentView });
        render({ keepScroll: true });
      },
    },
    {
      label: item.favorite ? "取消收藏" : "收藏",
      icon: CTX_ICONS.star,
      act: async () => {
        await invoke("toggle_favorite", { id: item.id });
        item.favorite = !item.favorite;
        render({ keepScroll: true });
      },
    }
  );
  // 危险操作单独成组：低频，但一旦点错代价最大，分组能靠肌肉记忆避开
  entries.push({ sep: true });
  entries.push(
    {
      // 批量删除是个模式：进入后左键改为选中，再点一次同一项即取消选中
      label: selectMode ? "退出批量选中" : "批量删除",
      icon: CTX_ICONS.select,
      act: () => (selectMode ? exitSelectMode() : enterSelectMode()),
    },
    {
      label: "删除",
      icon: CTX_ICONS.trash,
      danger: true,
      act: async () => {
        const purge = await askPurgeFiles([item]);
        const note = await invoke("delete_item", {
          id: item.id,
          deleteLocalFiles: purge,
        });
        items = items.filter((i) => i.id !== item.id);
        render({ keepScroll: true });
        if (note) toast(note);
      },
    }
  );
  showMenu(x, y, entries);
}

// 点空白处 / 滚动 / Esc → 关闭；非卡片区域右键 → 只关不弹
document.addEventListener("click", closeContextMenu);
document.addEventListener(
  "scroll",
  () => {
    closeContextMenu();
    hideHoverPreview();
  },
  true
);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    // 功能导览是最上层的阻断式遮罩，优先级最高；Esc 等同于「跳过」
    if (!document.getElementById("feature-tour")?.hidden) {
      closeFeatureTour();
      return;
    }
    // 详情弹窗次之：编辑态是否直接关，由 requestCloseDetail 按「怎么进来的」决定
    if (!document.getElementById("detail-modal")?.hidden) {
      requestCloseDetail();
      return;
    }
    closeContextMenu();
    hideHoverPreview();
    exitSelectMode();
  }
});
document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  closeContextMenu();
});

let toastTimer = null;
function toast(msg) {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1800);
}

// ---------------------------------------------------------------------------
// 设置页
// ---------------------------------------------------------------------------

// 设置页的分类。原先 10 个配置项 + 缓存块 + 危险按钮平铺成一长条，既杂又
// 容易被 .view 的 overflow:hidden 裁掉（且无法滚动）。改成分页后每页只显示一组，
// 组内仍保持平铺（不折叠）—— 视觉密度下来了，但不会多出「先点开才能改」这一步。
//
// 「记录」已并入「记录与缓存」：记什么（文件复制）→ 留多久（保留天数）→ 存在哪
// （缓存目录）→ 怎么清（清理缓存 / 清空全部），本就是同一条链路，拆成两页反而跳。
const SETTINGS_TABS = [
  { id: "general", label: "常规" },
  { id: "appearance", label: "外观" },
  { id: "data", label: "数据" },
  { id: "about", label: "关于" },
];

function settingsHtml() {
  const s = settings || {};
  // 有新版时「关于」标签本身带一个小红点（侧栏的「设置」红点在 applyUpdateState 里处理）
  const hasUpd = !!updateInfo?.hasUpdate;
  const dotTip = hasUpd ? `有新版本 v${updateInfo.latest}，点击查看` : "";
  const tabBar = `<div class="settings-tabs">${SETTINGS_TABS.map(
    (t) => {
      const dot = t.id === "about" && hasUpd;
      return (
        `<button class="tab${t.id === settingsTab ? " is-active" : ""}` +
        `${dot ? " has-dot" : ""}" data-tab="${t.id}"` +
        `${dot ? ` title="${esc(dotTip)}"` : ""}>${t.label}</button>`
      );
    }
  ).join("")}</div>`;

  // 只渲染当前分类 —— 其余分类的控件这一刻**不在 DOM 里**，所以 bindSettings 里
  // 每一个 querySelector 都必须做存在性保护，否则会 null.onclick / null.onchange 报错。
  let body = "";
  if (settingsTab === "appearance") body = settingsAppearanceHtml(s);
  else if (settingsTab === "about") body = settingsAboutHtml();
  // "record" 已并入 "data"；带上这个旧值兜底，万一 settingsTab 还是它，
  // 也不会停在已经不存在的分类上。
  else if (settingsTab === "data" || settingsTab === "record")
    body = settingsDataHtml(s);
  else body = settingsGeneralHtml(s);

  return `<div class="settings">
    <h2>设置</h2>
    ${tabBar}
    <div class="settings-body">${body}</div>
  </div>`;
}

// ── 设置页的排版基元 ──────────────────────────────────────────────────────
// 设置页一律「区块（卡片）→ 行 → 标题 + 说明 + 右侧控件」三段式。
// 抽成这三个函数而不是每个模板手写：一页十几行，手写必然出现某行少了 row-text、
// 某个 label 忘了 for、某个子项忘了 is-sub —— 这类不一致正是上一版看起来"乱"的原因。
//
// `sub = true` 表示这一项从属于上一项（缩进 + 左侧竖线），取代原来用全角空格
// 「　└ 」伪装层级的写法。
function rowText(title, desc, id) {
  const t = id
    ? `<label class="row-title" for="${id}">${title}</label>`
    : `<span class="row-title">${title}</span>`;
  return `<div class="row-text">${t}${desc ? `<div class="row-desc">${desc}</div>` : ""}</div>`;
}

function rowCheck(id, title, desc, checked, sub = false) {
  return `<div class="row${sub ? " is-sub" : ""}">${rowText(title, desc, id)}<div class="row-ctrl"><input type="checkbox" id="${id}" ${checked ? "checked" : ""} /></div></div>`;
}

/// 右侧是自定义控件（分段控件 / 按钮 / 数字框 / 滑块）的行
function rowCtl(title, desc, ctl, sub = false, id) {
  return `<div class="row${sub ? " is-sub" : ""}">${rowText(title, desc, id)}<div class="row-ctrl">${ctl}</div></div>`;
}

function block(title, inner, opts = {}) {
  const cls = ["block", opts.danger ? "is-danger" : ""].filter(Boolean).join(" ");
  return `<section class="${cls}">${title ? `<h3 class="block-h">${title}</h3>` : ""}${inner}</section>`;
}

function settingsGeneralHtml(s) {
  return [
    block(
      "启动",
      rowCheck("set-autostart", "开机自启", "开机后自动运行", s.autoStart) +
        rowCheck(
          "set-tray",
          "关闭时收进托盘",
          "关掉窗口后在后台继续记录；右键托盘图标可打开菜单",
          s.trayOnClose
        )
    ),
    block(
      "置顶",
      rowCheck(
        "set-pin-global",
        "置顶互通",
        "任一分类置顶或取消，其它分类同步；关闭后各分类独立记录",
        s.pinGlobal
      ) +
        rowCheck(
          "set-pin-cancel",
          "关闭互通时取消同步置顶",
          "勾选则关掉互通时连同它产生的同步置顶一起取消；不勾选则永久保留",
          s.pinCancelOnDisable,
          true
        )
    ),
    block(
      "帮助",
      rowCtl(
        "功能导览",
        "随时可以再看一遍这 7 步说明",
        `<button class="btn-ghost" id="replay-tour">重新查看</button>`
      )
    ),
  ].join("");
}

function settingsAppearanceHtml(s) {
  return [
    block(
      "主题",
      rowCtl(
        "深浅模式",
        "「系统」跟随 Windows 的深浅色设置；侧栏底部也有一键切换",
        `<div class="seg" id="set-theme">
          <button type="button" data-theme="light" ${s.themeMode === "light" ? 'class="on"' : ""}>浅色</button>
          <button type="button" data-theme="dark" ${s.themeMode === "dark" ? 'class="on"' : ""}>深色</button>
          <button type="button" data-theme="system" ${(s.themeMode || "system") === "system" ? 'class="on"' : ""}>系统</button>
        </div>`
      )
    ),
    block(
      "窗口",
      rowCtl(
        "材质浓度",
        "越高越厚实 · 越低越通透",
        `<input type="range" id="set-material" min="10" max="90" value="${s.materialAlpha ?? 50}" /><span class="hint" id="material-val">${s.materialAlpha ?? 50}%</span>`
      ) +
        rowCheck(
          "set-round",
          "圆角模式",
          "圆角 + 无背景模糊（四角干净）／直角 + 保留背景模糊。Win11 本身即圆角，此开关只影响背景模糊与观感",
          s.roundMode
        )
    ),
    block(
      "显示",
      rowCheck(
        "set-ignore-dpi",
        "不跟随系统缩放",
        "界面固定按 100% 显示。Windows 调整缩放后窗口与布局不再跟着变（含托盘菜单）；关掉则恢复跟随",
        s.ignoreDpiScale
      )
    ),
  ].join("");
}

// 「保留 N 天 / 永久保存」的展示文案。0 在语义上是「永久保留」，
// 直接拼数字会得到「仅保留近 0 天内容」这种像 bug 的文案。
function retentionLabel(days) {
  const n = days ?? 3;
  return n === 0 ? "永久保存" : `仅保留近 ${n} 天内容`;
}

// 数据页按同一条链路排：记什么 → 留多久 → 存在哪 → 怎么清 → 删条目的连带动作。
// 最后「危险操作」单独成块并用红色标题隔离 —— 清空全部历史与「文件复制」这种
// 普通开关并列时，视觉上完全分不出哪一个是不可撤销的。
function settingsDataHtml(s) {
  return [
    block(
      "记录",
      rowCheck(
        "set-file-copy",
        "文件复制",
        "记录资源管理器里复制的文件（默认关闭，关闭时不采集）",
        s.fileCopy
      ) +
        rowCtl(
          "保留天数",
          "到期自动清理，<b>默认 3 天</b>；填 0 表示永久保存。<b>置顶与收藏默认豁免</b>，见下方选项",
          `<input type="number" id="set-retention" min="0" value="${s.retentionDays ?? 3}" /><span class="hint">0 = 永久保存</span>`,
          false,
          "set-retention"
        ) +
        // 置顶/收藏默认豁免到期清理：用户把它们标成「要留着」，到期自动删掉
        // 等于违背这个意图。要连它们一起清，必须在这里显式勾选。
        rowCheck(
          "set-prune-pinned",
          "到期清理置顶/收藏",
          "不勾选（默认）则置顶与收藏<b>不会</b>因到期被删；勾选后到期一并清理（不可恢复）",
          s.pruneIncludePinned,
          true
        )
    ),
    block(
      "存储位置",
      `<div class="cache-box">
        <div class="cache-title">缓存目录 <span class="hint">${retentionLabel(s.retentionDays)}</span></div>
        <div class="cache-path" id="cache-path">${s.cacheDir || "加载中…"}</div>
        <div class="cache-stats" id="cache-stats"></div>
        <div class="btn-row">
          <button class="btn-ghost" id="btn-pick-dir">更改目录</button>
          <button class="btn-ghost" id="btn-open-dir">打开目录</button>
          <button class="btn-ghost" id="btn-open-log">日志目录</button>
        </div>
        <div class="cache-log-hint"><b>缓存与日志相互独立</b>：清空、保留天数、退出清理都不会影响日志，日志仅能手动删除。</div>
      </div>`
    ),
    block(
      "清理",
      rowCheck(
        "set-clean",
        "关闭时清理缓存",
        "退出时清理缓存与历史（默认保留置顶/收藏）",
        s.cleanOnClose
      ) +
        rowCheck(
          "set-clean-pinned",
          "同时清理置顶/收藏",
          "不勾选则置顶/收藏不受清理影响",
          s.cleanIncludePinned,
          true
        ) +
        rowCheck(
          "set-clean-confirm",
          "退出前弹窗提醒",
          "退出前先确认一次",
          s.cleanConfirmExit,
          true
        )
    ),
    block(
      "删除文件条目",
      rowCheck(
        "set-delete-files",
        "同时处理本地文件",
        "文件条目只是<b>路径引用</b>。关闭（默认）时删条目只删记录，磁盘上的文件不动；开启后删除会先把本地文件移入回收站，<b>每次都先弹窗确认</b>",
        s.deleteFilesWithItem
      ) +
        rowCtl(
          "处理方式",
          "回收站可还原；永久删除不可恢复",
          `<div class="seg" id="set-delete-mode" ${s.deleteFilesWithItem ? "" : "data-disabled"}>
            <button type="button" data-perm="0" ${s.deleteFilesPermanent ? "" : 'class="on"'}>移入回收站</button>
            <button type="button" data-perm="1" ${s.deleteFilesPermanent ? 'class="on"' : ""}>永久删除</button>
          </div>`,
          true
        )
    ),
    block(
      "危险操作",
      `<div class="row"><div class="row-text"><span class="row-title">删除条目</span>
         <div class="row-desc">这两个操作无法撤销。开了「同时处理本地文件」时，清空已失效不会碰文件（它们本来就不存在）</div></div>
         <div class="row-ctrl">
           <button class="btn-ghost" id="clear-missing">清空已失效</button>
           <button class="btn-danger" id="clear-all">清空全部历史</button>
         </div></div>`,
      { danger: true }
    ),
  ].join("");
}

// ── 关于页 ────────────────────────────────────────────────────────────────
// 版本 / 检查更新 / 运行环境自检 / 快捷入口。
// 材质在不同 Windows 版本上表现不一致，所以把「系统 build + 实际生效档位」直接
// 摆在页面上——出问题时一眼能看到，不用再去翻日志找 [effects] 那几行。
const TIER_LABEL = {
  "system-backdrop": "系统材质（Mica / SystemBackdrop）",
  mica: "Mica",
  acrylic: "亚克力（Acrylic）",
  solid: "纯色（Solid，材质不可用时的兜底）",
};

// 关于页会渲染缓存目录、Release 更新说明等外部文本，统一转义后再插 innerHTML。
// 转义统一走 formats.js（渲染模块自己也要用同一份，不能各写一遍）
const esc = escFmt;

// ── 极简 Markdown 渲染 ──────────────────────────────────────────────────
// Release 说明是 Markdown，纯文本显示会满屏 ** 和 -。不引第三方库（项目取向是
// 依赖极少，且 CSP 禁了外部 CDN），只支持写 Release 时实际会用到的语法：
// 标题、列表、粗体、行内代码、链接、代码块。
//
// 安全前提：先整体 esc 再做标签替换，所以任何标签/属性都不可能从输入注入；
// 链接额外只放行 http(s)，挡掉 javascript: 之类。
function mdInline(t) {
  return (
    t
      // 行内代码优先，内容不再做后续替换，避免代码里的 * _ 被误当强调
      .replace(/`([^`]+)`/g, "<code>$1</code>")
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
      .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, text, url) => {
        const u = esc(url);
        // 只允许 http/https，其余降级成纯文本
        return /^https?:\/\//i.test(url)
          ? `<a href="${u}" data-ext="${u}">${text}</a>`
          : text;
      })
  );
}

function md(src) {
  const lines = String(src ?? "").replace(/\r\n?/g, "\n").split("\n");
  let out = "";
  let list = null; // 当前是否在列表中
  let inCode = false;
  let codeBuf = [];

  const closeList = () => {
    if (list) {
      out += `</${list}>`;
      list = null;
    }
  };

  for (const raw of lines) {
    // 围栏代码块：原样输出，不做任何行内解析
    if (/^\s*```/.test(raw)) {
      if (inCode) {
        out += `<pre><code>${esc(codeBuf.join("\n"))}</code></pre>`;
        codeBuf = [];
        inCode = false;
      } else {
        closeList();
        inCode = true;
      }
      continue;
    }
    if (inCode) {
      codeBuf.push(raw);
      continue;
    }

    const line = esc(raw);
    if (!line.trim()) {
      closeList();
      continue;
    }

    // 标题：### / ## / #
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    if (h) {
      closeList();
      const lv = Math.min(h[1].length + 2, 5); // h1 当 h3 用，避免抢页面标题层级
      out += `<h${lv}>${mdInline(h[2])}</h${lv}>`;
      continue;
    }

    // 列表项：- / * / + / 数字.
    const li = /^(\s*)(?:[-*+]|\d+\.)\s+(.*)$/.exec(line);
    if (li) {
      const want = "ul";
      if (list !== want) {
        closeList();
        out += `<${want}>`;
        list = want;
      }
      out += `<li>${mdInline(li[2])}</li>`;
      continue;
    }

    closeList();
    out += `<p>${mdInline(line)}</p>`;
  }

  // 未闭合的围栏代码块：补上，避免内容丢失
  if (inCode) out += `<pre><code>${esc(codeBuf.join("\n"))}</code></pre>`;
  closeList();
  return out;
}

function settingsAboutHtml() {
  return `
    <div class="about-hero">
      <div class="about-hero-l">
        <div class="about-name">轻贴</div>
        <p class="about-slogan">安静常驻托盘的剪贴板历史 · 只存本机，无账号、不上传</p>
      </div>
      <div class="about-hero-r">
        <div class="about-badge">
          <div class="about-badge-l">当前版本</div>
          <div class="about-badge-v" id="about-ver">读取中…</div>
        </div>
        <div class="about-state" id="about-update">尚未检查更新</div>
        <div class="about-actions">
          <button class="btn-ghost" id="btn-check-update">${updateBtnLabel()}</button>
          <button class="btn-ghost" id="btn-repo">项目主页</button>
          <button class="btn-ghost" id="btn-feedback">问题反馈</button>
          <button class="btn-ghost" id="btn-diag" title="复制版本、系统、材质、数据目录等排查信息">复制诊断信息</button>
        </div>
      </div>
    </div>

    <!-- 隐私承诺集中在一条：这软件的核心卖点就是「只在你自己的机器上」，
         但原来的关于页一个字都没讲，用户只能靠 README 猜。 -->
    <div class="about-privacy">
      <span class="about-privacy-i">只存本机</span>
      <span class="about-privacy-i">Windows 账户级加密</span>
      <span class="about-privacy-i">无账号 · 不上传</span>
      <span class="about-privacy-i">剪贴板正文不进日志</span>
    </div>

    <div class="about-facts">
      <div class="about-fact">
        <span class="about-fact-k">系统</span>
        <span class="about-fact-v" id="ab-os">—</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">材质档位</span>
        <span class="about-fact-v" id="ab-tier">—</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">圆角模式</span>
        <span class="about-fact-v" id="ab-round">—</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">开机自启</span>
        <span class="about-fact-v" id="ab-auto">—</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">内存占用</span>
        <span class="about-fact-v" id="ab-mem">—</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">许可</span>
        <span class="about-fact-v">MIT 开源许可</span>
      </div>
      <div class="about-fact">
        <span class="about-fact-k">缓存目录</span>
        <span class="about-fact-v about-path" id="ab-dir">—</span>
      </div>
    </div>

    <div class="about-log">
      <div class="about-log-h">
        <h3 class="about-log-t" id="about-log-t">更新日志</h3>
        <span class="about-log-v" id="about-log-v"></span>
      </div>
      <div id="about-notes" class="about-notes">正在读取…</div>
    </div>`;
}

function setTxt(id, v) {
  const el = document.getElementById(id);
  if (el) el.textContent = v;
}

// 渲染更新日志：标题带上版本号，让「常驻的是哪一版」一眼可辨。
// 拉不到（离线/私有库）时给一句人话，不留空白。
function renderNotes(tag, notes) {
  const box = document.getElementById("about-notes");
  const vt = document.getElementById("about-log-v");
  if (vt) vt.textContent = tag ? "v" + tag : "";
  if (!box) return;
  if (!notes) {
    box.className = "about-notes is-empty";
    box.textContent = "暂无版本说明。";
    return;
  }
  box.className = "about-notes";
  box.innerHTML = md(notes);
}

// 关于页的更新区域只做「渲染」，不发请求 —— 数据来自 updateInfo（启动自检的缓存
// 或用户手动检查的结果）。这样进关于页不会联网、不会卡，也不会再闪黑窗口。
/**
 * 主按钮文案：区分三种情况。
 *
 * 「已下好待装」和「有新版本待下」是完成同一次更新的两步，所以共用主按钮；
 * 而「已是最新」时主按钮回归「检查更新」—— 不能因为本地躺着一份安装包
 * 就把主按钮占死，那样用户既没法复查更新，看着也很莫名。
 */
/// 主按钮文案三态，与下方点击分发一一对应（顺序也必须一致）：
///   1. 本地已有下载好的安装包（上次取消过安装）→「立即安装」
///   2. 有新版但没下载                          →「下载最新版」
///   3. 其余（已是最新 / 未检查）                →「检查更新」
/// 「重新安装」不再抢占主按钮：装成功后 prune_stale_installers 会清掉
/// 本地包，按钮自动回到「检查更新」，不会挡住检查更新的入口。
function updateBtnLabel() {
  if (updateInfo?.downloadedPath) return "立即安装";
  if (updateInfo?.hasUpdate) return "下载最新版";
  return "检查更新";
}

function renderUpdatePanels() {
  const info = updateInfo;
  const cur = info?.displayCurrent || info?.current || "";
  const has = !!info?.hasUpdate;

  const btn = document.getElementById("btn-check-update");
  if (btn) {
    if (dlState.running) {
      setDownloadUi(dlState.percent);
    } else {
      btn.classList.remove("is-downloading");
      btn.disabled = false;
      btn.textContent = updateBtnLabel();
    }
  }

  const tip = document.getElementById("about-update");
  if (!tip) return;

  if (dlState.running) {
    tip.className = "about-state is-new";
    tip.textContent = `正在下载 v${info?.latest || ""}…`;
  } else if (dlState.error) {
    tip.className = "about-state is-err";
    tip.textContent = "下载失败：" + dlState.error;
  } else if (has && info?.downloadedPath) {
    // 有新版，且包已经下好了（用户上次在安装器上取消了）→ 提示直接装
    tip.className = "about-state is-new";
    tip.innerHTML = `发现新版本 <b>v${esc(info.latest)}</b>，已下载，点击「立即安装」`;
  } else if (has) {
    tip.className = "about-state is-new";
    tip.innerHTML = `发现新版本 <b>v${esc(info.latest)}</b>，当前 v${esc(cur)}`;
  } else if (info?.latest) {
    tip.className = "about-state is-ok";
    // 不带版本号：上方大字已经显示了当前版本，这里再写一遍是重复。
    tip.textContent = "已是最新版本";
  } else {
    tip.className = "about-state";
    tip.textContent = "尚未检查更新";
  }

  // 更新日志：标题带版本号，所以「这条说明属于哪一版」一眼可辨。
  //   有新版 → 显示新版本的说明（这才是你要不要升级的依据）；
  //   没有新版 → 显示**本机这版**的说明。它随包内置，所以 beta 这种
  //              GitHub 上还没有 Release 的版本也能看到自己的更新日志；
  //   两者都没有 → 回退到远端最近一个版本的说明。
  if (has) {
    renderNotes(info.latest, info.notes);
  } else if (info?.localNotes) {
    // 跨版本升级：标题显示区间（`v2.3.0 → v2.4.1`），一眼看出这次跨了几版。
    // 没有起点时（老配置里没这个字段）后端会退化为「最近几节」，
    // 标题相应写成 `v2.4.1 及之前 3 个版本`，免得只写一个版本号却列了好几节。
    const from = info.localNotesFrom;
    const to = info.localTag || cur;
    const count = info.localNotesCount || 1;
    let tag = to;
    if (from && from !== to) tag = `${from} → ${to}`;
    else if (count > 1) tag = `${to} 及之前 ${count - 1} 个版本`;
    renderNotes(tag, info.localNotes);
  } else {
    renderNotes(info?.latest || "", info?.notes || "");
  }
}

/**
 * 下载进度画进按钮：底色是从左向右推进的进度条。
 *
 * 进度值走 CSS 变量 `--p`，填充层与「已填充部分的文字」共用同一条裁剪线，
 * 所以两者永远同步；按钮还有一层固定的空轨底色，否则根本看不出填充从哪儿开始。
 */
function setDownloadUi(percent) {
  const btn = document.getElementById("btn-check-update");
  if (!btn) return;
  const p = Math.min(100, Math.max(0, Math.round(Number(percent) || 0)));
  if (!btn.classList.contains("is-downloading")) {
    btn.classList.add("is-downloading");
    btn.disabled = true;
    btn.innerHTML =
      `<span class="dl-fill" aria-hidden="true"></span>` +
      `<span class="dl-label dl-label-off" aria-hidden="true"></span>` +
      `<span class="dl-label dl-label-on" aria-hidden="true"></span>`;
  }
  setDownloadLabel(`正在下载 ${p}%`);
  btn.style.setProperty("--p", `${p}%`);
}

/** 进度按钮的两层文字必须同内容（一层在空轨上、一层在填充上），一起改。 */
function setDownloadLabel(text) {
  const btn = document.getElementById("btn-check-update");
  if (!btn) return;
  btn.querySelectorAll(".dl-label").forEach((el) => (el.textContent = text));
  // 两层文字都是装饰性的（aria-hidden），可读文本由按钮自己的 aria-label 承担
  btn.setAttribute("aria-label", text);
}

/** 下载完成后的那一步：进度条填满，文字换成结果（如「正在启动安装程序…」）。 */
function setDownloadDone(text) {
  const btn = document.getElementById("btn-check-update");
  if (!btn) return;
  if (!btn.classList.contains("is-downloading")) setDownloadUi(100);
  btn.style.setProperty("--p", "100%");
  setDownloadLabel(text);
}

/** 点「下载最新版」：Rust 下到缓存目录 update/，完成后自动拉起安装器并退出本进程。 */
async function startDownload() {
  if (dlState.running) return;
  dlState = { running: true, percent: 0, error: "" };
  renderUpdatePanels();
  let downloaded = "";
  try {
    downloaded = await invoke("download_update");
    setDownloadDone("下载完成，正在启动安装程序…");
    // Rust 会先用 runas 拉起安装器（触发 UAC），再退出自己 ——
    // 安装器要覆盖 exe，本进程正锁着它。
    await invoke("install_downloaded", { path: downloaded });
  } catch (e) {
    // 分开报：下载失败和「下载好了但安装器起不来」是两回事
    dlState = {
      running: false,
      percent: 0,
      error: downloaded ? `启动安装程序失败（${e}）` : String(e),
    };
    renderUpdatePanels();
    toast(downloaded ? "无法启动安装程序：" + e : "下载失败：" + e);
  }
}

/**
 * 「重新安装」：安装包已在本地（上次下载完成后用户取消了安装），
 * 直接拉起，不再下载。装完 update_state 里的 downloadedPath 自然消失。
 */
async function installDownloaded() {
  const path = updateInfo?.downloadedPath;
  if (!path) return;
  setDownloadDone("正在启动安装程序…");
  try {
    await invoke("install_downloaded", { path });
  } catch (e) {
    // UAC 点否 / 安装器起不来：文件还在，按钮保持「重新安装」
    toast("无法启动安装程序：" + e);
    renderUpdatePanels();
  }
}

async function loadAbout() {
  try {
    const a = await invoke("about_info");
    setTxt("about-ver", "v" + a.version);
    // 系统版本：产品名 + marketing version(26H2) + build(26300)，全部来自注册表。
    // 缺哪段就省哪段，不显示「未知/undefined」这类脏字。
    const parts = [];
    if (a.osName) parts.push(a.osName);
    if (a.osDisplay) parts.push(a.osDisplay);
    if (a.osBuildRaw) parts.push("build " + a.osBuildRaw);
    setTxt("ab-os", parts.length ? parts.join(" · ") : "Windows");
    setTxt("ab-tier", TIER_LABEL[a.tier] || a.tier);
    setTxt("ab-round", a.roundMode ? "开启" : "关闭");
    setTxt("ab-auto", a.autoStart ? "已开启" : "未开启");
    setTxt("ab-mem", `${a.memMb} MB`);
    setTxt("ab-dir", a.dataDir || "（默认）");
  } catch (e) {
    setTxt("about-ver", "读取失败");
  }
  // 更新状态只读缓存；万一启动时的自检还没落地，这里补问一次（同样不联网）
  if (!updateInfo) {
    try {
      updateInfo = await invoke("update_state");
    } catch (e) {
      updateInfo = null;
    }
  }
  renderUpdatePanels();
}

async function aboutCheckUpdate() {
  const btn = document.getElementById("btn-check-update");
  if (btn) btn.disabled = true;
  const tip = document.getElementById("about-update");
  if (tip) {
    tip.className = "about-state";
    tip.textContent = "正在检查更新…";
  }
  try {
    updateInfo = await invoke("check_update");
    dlState = { running: false, percent: 0, error: "" };
    applyUpdateState();
    toast(
      updateInfo.hasUpdate
        ? `发现新版本 v${updateInfo.latest}，当前 v${updateInfo.displayCurrent}`
        : "已是最新版本"
    );
  } catch (e) {
    if (tip) {
      tip.className = "about-state is-err";
      tip.textContent = "检查更新失败";
    }
    toast("检查更新失败，请检查网络");
  }
  renderUpdatePanels();
}

/** 更新状态 → 侧栏「设置」红点 + 「关于」标签红点 + 关于页内容。 */
function applyUpdateState() {
  const has = !!updateInfo?.hasUpdate;
  const tip = has ? `有新版本 v${updateInfo.latest}，点击查看` : "";

  // 侧栏底部「设置」是常驻 DOM，直接改类名即可
  const navSet = document.querySelector(
    '.sidebar-footer .nav-item[data-view="settings"]'
  );
  if (navSet) {
    navSet.classList.toggle("has-dot", has);
    if (has) navSet.title = tip;
    else navSet.removeAttribute("title");
  }

  // 「关于」标签在设置页里，随 settingsHtml() 渲染；这里兼顾「设置页已经开着」的情况
  const tabAbout = document.querySelector('.settings-tabs .tab[data-tab="about"]');
  if (tabAbout) {
    tabAbout.classList.toggle("has-dot", has);
    if (has) tabAbout.title = tip;
    else tabAbout.removeAttribute("title");
  }

  renderUpdatePanels();
}

// 横幅自动收起的计时器（0 = 没在计时）
let bannerTimer = 0;

/** 收起横幅：手动关闭、点「查看」、到点自动收起，三条路都走这里（顺带清掉计时器）。 */
function closeUpdateBanner() {
  if (bannerTimer) {
    clearTimeout(bannerTimer);
    bannerTimer = 0;
  }
  const el = document.getElementById("update-banner");
  if (!el) return;
  el.classList.add("is-out");
  // 等淡出走完再摘掉节点，别「啪」一下消失
  setTimeout(() => el.remove(), 200);
}

/**
 * 启动自检发现新版本时的一次性提示：窗口顶部横幅，点「查看」直接跳到关于页。
 * 出现 **10 秒后自动收起** —— 通知到就够了，不该一直占住标题栏下方。
 */
function showUpdateBanner() {
  if (!updateInfo?.hasUpdate) return;
  if (document.getElementById("update-banner")) return;

  const el = document.createElement("div");
  el.id = "update-banner";
  el.innerHTML =
    `<span class="ub-text">发现新版本 <b>v${esc(updateInfo.latest)}</b></span>` +
    `<button class="ub-btn" id="ub-go">查看</button>` +
    `<button class="ub-close" id="ub-close" title="关闭">×</button>`;
  document.body.appendChild(el);

  el.querySelector("#ub-go").onclick = () => {
    closeUpdateBanner();
    // 先定好要显示的分类，再走真实点击 —— 导航是事件委托在 .sidebar 上的，
    // 这样最不容易漏掉状态同步（render() 会按 settingsTab 渲染设置页）。
    settingsTab = "about";
    const nav = document.querySelector(
      '.sidebar-footer .nav-item[data-view="settings"]'
    );
    if (nav) nav.click();
  };
  el.querySelector("#ub-close").onclick = closeUpdateBanner;
  // 10 秒后自动收起
  bannerTimer = setTimeout(closeUpdateBanner, 10000);
}

/** 重复启动提示弹窗（常驻元素，绑一次）。 */
function bindSingletonModal() {
  const modal = document.getElementById("singleton-modal");
  const ok = document.getElementById("singleton-ok");
  if (modal && ok) ok.onclick = () => (modal.hidden = true);
}

/** 首次运行引导：开机自启（只在全新安装后的第一次启动出现）。 */
function openAutostartAsk() {
  const modal = document.getElementById("firstrun-autostart");
  if (modal) modal.hidden = false;
}

/** 重新拉一次设置，同步内存里的 settings。
 *
 * 改完「问过了 / 看过导览」这类开关后必须刷一次，否则同一会话里后续判断
 * （比如关闭窗口时读 askedTrayOnClose）还在用旧值。失败就保持旧值，不打断流程。 */
async function reloadSettings() {
  try {
    settings = await invoke("get_settings");
  } catch {
    /* 拿不到就保持旧值，不影响当前流程 */
  }
}

// ---------------------------------------------------------------------------
// 首次运行功能导览（7 步卡片式巡览，文案见 ONBOARDING.md）
//
// 为什么是卡片而不是聚光灯指引：窗口只有 1060×690，遮罩挖洞很容易连要指的
// 控件一起盖住；而且控件位置随视图、分栏、窗口尺寸变化，定位逻辑必然错位。
//
// 关键约定：**任何退出方式都算「看过了」**（完成 / 跳过 / Esc / 点遮罩），
// 否则每次启动都弹，比不弹更烦。
// ---------------------------------------------------------------------------

let tourStep = 0;

/** 一次性取出导览需要的节点，避免每个函数里反复 getElementById */
function tourEls() {
  const mask = document.getElementById("feature-tour");
  if (!mask) return null;
  return {
    mask,
    body: mask.querySelector(".tour-body"),
    steps: [...mask.querySelectorAll(".tour-step")],
    dots: document.getElementById("tour-dots"),
    prev: document.getElementById("tour-prev"),
    next: document.getElementById("tour-next"),
    skip: document.getElementById("tour-skip"),
    card: mask.querySelector(".modal"),
  };
}

/** 重播一个具名动效（见 src/animations.css）。
 *
 * 必须「移除 → 强制回流 → 加回」：同一个 class 一直在时浏览器不会重跑动画，
 * 翻页会变成从第二页起就没有进场效果。读一次 offsetWidth 是为了逼出回流。 */
function replayFx(el, cls) {
  if (!el) return;
  el.classList.remove(cls);
  void el.offsetWidth;
  el.classList.add(cls);
}

function renderTourStep(t) {
  const last = t.steps.length - 1;
  t.steps.forEach((s, i) => (s.hidden = i !== tourStep));
  // 正文区回到顶部：上一步可能是长内容（设置那步），滚到底再翻页会停在半截
  if (t.body) t.body.scrollTop = 0;
  replayFx(t.steps[tourStep], "fx-stagger");
  // 步骤计数放在进度条左边，与胶囊互相印证
  const count = document.getElementById("tour-count");
  if (count) count.textContent = `${tourStep + 1} / ${t.steps.length}`;
  // 顶部进度条：按步数推进（第 1 步就有填充，不从 0 起步）
  const fill = document.getElementById("tour-fill");
  if (fill) fill.style.width = `${((tourStep + 1) / t.steps.length) * 100}%`;
  // 进度点按步数生成，步数改了这里不用动
  if (t.dots && t.dots.childElementCount !== t.steps.length) {
    t.dots.replaceChildren(
      ...t.steps.map(() => {
        const d = document.createElement("span");
        d.className = "tour-dot";
        return d;
      })
    );
  }
  if (t.dots) {
    [...t.dots.children].forEach((d, i) => d.classList.toggle("is-on", i === tourStep));
  }
  t.prev.hidden = tourStep === 0;
  t.next.textContent = tourStep === last ? "开始使用" : "下一步";
}

/** 本次导览是不是「首次运行」那条链路打开的 —— 决定关闭后要不要接着问开机自启。
 *  从设置页「重新查看」进来的不算：那种情形下用户早就过了首次引导。 */
let tourFromFirstRun = false;

function openFeatureTour(fromFirstRun = false) {
  const t = tourEls();
  if (!t) return;
  tourFromFirstRun = fromFirstRun;
  tourStep = 0;
  renderTourStep(t);
  t.mask.hidden = false;
  replayFx(t.card, "fx-pop-in");
}

/**
 * 关闭导览并记下「已看过」。
 *
 * 导览是**纯讲解卡片**，不夹带任何设置动作 —— 它只负责讲「这里能干什么」，
 * 开关留给用户自己去设置页点。所以这里只写记忆标记，然后接着走首次引导的下一步。
 */
async function closeFeatureTour() {
  const t = tourEls();
  if (!t) return;
  t.mask.hidden = true;
  await invoke("dismiss_onboarding", { kind: "tour" }).catch(() => {});
  await reloadSettings();
  // 首次运行这条链路上，导览看完接着问「开机自启」（导览本身不问）。
  // 从设置页重看的不接这一步 —— 那种情形下用户早过了首次引导。
  if (tourFromFirstRun && settings && !settings.askedAutostart) openAutostartAsk();
}

/** 导览的翻页与退出绑定（常驻元素，只绑一次） */
function bindFeatureTour() {
  const t = tourEls();
  if (!t) return;
  t.next.onclick = () => {
    if (tourStep >= t.steps.length - 1) {
      closeFeatureTour();
    } else {
      tourStep += 1;
      renderTourStep(t);
    }
  };
  t.prev.onclick = () => {
    if (tourStep === 0) return;
    tourStep -= 1;
    renderTourStep(t);
  };
  // 跳过与点遮罩都算看过
  t.skip.onclick = () => closeFeatureTour();
  t.mask.onclick = (e) => {
    if (e.target === t.mask) closeFeatureTour();
  };
}

/**
 * 首次运行引导的两个弹窗（常驻元素，绑一次）。
 *
 * 两个引导都遵循同一原则：**不管用户选什么都算「问过了」**，
 * 之后不再弹 —— 真正的开关随时能在设置里改，每次启动都追问才是骚扰。
 */
function bindFirstRunModals() {
  // 记下「已经问过」，失败也不阻塞流程（顶多是下次再问一遍）
  const dismiss = (kind) => invoke("dismiss_onboarding", { kind }).catch(() => {});
  const refresh = reloadSettings;

  const auto = document.getElementById("firstrun-autostart");
  const autoEnable = document.getElementById("fra-enable");
  if (autoEnable)
    autoEnable.onclick = async () => {
      if (auto) auto.hidden = true;
      try {
        await invoke("set_autostart", { value: true });
        toast("已开启开机自启");
      } catch (e) {
        toast("开启失败：" + e);
      }
      await dismiss("autostart");
      await refresh();
    };
  const autoLater = document.getElementById("fra-later");
  if (autoLater)
    autoLater.onclick = async () => {
      if (auto) auto.hidden = true;
      await dismiss("autostart");
      toast("以后可在「设置 → 通用」里打开");
    };

  const close = document.getElementById("firstrun-close");
  const closeTray = document.getElementById("frc-tray");
  if (closeTray)
    closeTray.onclick = async () => {
      if (close) close.hidden = true;
      await invoke("set_tray_on_close", { value: true }).catch(() => {});
      await dismiss("tray");
      await refresh();
      // 直接走关闭：Rust 侧 CloseRequested 会按 tray_on_close 把窗口收进托盘。
      // 刻意**不**经过 doCloseWindow()——用户刚选了后台运行，
      // 这时再弹「退出并清理」确认是自相矛盾的。
      appWindow.close().catch((err) => toast("关闭失败：" + err));
    };
  const closeQuit = document.getElementById("frc-quit");
  if (closeQuit)
    closeQuit.onclick = async () => {
      if (close) close.hidden = true;
      await invoke("set_tray_on_close", { value: false }).catch(() => {});
      await dismiss("tray");
      await refresh();
      // 选了直接退出 → 走正常关闭路径（该弹的清理确认照旧）
      doCloseWindow();
    };
}

async function aboutFeedback() {
  try {
    const a = await invoke("about_info");
    const body =
      `版本：${a.version}\n系统 build：${a.osBuild}\n` +
      `材质档位：${a.tier}\n圆角模式：${a.roundMode ? "开" : "关"}\n\n` +
      `问题描述：\n`;
    const url =
      "https://github.com/kunge9106/self_clipboard/issues/new?title=" +
      encodeURIComponent("问题反馈") +
      "&body=" +
      encodeURIComponent(body);
    await invoke("open_external", { url });
  } catch (e) {
    toast("打开反馈页面失败：" + e);
  }
}

async function refreshCacheInfo() {
  try {
    const info = await invoke("cache_info");
    const pathEl = document.getElementById("cache-path");
    const statsEl = document.getElementById("cache-stats");
    if (pathEl) pathEl.textContent = info.dir;
    if (statsEl) {
      // 日志单独标出来：既不隐瞒它占的空间，也说清它是「清不掉的那部分」。
      const logPart = info.logSize
        ? `，含日志 ${fmtSize(info.logSize)}`
        : "";
      statsEl.textContent = `${info.count} 个文件 · ${fmtSize(info.size)}${logPart}`;
    }
  } catch (e) {
    console.error("cache_info failed", e);
  }
}

function fmtSize(bytes) {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / 1024 / 1024).toFixed(1) + " MB";
}

function bindSettings(view) {
  // 分类标签：切换后整块重新渲染 + 重新绑定。
  // 只改 settingsTab 再重绘就够 —— 每组控件互不依赖，重绑比「局部显示/隐藏」更不容易漏。
  // 注意：这里不能复用 render()，render() 走的是列表分支（会重建 .history 滚动容器）。
  view.querySelectorAll(".settings-tabs .tab").forEach((btn) => {
    btn.onclick = () => {
      const id = btn.dataset.tab;
      if (!id || id === settingsTab) return;
      settingsTab = id;
      view.innerHTML = settingsHtml();
      bindSettings(view);
    };
  });

  /// 开关类设置的统一绑定。
  ///
  /// 提示文案从**行标题**取（`.row-title`），而不是写死「已开启/已关闭」——
  /// 八个开关共用一条绑定，写死的话用户只看到「已关闭」，根本不知道关的是哪个
  /// （用户实测反馈）。取标题还有个好处：以后新增开关不用再改文案。
  const bindCheck = (id, cmd) => {
    const el = view.querySelector("#" + id);
    if (!el) return;
    el.onchange = async () => {
      const title =
        el.closest(".row")?.querySelector(".row-title")?.textContent?.trim() || "设置";
      try {
        await invoke(cmd, { value: el.checked });
        // 同步本地缓存（退出弹窗文案等依赖最新配置）
        settings = await invoke("get_settings");
        toast(`${title}已${el.checked ? "开启" : "关闭"}`);
      } catch (e) {
        toast(`${title}保存失败：${e}`);
        el.checked = !el.checked;
      }
    };
  };
  bindCheck("set-autostart", "set_autostart");
  bindCheck("set-tray", "set_tray_on_close");
  bindCheck("set-file-copy", "set_file_copy");
  bindCheck("set-delete-files", "set_delete_files_with_item");

  // 「处理方式」是从属选项：主开关关着时整组置灰且不可点（CSS 里 pointer-events: none 兜底）。
  // 视觉禁用而不禁用行为是最糟的组合——灰着却点得动，用户会以为开关自己坏了。
  const delMode = view.querySelector("#set-delete-mode");
  const delFilesChk = view.querySelector("#set-delete-files");
  if (delMode && delFilesChk) {
    const syncDelMode = () => delMode.toggleAttribute("data-disabled", !delFilesChk.checked);
    syncDelMode();
    delFilesChk.addEventListener("change", syncDelMode);
    delMode.addEventListener("click", async (e) => {
      const btn = e.target.closest("button[data-perm]");
      if (!btn || !delFilesChk.checked) return;
      const perm = btn.dataset.perm === "1";
      try {
        await invoke("set_delete_files_permanent", { value: perm });
        settings = await invoke("get_settings");
        delMode
          .querySelectorAll("button[data-perm]")
          .forEach((b) => b.classList.toggle("on", b === btn));
        toast(perm ? "已改为永久删除（不可恢复）" : "已改为移入回收站");
      } catch (err) {
        toast("保存失败：" + err);
      }
    });
  }
  bindCheck("set-clean", "set_clean_on_close");
  bindCheck("set-clean-pinned", "set_clean_include_pinned");
  // 保留期清理的豁免开关。与上面那条是**两个不同策略点**，别合并：
  // 一个是「我主动退出时清不清置顶」，一个是「后台到期时清不清置顶」。
  bindCheck("set-prune-pinned", "set_prune_include_pinned");
  bindCheck("set-clean-confirm", "set_clean_confirm_exit");

  bindCheck("set-pin-cancel", "set_pin_cancel_on_disable");
  bindCheck("set-ignore-dpi", "set_ignore_dpi_scale");

  // 主题三档：浅色 / 深色 / 系统。切换走 setThemeMode（与侧栏按钮同一入口），
  // 所以在这边点了之后侧栏按钮的文案也会跟着变。
  const themeSeg = view.querySelector("#set-theme");
  if (themeSeg) {
    markThemeSeg(settings.themeMode || "system");
    themeSeg.addEventListener("click", async (e) => {
      const btn = e.target.closest("button[data-theme]");
      if (!btn) return;
      try {
        await setThemeMode(btn.dataset.theme);
      } catch (err) {
        console.error("set theme mode failed", err);
      }
    });
  }

  // 重看功能导览：只打开，不写「已看过」标记 —— 看多少次都不影响首次判断
  const replayTour = view.querySelector("#replay-tour");
  if (replayTour) replayTour.onclick = () => openFeatureTour();

  // 置顶互通不能走 bindCheck：这个开关会把已有置顶记录归一到新表示，
  // 必须顺带把本地缓存的 items 拉新的，否则列表里的置顶徽章/排序还是旧的。
  const pinGlobal = view.querySelector("#set-pin-global");
  if (pinGlobal)
    pinGlobal.onchange = async () => {
      try {
        await invoke("set_pin_global", { value: pinGlobal.checked });
        settings = await invoke("get_settings");
        items = await invoke("get_history");
        toast(pinGlobal.checked ? "置顶已改为全分类互通" : "置顶已改为按分类独立");
      } catch (e) {
        toast("保存失败：" + e);
        pinGlobal.checked = !pinGlobal.checked;
      }
    };

  const retention = view.querySelector("#set-retention");
  if (retention)
    retention.onchange = async () => {
      const days = Math.max(0, parseInt(retention.value, 10) || 0);
      retention.value = days;
      try {
        await invoke("set_retention_days", { days });
        // 立即按新天数清理并刷新列表，让「修改生效」可见
        items = await invoke("get_history");
        settings = await invoke("get_settings");
        render();
        toast(days === 0 ? "已改为永久保留" : `仅保留近 ${days} 天`);
      } catch (e) {
        toast("保存失败：" + e);
      }
    };

  const material = view.querySelector("#set-material");
  if (material) {
    const label = view.querySelector("#material-val");
    // 只显示百分比：括号里的说明与上方「越高越厚实 · 越低越通透」重复，
    // 而且拖动时文字长度跳动，视觉上像"突然冒出一串括号"（用户实测反馈）。
    const show = (v) => {
      if (label) label.textContent = `${v}%`;
    };
    // 拖动中：只改观感与本地状态，不落盘（高频写 config.json 没必要）。
    // 关键是同步模块级 settings —— settingsHtml() 读的就是它，
    // 不同步的话切到别的分类再回来，滑块会显示成旧值（看起来像「恢复默认」）。
    material.addEventListener("input", () => {
      const v = parseInt(material.value, 10);
      if (!Number.isFinite(v)) return;
      show(v);
      applyGlassAlpha(v);
      syncAlphaToSettings(v);
    });
    // 松手才持久化
    material.addEventListener("change", () => {
      const v = parseInt(material.value, 10);
      if (!Number.isFinite(v)) return;
      invoke("set_material_alpha", { value: v }).catch((e) =>
        toast("保存失败：" + e)
      );
    });
  }

  // 圆角模式：切换后立即改动窗口材质（Rust）+ CSS 层观感，无需重启。
  //
  // **浓度不跟着动**：直角 / 圆角两个模式各记着自己那一档
  // （后端 alpha_straight / alpha_round），切模式只是换用另一档 ——
  // 直角调到 68 → 切圆角（用圆角那档，比如 45）→ 切回直角仍是 68。
  // 这里把该模式记住的那一档取回来，同步到滑块 / 标签 / 观感。
  const round = view.querySelector("#set-round");
  if (round)
    round.onchange = async () => {
      try {
        await invoke("set_round_mode", { value: round.checked });
        applyRoundMode(round.checked);
        // 模式变了 → 当前生效的那一档（materialAlpha）也随之改变，重新拉一次配置
        settings = await invoke("get_settings");
        const alpha = settings.materialAlpha ?? 50;
        applyGlassAlpha(alpha);
        const slider = view.querySelector("#set-material");
        if (slider) slider.value = alpha;
        const label = view.querySelector("#material-val");
        if (label) label.textContent = `${alpha}%（越高越厚实 · 越低越通透）`;
        toast(
          round.checked
            ? `已切换为圆角模式（无背景模糊）· 浓度 ${alpha}%`
            : `已切换为直角模式（保留背景模糊）· 浓度 ${alpha}%`
        );
      } catch (e) {
        toast("保存失败：" + e);
        round.checked = !round.checked;
      }
    };

  // 缓存目录那两个按钮只存在于「数据与缓存」页，切到别的分类时它们不在 DOM 里
  // → 必须做存在性判断，否则 null.onclick 会直接抛错、整页绑定中断。
  const pickDir = view.querySelector("#btn-pick-dir");
  if (pickDir)
    pickDir.onclick = async () => {
      try {
        const res = await invoke("pick_cache_dir");
        if (res) {
          const [path, moved] = res;
          toast(
            moved
              ? `已迁移到「${path}」，即将重启…`
              : "已经是当前缓存目录，无需更改"
          );
        }
      } catch (e) {
        toast("更改目录失败：" + e);
      }
    };

  const openDir = view.querySelector("#btn-open-dir");
  if (openDir)
    openDir.onclick = () =>
      invoke("open_cache_dir").catch((e) => toast("打开失败：" + e));

  const openLog = view.querySelector("#btn-open-log");
  if (openLog)
    openLog.onclick = () =>
      invoke("open_log_dir").catch((e) => toast("打开失败：" + e));

  // 「关于」页的按钮同理，只在切到该分类时存在 → 同样做存在性保护；
  // 顺便在进入该页时就把版本号与运行环境拉出来。
  const btnCheck = view.querySelector("#btn-check-update");
  if (btnCheck) {
    // 有新版且已下好 → 直接装；有新版没包 → 先下；没新版 → 复查
    btnCheck.onclick = () => {
      // 与 updateBtnLabel() 的三态严格同序：有包先装、有新版下载、
      // 其余检查更新。顺序若与文案不一致，边缘情况（版本追平但包还在）
      // 会出现「按钮写着立即安装、点的却是检查更新」的割裂。
      if (updateInfo?.downloadedPath) return installDownloaded();
      if (updateInfo?.hasUpdate) return startDownload();
      return aboutCheckUpdate();
    };
    loadAbout();
  }

  const btnRepo = view.querySelector("#btn-repo");
  if (btnRepo)
    btnRepo.onclick = () =>
      invoke("open_external", {
        url: "https://github.com/kunge9106/self_clipboard",
      });
  const btnFeedback = view.querySelector("#btn-feedback");
  if (btnFeedback) btnFeedback.onclick = aboutFeedback;

  // 一键复制诊断信息：报障时用户最缺的就是「版本/系统/材质/数据在哪」，
  // 让他一条条截图远不如点一下粘贴。
  const btnDiag = view.querySelector("#btn-diag");
  if (btnDiag)
    btnDiag.onclick = async () => {
      try {
        await invoke("copy_diagnostics");
        toast("诊断信息已复制，可直接粘贴到反馈里");
      } catch (e) {
        toast("复制失败：" + e);
      }
    };

  // 更新日志里的链接：用事件委托交给系统浏览器打开。
  // WebView 内的 <a href> 会被当作应用内导航（点了直接白屏），所以必须拦下。
  const notesBox = view.querySelector("#about-notes");
  if (notesBox) {
    notesBox.addEventListener("click", (e) => {
      const a = e.target.closest("a[data-ext]");
      if (!a) return;
      e.preventDefault();
      invoke("open_external", { url: a.dataset.ext });
    });
  }

  // 危险操作：这里只负责把确认弹窗打开，真正的清空要用户在弹窗里再确认一次才执行
  const clear = view.querySelector("#clear-all");
  if (clear) clear.onclick = () => openClearAllModal();

  const clearMissing = view.querySelector("#clear-missing");
  if (clearMissing) clearMissing.onclick = () => openClearMissingModal();

  // 同理：只有缓存块在页面上时才去查缓存信息，别在别的分类里白发一次 IPC
  if (view.querySelector("#cache-path")) refreshCacheInfo();
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function main() {
  // 控件绑定优先（即便后续异步步骤出错，也能保证可交互）
  bindWindowControls();
  bindHoverGuard();
  bindDrag();
  bindResize();
  bindExitModal();
  bindClearAllModal();
  bindClearMissingModal();
  bindPurgeModal();
  bindFeatureTour();
  bindClearView();
  bindSelectBar();
  bindClearViewModal();
  bindSingletonModal();
  bindFirstRunModals();
  bindDetailModal();
  bindNav();
  bindThemeToggle();
  bindSearch();
  startTimeTicker();

  await applyEffectInfo();

  // 初始历史
  try {
    items = await invoke("get_history");
  } catch (e) {
    console.error("load history failed", e);
  }

  // 配置与版本号（状态栏）
  try {
    settings = await invoke("get_settings");
    // 应用材质浓度（CSS 层，窗口主体的不透明度）
    applyGlassAlpha(settings.materialAlpha ?? 50);
    // 圆角模式（圆角 + 无系统模糊）
    applyRoundMode(settings.roundMode);
    // 主题三档（浅色/深色/系统）：单独 try，避免权限缺失时中断其余绑定
    try {
      await applyThemeMode(settings.themeMode);
      await appWindow.onThemeChanged(({ payload }) => {
        // 手动钉住时不跟系统；只有 system 档才响应
        if ((settings.themeMode || "system") === "system") applyTheme(payload === "dark");
      });
    } catch (e) {
      console.error("theme setup failed", e);
    }
  } catch (e) {
    console.error("load settings failed", e);
  }
  try {
    const rawVer = await getVersion();
    // 预发布版（版本号含「-」如 2.2.1-beta.1）在尾部标 Beta，一眼可辨。
    const isPre = rawVer.includes("-");
    document.getElementById("status-version").textContent =
      `Version：${rawVer}${isPre ? "（Beta）" : ""}`;
  } catch (e) {
    // 版本号不在前端写死：它由 tauri.conf.json 引用 package.json 得到，
    // 写死会在下次发版后与真实版本不一致。取不到时留一个占位符。
    console.error("get version failed", e);
    document.getElementById("status-version").textContent = "Version：—";
  }

  render();

  // 首次运行引导（只在全新安装后的第一次启动出现）。
  // 顺序：功能导览 → 开机自启 →（首次点关闭时）后台运行询问。
  // 导览是纯讲解卡片，不问自启；自启询问在导览关闭后由 closeFeatureTour 接上。
  if (settings && settings.tourSeenVersion < settings.tourVersion) {
    openFeatureTour(true);
  } else if (settings && !settings.askedAutostart) {
    openAutostartAsk();
  }

  // ── 更新状态 ──────────────────────────────────────────────────────────
  // Rust 启动时已在后台自检过一次（失败或没有新版都静默）。这里先读缓存把红点
  // 补上，再监听自检结果事件 —— 自检通常比前端就绪慢，所以两条路都要走。
  try {
    updateInfo = await invoke("update_state");
  } catch (e) {
    updateInfo = null;
  }
  applyUpdateState();

  await listen("update-available", (e) => {
    updateInfo = e.payload;
    applyUpdateState();
    // 只在启动自检发现新版本时提示一次；失败/无新版不会有这条事件
    showUpdateBanner();
  });

  await listen("update-progress", (e) => {
    const p = e.payload?.percent ?? 0;
    dlState.percent = p;
    if (dlState.running) setDownloadUi(p);
  });

  // 重复启动 → 主进程已把窗口唤回，这里补一个说明
  await listen("singleton-wake", () => {
    const modal = document.getElementById("singleton-modal");
    if (modal) modal.hidden = false;
  });

  // 增量：复制新内容时 Rust 推事件，直接插到顶部
  await listen("clipboard-update", (e) => {
    const item = e.payload;
    if (!items.some((i) => i.id === item.id)) {
      items.unshift(item);
      // 新内容插到顶部，但用户可能正在看下面的内容 → 别把位置顶掉
      render({ keepScroll: true });
    }
  });
}

main();
