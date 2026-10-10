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
let ageDays = 0; // 时间筛选：只显示近 N 天（0 = 全部）
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

  // 最大化 ↔ 还原：图标与提示词跟随窗口状态（需求 #6）
  const maxBtn = document.getElementById("btn-maximize");
  const syncMaxState = async () => {
    const max = await appWindow.isMaximized().catch(() => false);
    maxBtn.classList.toggle("is-max", max);
    maxBtn.title = max ? "还原" : "最大化";
  };
  appWindow.onResized(() => syncMaxState());
  syncMaxState();
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

/**
 * 垃圾桶：点击弹出清理菜单。
 *   - 「清空当前列表」只清当前视图 + 搜索 + 时间筛选命中的条目
 *   - 「清理 N 天前的内容」按时间清理更早的内容（用户要的「清理多久以前的」）
 * 两者都豁免置顶/收藏；菜单项带条数预览，点之前就知道会删多少。
 */
function bindClearView() {
  const btn = document.getElementById("btn-clear-view");
  btn.onclick = (e) => {
    e.stopPropagation(); // 拦下 document 的 click，否则菜单会被立刻关掉

    const cleanable = items.filter((i) => !i.pinned && !i.favorite);
    const visible = filtered().filter((i) => !i.pinned && !i.favorite);
    const olderCount = (days) => {
      const cutoff = Date.now() - days * 86_400_000;
      return cleanable.filter((i) => i.ts < cutoff).length;
    };

    const entries = [
      {
        label: visible.length
          ? `清空当前列表（${visible.length} 条）`
          : "清空当前列表",
        danger: true,
        disabled: visible.length === 0,
        act: () => clearVisible(visible),
      },
      { sep: true },
    ];
    for (const days of [1, 7, 30]) {
      const n = olderCount(days);
      entries.push({
        label: `清理 ${days} 天前的内容（${n} 条）`,
        disabled: n === 0,
        act: () => clearOlder(days),
      });
    }

    const r = btn.getBoundingClientRect();
    showMenu(r.left - 176, r.top - 190, entries);
  };
}

async function clearVisible(targets) {
  if (targets.length === 0) {
    toast("没有可清理的条目（置顶/收藏除外）");
    return;
  }
  try {
    await invoke("delete_items", { ids: targets.map((i) => i.id) });
    items = items.filter((i) => !targets.some((t) => t.id === i.id));
    render();
    toast(`已清理 ${targets.length} 条`);
  } catch (e) {
    toast("清理失败：" + e);
  }
}

async function clearOlder(days) {
  try {
    const n = await invoke("clear_older_than", { days });
    items = await invoke("get_history");
    render();
    toast(n > 0 ? `已清理 ${n} 条（${days} 天前）` : `${days} 天前没有可清理的内容`);
  } catch (e) {
    toast("清理失败：" + e);
  }
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

/** 时间筛选：只显示近 N 天复制的内容 */
function bindAgeFilter() {
  const el = document.getElementById("age-filter");
  if (!el) return;
  el.addEventListener("change", () => {
    ageDays = parseInt(el.value, 10) || 0;
    render();
  });
}

// ---------------------------------------------------------------------------
// 历史渲染
// ---------------------------------------------------------------------------

function filtered() {
  let list = items;
  if (currentView === "pinned") list = list.filter((i) => i.favorite);
  else if (currentView === "images") list = list.filter((i) => i.kind === "image");
  else if (currentView === "files") list = list.filter((i) => i.kind === "files");

  if (query) {
    list = list.filter(
      (i) =>
        (i.preview && i.preview.toLowerCase().includes(query)) ||
        (i.files && i.files.some((f) => f.toLowerCase().includes(query)))
    );
  }
  // 时间筛选（近 N 天）
  if (ageDays > 0) {
    const cutoff = Date.now() - ageDays * 86_400_000;
    list = list.filter((i) => i.ts >= cutoff);
  }
  // pinned 优先，其余按时间倒序
  return [...list].sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
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
  // 设置页不需要搜索栏（需求 #1）
  toolbar.style.display = currentView === "settings" ? "none" : "";
  if (currentView === "settings") {
    view.innerHTML = settingsHtml();
    bindSettings(view);
    return;
  }

  const list = filtered();
  // 状态栏：搜索/筛选时显示命中条数，否则显示总数
  const countEl = document.getElementById("status-count");
  if (countEl) {
    if (query) countEl.textContent = `找到 ${list.length} 条结果`;
    else if (ageDays > 0) countEl.textContent = `近 ${ageDays} 天 ${list.length} 条`;
    else countEl.textContent = `共 ${items.length} 条`;
  }

  if (list.length === 0) {
    view.innerHTML = emptyHtml();
    return;
  }

  const wrap = document.createElement("div");
  wrap.className = "history";
  for (const item of list) {
    wrap.appendChild(cardEl(item));
  }
  view.innerHTML = "";
  view.appendChild(wrap);
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
  if (ageDays > 0) {
    return `<div class="empty"><div class="empty-title">近 ${ageDays} 天内暂无内容</div>
      <div class="empty-sub">换个时间范围看看</div></div>`;
  }
  const titles = {
    history: ["暂无内容", "复制点东西试试，会自动记录到这里"],
    pinned: ["暂无收藏内容", "在条目上右键选择「收藏」，会出现在这里"],
    images: ["暂无图片", "复制的图片会自动记录到这里"],
    files: ["暂无文件", "复制的文件会自动记录到这里"],
  };
  const [title, sub] = titles[currentView] || titles.history;
  return `<div class="empty"><div class="empty-title">${title}</div>
    <div class="empty-sub">${sub || ""}</div></div>`;
}

function cardEl(item) {
  const card = document.createElement("div");
  card.className = "card" + (item.pinned ? " is-pinned" : "");
  card.dataset.id = item.id;

  // 内容区
  const body = document.createElement("div");
  body.className = "card-body";
  const meta = document.createElement("div");
  meta.className = "card-meta";
  meta.innerHTML = `<span class="card-kind">${KIND_LABEL[item.kind] || item.kind}</span>
    <span class="card-time" data-ts="${item.ts}">${timeStr(item.ts)}</span>${item.pinned ? '<span style="color:var(--accent)">已置顶</span>' : ""}`;
  body.appendChild(meta);

  if (item.kind === "image") {
    // 图片条目：内容区放小缩略图（不显示「[图片]」占位字），
    // 大图仍由鼠标悬浮时的浮层给出（见 bindImagePreview）
    const img = document.createElement("img");
    img.className = "card-image";
    img.alt = "图片";
    img.loading = "lazy";
    loadImage(item.id)
      .then((url) => {
        img.src = url;
      })
      .catch(() => img.remove()); // 文件缺失/损坏 → 不占位
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

/** id → dataURL，已读过的图片缓存，避免反复读取 */
const imageCache = new Map();
const IMAGE_CACHE_MAX = 24;
let previewBox = null;
let previewTimer = null;

/** 读取图片（dataURL）；命中缓存则不再走 IPC / 磁盘。列表缩略图与悬浮预览共用。 */
async function loadImage(id) {
  const hit = imageCache.get(id);
  if (hit) return hit;
  const url = await invoke("get_image", { id: id });
  imageCache.set(id, url);
  // 只保留最近若干张，避免长时间使用后 dataURL 撑爆内存
  if (imageCache.size > IMAGE_CACHE_MAX) {
    imageCache.delete(imageCache.keys().next().value);
  }
  return url;
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
    await invoke("copy_back", { id: item.id });
    toast("已复制到剪贴板");
  } catch (e) {
    toast("复制失败：" + e);
  }
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
  showMenu(x, y, [
    { label: "复制", act: () => copyBack(item) },
    {
      label: item.pinned ? "取消置顶" : "置顶",
      act: async () => {
        await invoke("toggle_pin", { id: item.id });
        item.pinned = !item.pinned;
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
    <div class="row"><label>开机自启</label>
      <input type="checkbox" id="set-autostart" ${s.autoStart ? "checked" : ""} />
      <span class="hint">登录 Windows 后自动运行轻贴</span></div>
    <div class="row"><label>关闭时后台运行</label>
      <input type="checkbox" id="set-tray" ${s.trayOnClose ? "checked" : ""} />
      <span class="hint">点关闭按钮时隐藏到托盘，双击托盘图标可唤回</span></div>
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
  bindDrag();
  bindResize();
  bindExitModal();
  bindClearView();
  bindNav();
  bindSearch();
  bindAgeFilter();
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
