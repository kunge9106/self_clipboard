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
let paused = false;

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
  document.getElementById("btn-minimize").onclick = () => appWindow.minimize();
  document.getElementById("btn-maximize").onclick = () =>
    appWindow.toggleMaximize();
  document.getElementById("btn-close").onclick = () => appWindow.close();
}

function bindNav() {
  document.getElementById("nav").addEventListener("click", (e) => {
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

function bindPause() {
  const btn = document.getElementById("btn-pause");
  const label = document.getElementById("pause-label");
  btn.onclick = async () => {
    paused = !paused;
    label.textContent = paused ? "已暂停" : "录制中";
    btn.style.opacity = paused ? "0.5" : "1";
    try {
      await invoke("set_paused", { paused });
    } catch (e) {
      console.error(e);
    }
  };
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
const KIND_ICON = { text: "📝", image: "🖼", files: "📁" };

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

  // 缩略区
  const thumb = document.createElement("div");
  thumb.className = "card-thumb";
  if (item.kind === "image") {
    const img = document.createElement("img");
    img.alt = "图片";
    img.loading = "lazy";
    thumb.appendChild(img);
    invoke("get_image", { id: item.id })
      .then((b64) => {
        img.src = b64;
      })
      .catch(() => {
        thumb.textContent = "🖼";
      });
  } else {
    thumb.textContent = KIND_ICON[item.kind] || "📄";
  }

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

  // 操作区
  const actions = document.createElement("div");
  actions.className = "card-actions";
  actions.appendChild(
    actionBtn("fav", "★", item.favorite, async () => {
      await invoke("toggle_favorite", { id: item.id });
      item.favorite = !item.favorite;
      render();
    })
  );
  actions.appendChild(
    actionBtn("pin", "📌", item.pinned, async () => {
      await invoke("toggle_pin", { id: item.id });
      item.pinned = !item.pinned;
      render();
    })
  );
  actions.appendChild(
    actionBtn("del", "🗑", false, async () => {
      await invoke("delete_item", { id: item.id });
      items = items.filter((i) => i.id !== item.id);
      render();
    })
  );

  // 点击卡片（非按钮）→ 复制回剪贴板
  card.addEventListener("click", (e) => {
    if (e.target.closest(".act")) return;
    copyBack(item);
  });

  card.appendChild(thumb);
  card.appendChild(body);
  card.appendChild(actions);
  return card;
}

function actionBtn(cls, glyph, on, onClick) {
  const b = document.createElement("button");
  b.className = "act " + cls + (on ? " is-on" : "");
  b.textContent = glyph;
  b.title = cls === "fav" ? "收藏" : cls === "pin" ? "置顶" : "删除";
  b.onclick = onClick;
  return b;
}

async function copyBack(item) {
  try {
    await invoke("copy_back", { id: item.id });
    toast(item.kind === "image" ? "图片写回暂未支持" : "已复制到剪贴板");
  } catch (e) {
    toast("复制失败：" + e);
  }
}

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
      <span class="hint">用 Ctrl+` 唤起/隐藏窗口（开关功能开发中）</span></div>
    <div class="row"><label>录制状态</label>
      <button class="btn-ghost" id="set-pause">${paused ? "已暂停 → 恢复" : "录制中 → 暂停"}</button></div>
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
  const pause = view.querySelector("#set-pause");
  if (pause)
    pause.onclick = async () => {
      paused = !paused;
      try {
        await invoke("set_paused", { paused });
      } catch (e) {}
      pause.textContent = paused ? "已暂停 → 恢复" : "录制中 → 暂停";
    };
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function main() {
  await applyEffectInfo();
  applyTheme(await appWindow.theme().catch(() => "light") === "dark");

  await appWindow.onThemeChanged(({ payload }) => applyTheme(payload === "dark"));

  bindWindowControls();
  bindNav();
  bindSearch();
  bindPause();

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
