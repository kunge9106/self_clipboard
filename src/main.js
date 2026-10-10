import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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

  const radius =
    info.tier === "system-backdrop" ? 12 : Math.min(info.maxRadius || 10, 12);
  document.documentElement.style.setProperty("--radius", radius + "px");
  document.documentElement.style.setProperty(
    "--radius-sm",
    Math.max(6, radius - 2) + "px"
  );

  document.getElementById("effect-debug").textContent =
    `build ${info.build} · tier ${info.tier} · radius ${radius}px`;
}

function applyTheme(dark) {
  document.body.classList.toggle("dark", !!dark);
}

function bindWindowControls() {
  const guard = (label, p) =>
    p.catch((err) => toast(label + "失败：" + err));
  document.getElementById("btn-minimize").onclick = () =>
    guard("最小化", appWindow.minimize());
  document.getElementById("btn-maximize").onclick = () =>
    guard("最大化", appWindow.toggleMaximize());
  document.getElementById("btn-close").onclick = () =>
    guard("关闭", appWindow.close());
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
  // pinned 优先，其余按时间倒序
  return [...list].sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    return b.ts - a.ts;
  });
}

function timeStr(ts) {
  const d = new Date(ts);
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}`;
}

const KIND_LABEL = { text: "文本", image: "图片", files: "文件" };

function render() {
  const view = document.getElementById("view");
  if (currentView === "settings") {
    view.innerHTML = settingsHtml();
    bindSettings(view);
    return;
  }

  const list = filtered();
  if (list.length === 0) {
    view.innerHTML = `<div class="empty"><div class="empty-title">暂无内容</div>
      <div class="empty-sub">复制点东西试试，会自动记录到这里</div></div>`;
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
    <span>${timeStr(item.ts)}</span>${item.pinned ? '<span style="color:var(--accent)">已置顶</span>' : ""}`;
  const preview = document.createElement("div");
  preview.className = "card-preview";
  preview.textContent = item.preview || "";
  body.appendChild(meta);
  body.appendChild(preview);
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

async function copyBack(item) {
  try {
    await invoke("copy_back", { id: item.id });
    toast(item.kind === "image" ? "图片写回暂未支持" : "已复制到剪贴板");
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

function openContextMenu(x, y, item) {
  closeContextMenu();

  const menu = document.createElement("div");
  menu.className = "ctx-menu";

  const entries = [
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
  ];

  for (const entry of entries) {
    const btn = document.createElement("button");
    btn.className = "ctx-item" + (entry.danger ? " danger" : "");
    btn.textContent = entry.label;
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

// 点空白处 / 滚动 / Esc → 关闭；非卡片区域右键 → 只关不弹
document.addEventListener("click", closeContextMenu);
document.addEventListener("scroll", closeContextMenu, true);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeContextMenu();
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
  return `<div class="settings">
    <h2>设置</h2>
    <div class="row"><label>自动清理（天）</label>
      <input type="number" min="0" value="30" />
      <span class="hint">0 表示永不自动清理（收藏/置顶除外，永久保留）</span></div>
    <div class="row"><label>全局热键</label>
      <input type="checkbox" checked />
      <span class="hint">用 Ctrl+反引号(\`) 唤起/隐藏窗口（开关功能开发中）</span></div>
    <button class="btn-danger" id="clear-all">清空全部历史</button>
  </div>`;
}

function bindSettings(view) {
  const clear = view.querySelector("#clear-all");
  if (clear)
    clear.onclick = async () => {
      await invoke("clear_all");
      items = [];
      render();
      toast("已清空");
    };
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function main() {
  // 控件绑定优先（即便后续异步步骤出错，也能保证可交互）
  bindWindowControls();
  bindNav();
  bindSearch();

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
