import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";

const appWindow = getCurrentWindow();

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
let query = "";
let settings = null; // 后端配置（get_settings）

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

function bindWindowControls() {
  const guard = (label, p) =>
    p.catch((err) => toast(label + "失败：" + err));
  document.getElementById("btn-minimize").onclick = () =>
    guard("最小化", appWindow.minimize());
  document.getElementById("btn-maximize").onclick = () =>
    guard("最大化", appWindow.toggleMaximize());
  document.getElementById("btn-close").onclick = () => {
    // 「关闭时清理缓存」+「退出前弹窗提醒」都开启时，先确认再退出（需求 #2）
    if (settings && settings.cleanOnClose && settings.cleanConfirmExit) {
      openExitModal();
    } else {
      guard("关闭", appWindow.close());
    }
  };

  // 窗口尺寸变化时同步图标
  appWindow.onResized(() => syncMaxState());
  // 从最小化 / 托盘恢复时尺寸未必变化 → onResized 不触发，图标会停在旧状态。
  // 获得焦点时补一次同步，顺带把「材质浓度 / 圆角模式」按当前配置重放一遍。
  appWindow.onFocusChanged(({ payload: focused }) => {
    if (!focused) return;
    syncMaxState();
    if (settings) {
      applyGlassAlpha(settings.materialAlpha ?? 62);
      applyRoundMode(settings.roundMode);
    }
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
    ? "退出时将清理全部缓存内容（含置顶/收藏），确定退出吗？"
    : "退出时将清理缓存内容（置顶/收藏会保留），确定退出吗？";
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
// 工具栏垃圾桶：搜索时只清搜索结果，否则清全部；置顶/收藏始终豁免（需求 #3）
// ---------------------------------------------------------------------------

function bindClearView() {
  document.getElementById("btn-clear-view").onclick = async () => {
    const targets = filtered().filter((i) => !isPinnedAnywhere(i) && !i.favorite);
    if (targets.length === 0) {
      toast("没有可清理的条目（置顶/收藏除外）");
      return;
    }
    try {
      await invoke("delete_items", { ids: targets.map((i) => i.id) });
      items = items.filter((i) => !targets.some((t) => t.id === i.id));
      render();
      toast(query ? `已清理 ${targets.length} 条搜索结果` : `已清理 ${targets.length} 条`);
    } catch (e) {
      toast("清理失败：" + e);
    }
  };
}

function bindNav() {
  // 监听整个侧栏：历史/收藏/图片/文件 与 底部「设置」都在 .sidebar 内
  document.querySelector(".sidebar").addEventListener("click", (e) => {
    const item = e.target.closest(".nav-item");
    if (!item) return;
    document
      .querySelectorAll(".nav-item")
      .forEach((n) => n.classList.remove("is-active"));
    item.classList.add("is-active");
    currentView = item.dataset.view;
    render();
  });
}

function bindSearch() {
  const el = document.getElementById("search");
  el.addEventListener("input", () => {
    query = el.value.trim().toLowerCase();
    render();
  });
}

// ---------------------------------------------------------------------------
// 历史渲染
// ---------------------------------------------------------------------------

/** 「文件复制」是否开启。默认关闭：不采集文件、也不展示任何文件条目。 */
function fileCopyOn() {
  return !!(settings && settings.fileCopy);
}

/** 置顶按分类独立：这条内容是否在 view 这个分类里被置顶 */
function isPinnedIn(item, view) {
  return Array.isArray(item.pinnedViews) && item.pinnedViews.includes(view);
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
        (i.files && i.files.some((f) => f.toLowerCase().includes(query)))
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

function render() {
  hideImagePreview(); // 重绘后旧卡片已销毁，避免浮层残留
  const view = document.getElementById("view");
  const toolbar = document.querySelector(".toolbar");
  const isSettings = currentView === "settings";
  // 设置页不需要搜索栏（需求 #1）
  toolbar.style.display = isSettings ? "none" : "";
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
      <div class="empty-title">文件复制功能已关闭</div>
      <div class="empty-sub">需要记录资源管理器里复制的文件时，可在「设置 → 文件复制」中打开</div>
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
  refreshMissing(list); // 文件条目的原路径可能已被移动/删除 → 打「已失效」标记
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
    text: ["暂无文本", "复制的文本会自动记录到这里"],
    favorites: ["暂无收藏内容", "在条目上右键选择「收藏」，会出现在这里"],
    images: ["暂无图片", "复制的图片会自动记录到这里"],
    files: ["暂无文件", "复制的文件会自动记录到这里"],
  };
  const [title, sub] = titles[currentView] || titles.history;
  return `<div class="empty"><div class="empty-title">${title}</div>
    <div class="empty-sub">${sub || ""}</div></div>`;
}

function cardEl(item) {
  const card = document.createElement("div");
  const pinned = isPinnedIn(item, currentView); // 置顶按分类独立，只看当前分类
  card.className = "card" + (pinned ? " is-pinned" : "");
  card.dataset.id = item.id;

  // 内容区
  const body = document.createElement("div");
  body.className = "card-body";
  const meta = document.createElement("div");
  meta.className = "card-meta";
  meta.innerHTML = `<span class="card-kind">${KIND_LABEL[item.kind] || item.kind}</span>
    <span class="card-time" data-ts="${item.ts}">${timeStr(item.ts)}</span>${pinned ? '<span style="color:var(--accent)">已置顶</span>' : ""}`;
  body.appendChild(meta);

  if (item.kind === "image") {
    // 图片条目：内容区放小缩略图（不显示「[图片]」占位字），
    // 大图仍由鼠标悬浮时的浮层给出（见 bindImagePreview）
    const img = document.createElement("img");
    img.className = "card-image";
    img.alt = "图片";
    // 不在此处读图：交给 observeImages() 等卡片滚入可视区再加载，
    // 否则一次渲染几十张图会同时产生几十个大 BMP 字符串（内存峰值飙升）
    img.dataset.imgId = item.id;
    body.appendChild(img);
    bindImagePreview(card, item);
  } else {
    const preview = document.createElement("div");
    preview.className = "card-preview";
    preview.textContent = item.preview || "";
    body.appendChild(preview);
  }
  card.appendChild(body);

  // 左键 → 复制回剪贴板；右键 → 上下文菜单
  card.addEventListener("click", () => copyBack(item));
  card.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e.clientX, e.clientY, item);
  });
  return card;
}

// ---------------------------------------------------------------------------
// 图片悬浮预览
// 卡片布局与其它类型完全一致（图片条目只显示「[图片]」一行），
// 鼠标移入后再在卡片旁弹出大图。效率取舍：按需读取 + 内存缓存，
// 同一张图第二次悬浮不再走 IPC / 磁盘。
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
  const raw = await invoke("get_image", { id: id }); // 原始 BMP（大）→ 用完即弃
  const url = await shrinkImage(raw, IMAGE_MAX_EDGE);
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
            .catch(() => {
              img.dataset.imgLoaded = "0";
              img.remove(); // 文件缺失/损坏 → 不占位
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

function ensurePreviewBox() {
  if (previewBox) return previewBox;
  const box = document.createElement("div");
  box.className = "img-preview";
  box.hidden = true;
  const img = document.createElement("img");
  img.alt = "预览";
  box.appendChild(img);
  document.body.appendChild(box);
  previewBox = box;
  return box;
}

function hideImagePreview() {
  if (previewTimer) {
    clearTimeout(previewTimer);
    previewTimer = null;
  }
  if (previewBox && !previewBox.hidden) {
    previewBox.hidden = true;
    previewBox.firstChild.removeAttribute("src");
  }
}

/** 优先放卡片右侧；右侧空间不足改左侧；最后夹回视口内 */
function placePreview(box, card) {
  const r = card.getBoundingClientRect();
  const w = box.offsetWidth;
  const h = box.offsetHeight;
  const gap = 12;
  let x = r.right + gap;
  if (x + w > window.innerWidth - 8) x = r.left - gap - w;
  let y = r.top + r.height / 2 - h / 2;
  const clamp = (v, min, max) => Math.max(min, Math.min(v, max));
  x = clamp(x, 8, Math.max(8, window.innerWidth - w - 8));
  y = clamp(y, 8, Math.max(8, window.innerHeight - h - 8));
  box.style.left = `${x}px`;
  box.style.top = `${y}px`;
}

function bindImagePreview(card, item) {
  card.addEventListener("mouseenter", () => {
    if (previewTimer) clearTimeout(previewTimer);
    previewTimer = setTimeout(() => showImagePreview(card, item), 120);
  });
  card.addEventListener("mouseleave", hideImagePreview);
}

async function showImagePreview(card, item) {
  previewTimer = null;
  if (!card.isConnected || !card.matches(":hover")) return;

  let url;
  try {
    url = await loadImage(item.id);
  } catch {
    return; // 图片文件缺失/损坏 → 不弹预览
  }
  // 读取期间鼠标可能已移开
  if (!card.isConnected || !card.matches(":hover")) return;

  const box = ensurePreviewBox();
  const img = box.firstChild;
  // 图片尺寸只有加载完才准，加载后重新定位一次
  img.onload = () => {
    if (!box.hidden) placePreview(box, card);
  };
  img.src = url;
  box.hidden = false;
  placePreview(box, card);
}

async function copyBack(item) {
  try {
    // copy_back 返回可选的提示串（目前只有文件条目会返回：部分原文件已失效、已跳过）
    const warn = await invoke("copy_back", { id: item.id });
    toast(warn || "已复制到剪贴板");
  } catch (e) {
    toast("复制失败：" + e);
  }
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
      s.className = "tag-missing";
      s.textContent = "已失效";
      meta.appendChild(s);
    } else if (!missing && tag) {
      tag.remove();
    }
  });
}

// ---------------------------------------------------------------------------
// 右键上下文菜单
// ---------------------------------------------------------------------------

let ctxMenuEl = null;

function closeContextMenu() {
  if (ctxMenuEl) {
    ctxMenuEl.remove();
    ctxMenuEl = null;
  }
}

/** 通用浮层菜单。entries = [{ label, act, danger?, disabled? } | { sep: true }] */
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
    btn.textContent = entry.label;
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

function openContextMenu(x, y, item) {
  const pinnedHere = isPinnedIn(item, currentView);
  const viewName = { history: "历史", text: "文本", images: "图片", files: "文件", favorites: "收藏" }[currentView] || currentView;
  showMenu(x, y, [
    { label: "复制", act: () => copyBack(item) },
    {
      // 置顶按分类独立：这里动的是「当前分类」下的置顶状态
      label: pinnedHere ? `取消置顶（${viewName}）` : `置顶（${viewName}）`,
      act: async () => {
        await invoke("toggle_pin", { id: item.id, view: currentView });
        const vs = Array.isArray(item.pinnedViews) ? item.pinnedViews : (item.pinnedViews = []);
        const idx = vs.indexOf(currentView);
        if (idx >= 0) vs.splice(idx, 1);
        else vs.push(currentView);
        render();
      },
    },
    {
      label: item.favorite ? "取消收藏" : "收藏",
      act: async () => {
        await invoke("toggle_favorite", { id: item.id });
        item.favorite = !item.favorite;
        render();
      },
    },
    {
      label: "删除",
      danger: true,
      act: async () => {
        await invoke("delete_item", { id: item.id });
        items = items.filter((i) => i.id !== item.id);
        render();
      },
    },
  ]);
}

// 点空白处 / 滚动 / Esc → 关闭；非卡片区域右键 → 只关不弹
document.addEventListener("click", closeContextMenu);
document.addEventListener(
  "scroll",
  () => {
    closeContextMenu();
    hideImagePreview();
  },
  true
);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closeContextMenu();
    hideImagePreview();
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

function settingsHtml() {
  const s = settings || {};
  const fmtMB = (b) => (b / 1024 / 1024).toFixed(1);
  return `<div class="settings">
    <h2>设置</h2>
    <div class="row"><label>文件复制</label>
      <input type="checkbox" id="set-file-copy" ${s.fileCopy ? "checked" : ""} />
      <span class="hint">开启后记录在资源管理器里复制的文件（默认关闭，关闭时不采集也不显示文件条目）</span></div>
    <div class="row"><label>开机自启</label>
      <input type="checkbox" id="set-autostart" ${s.autoStart ? "checked" : ""} />
      <span class="hint">登录 Windows 后自动运行轻贴</span></div>
    <div class="row"><label>关闭时后台运行</label>
      <input type="checkbox" id="set-tray" ${s.trayOnClose ? "checked" : ""} />
      <span class="hint">点关闭按钮时隐藏到托盘继续后台运行（托盘图标始终常驻，右键可开菜单、双击唤回窗口）</span></div>
    <div class="row"><label>保留天数</label>
      <input type="number" id="set-retention" min="0" value="${s.retentionDays ?? 3}" />
      <span class="hint">仅保留近 N 天内容，0 表示永久保留</span></div>
    <div class="row"><label>材质浓度</label>
      <input type="range" id="set-material" min="25" max="90" value="${s.materialAlpha ?? 62}" />
      <span class="hint" id="material-val">${s.materialAlpha ?? 62}%（越高越厚实 · 越低越通透）</span></div>
    <div class="row"><label>圆角模式</label>
      <input type="checkbox" id="set-round" ${s.roundMode ? "checked" : ""} />
      <span class="hint">勾选＝圆角 + 无背景模糊（四角完全干净）；不勾选＝直角 + 保留背景模糊</span></div>
    <div class="cache-box">
      <div class="cache-title">缓存目录 <span class="hint">仅保留近 ${s.retentionDays ?? 3} 天内容</span></div>
      <div class="cache-path" id="cache-path">${s.cacheDir || "加载中…"}</div>
      <div class="cache-stats" id="cache-stats"></div>
      <div class="cache-actions">
        <button class="btn-ghost" id="btn-pick-dir">更改目录</button>
        <button class="btn-ghost" id="btn-open-dir">打开目录</button>
      </div>
    </div>
    <div class="row"><label>关闭时清理缓存</label>
      <input type="checkbox" id="set-clean" ${s.cleanOnClose ? "checked" : ""} />
      <span class="hint">退出时清理内容缓存与历史（置顶/收藏默认保留）</span></div>
    <div class="row"><label>　└ 同时清理置顶/收藏</label>
      <input type="checkbox" id="set-clean-pinned" ${s.cleanIncludePinned ? "checked" : ""} />
      <span class="hint">不勾选则置顶/收藏单独存放，不受清理影响</span></div>
    <div class="row"><label>　└ 退出前弹窗提醒</label>
      <input type="checkbox" id="set-clean-confirm" ${s.cleanConfirmExit ? "checked" : ""} />
      <span class="hint">每次退出时先弹窗确认</span></div>
    <button class="btn-danger" id="clear-all">清空全部历史</button>
  </div>`;
}

async function refreshCacheInfo() {
  try {
    const info = await invoke("cache_info");
    const pathEl = document.getElementById("cache-path");
    const statsEl = document.getElementById("cache-stats");
    if (pathEl) pathEl.textContent = info.dir;
    if (statsEl)
      statsEl.textContent = `${info.count} 个文件 · ${fmtSize(info.size)}`;
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
  const bindCheck = (id, cmd) => {
    const el = view.querySelector("#" + id);
    if (!el) return;
    el.onchange = async () => {
      try {
        await invoke(cmd, { value: el.checked });
        // 同步本地缓存（退出弹窗文案等依赖最新配置）
        settings = await invoke("get_settings");
        toast(el.checked ? "已开启" : "已关闭");
      } catch (e) {
        toast("保存失败：" + e);
        el.checked = !el.checked;
      }
    };
  };
  bindCheck("set-autostart", "set_autostart");
  bindCheck("set-tray", "set_tray_on_close");
  bindCheck("set-file-copy", "set_file_copy");
  bindCheck("set-clean", "set_clean_on_close");
  bindCheck("set-clean-pinned", "set_clean_include_pinned");
  bindCheck("set-clean-confirm", "set_clean_confirm_exit");

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
        toast(days === 0 ? "已改为永久保留" : `仅保留近 ${days} 天内容`);
      } catch (e) {
        toast("保存失败：" + e);
      }
    };

  const material = view.querySelector("#set-material");
  if (material) {
    const label = view.querySelector("#material-val");
    material.addEventListener("input", () => {
      const v = parseInt(material.value, 10);
      if (label) label.textContent = `${v}%（越高越厚实 · 越低越通透）`;
      applyGlassAlpha(v); // 实时生效（CSS 层，无需重启）
      invoke("set_material_alpha", { value: v }).catch(() => {});
    });
  }

  // 圆角模式：切换后立即改动窗口材质（Rust）+ CSS 层观感，无需重启。
  // 两种模式对「厚度」的要求不同，所以顺带把材质浓度也切到对应的档：
  //   圆角模式没有背景模糊 → 半透就等于直接看到桌面，要更实（90）
  //   直角模式有真模糊      → 50% 更通透好看
  const round = view.querySelector("#set-round");
  if (round)
    round.onchange = async () => {
      try {
        await invoke("set_round_mode", { value: round.checked });
        const alpha = round.checked ? 90 : 50;
        await invoke("set_material_alpha", { value: alpha });
        applyGlassAlpha(alpha);
        const slider = view.querySelector("#set-material");
        if (slider) slider.value = alpha;
        const label = view.querySelector("#material-val");
        if (label) label.textContent = `${alpha}%（越高越厚实 · 越低越通透）`;
        applyRoundMode(round.checked);
        settings = await invoke("get_settings");
        toast(
          round.checked
            ? "已切换为圆角模式（无背景模糊）"
            : "已切换为直角模式（保留背景模糊）"
        );
      } catch (e) {
        toast("保存失败：" + e);
        round.checked = !round.checked;
      }
    };

  view.querySelector("#btn-pick-dir").onclick = async () => {
    try {
      const res = await invoke("pick_cache_dir");
      if (res) toast("缓存目录已迁移，即将重启…");
    } catch (e) {
      toast("更改目录失败：" + e);
    }
  };
  view.querySelector("#btn-open-dir").onclick = () =>
    invoke("open_cache_dir").catch((e) => toast("打开失败：" + e));

  const clear = view.querySelector("#clear-all");
  if (clear)
    clear.onclick = async () => {
      await invoke("clear_all");
      items = [];
      render();
      toast("已清空");
    };

  refreshCacheInfo();
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
  bindClearView();
  bindNav();
  bindSearch();
  startTimeTicker();

  await applyEffectInfo();

  // 主题相关：单独 try，避免权限缺失时中断其余绑定
  try {
    applyTheme((await appWindow.theme().catch(() => "light")) === "dark");
    await appWindow.onThemeChanged(({ payload }) => applyTheme(payload === "dark"));
  } catch (e) {
    console.error("theme setup failed", e);
  }

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
    applyGlassAlpha(settings.materialAlpha ?? 62);
    // 圆角模式（圆角 + 无系统模糊）
    applyRoundMode(settings.roundMode);
  } catch (e) {
    console.error("load settings failed", e);
  }
  try {
    document.getElementById("status-version").textContent =
      `Version：${(await getVersion())}-ikun`;
  } catch (e) {
    document.getElementById("status-version").textContent = "Version：2.0.0-ikun";
  }

  render();

  // 增量：复制新内容时 Rust 推事件，直接插到顶部
  await listen("clipboard-update", (e) => {
    const item = e.payload;
    if (!items.some((i) => i.id === item.id)) {
      items.unshift(item);
      render();
    }
  });
}

main();
