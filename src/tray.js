/**
 * 托盘右键菜单窗口（tray.html）的脚本。
 *
 * 这个窗口由 Rust 端在第一次右键托盘时创建，之后只显示/隐藏，不再重建。
 * 职责只有三件：对齐外观（材质浓度 / 圆角模式 / 深色）、响应点击、失焦自动收起。
 *
 * 注意：整体包在 async IIFE 里，不用顶层 await —— vite 的构建目标包含 safari13，
 * 顶层 await 会因为目标环境不支持而直接构建失败。
 */
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

const win = getCurrentWindow();
const hide = () => win.hide().catch(() => {});

/** 材质浓度与圆角模式：读主窗口同一份配置，保持观感一致 */
async function applyLook() {
  let info = null;
  try {
    info = await invoke("effect_info");
  } catch {
    /* 读不到就按 solid 处理，下面会退回不透明底色 */
  }
  const solid = !info || info.tier === "solid";
  document.body.classList.toggle("tier-solid", solid);

  // 直角模式（默认）→ 菜单也是直角：窗口矩形与系统 acrylic 完全吻合，四角零残留；
  // 圆角模式 → 没有系统 blur，圆角由 CSS 负责，可以放心圆。
  const radius = info && info.round ? 12 : 0;
  document.documentElement.style.setProperty("--menu-radius", radius + "px");

  if (!solid) {
    try {
      const s = await invoke("get_settings");
      const a = Math.max(10, Math.min(95, Number(s.materialAlpha) || 62)) / 100;
      document.documentElement.style.setProperty("--glass-alpha", String(a));
    } catch {
      /* 用 CSS 里的默认值 */
    }
  }
}

(async () => {
  // 深色模式：与主窗口同一套 body.dark 变量
  document.body.classList.toggle(
    "dark",
    (await win.theme().catch(() => "light")) === "dark"
  );
  await applyLook();

  document.getElementById("tray-show").onclick = () => {
    hide(); // 先收菜单，避免主窗口抢焦点时菜单还糊在上面
    invoke("tray_show_main").catch(() => {});
  };

  document.getElementById("tray-quit").onclick = () => {
    invoke("tray_quit").catch(() => {});
  };

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hide();
  });

  // 点别处 → 窗口失焦 → 自动收起（菜单的通用行为）
  await win.onFocusChanged(({ payload: focused }) => {
    if (!focused) hide();
  });
})();
