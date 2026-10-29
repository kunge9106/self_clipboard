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
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

const win = getCurrentWindow();
const hide = () => win.hide().catch(() => {});

/**
 * 应用外观参数。参数由 Rust 端算好（`tray_menu_look` / `tray-menu-open` 事件的
 * payload），页面这边不做任何读取 —— 一是少两次 IPC，二是保证「弹出即最新」：
 * 用户改完材质浓度 / 圆角模式，下次右键立刻就是新观感。
 *
 * 形状：{ solid, round, alpha, dark }（round 已不用：菜单恒为圆角外观）
 */
function applyLook(look) {
  if (!look) return;
  document.body.classList.toggle("dark", !!look.dark);
  document.body.classList.toggle("tier-solid", !!look.solid);

  // 菜单**恒为圆角外观**（用户要求）：不管主窗口开没开圆角模式，托盘菜单
  // 都用「圆角模式」那一套材质与样式 —— CSS 圆角 + 无系统 blur（Rust 侧
  // apply_rounded 已把系统材质关干净，四角不会残留直角）。
  document.documentElement.style.setProperty("--menu-radius", "12px");

  if (look.solid) {
    // 兜底档：没有 acrylic，用 CSS 里的不透明底色（body.tier-solid）
    document.documentElement.style.removeProperty("--glass-alpha");
  } else {
    document.documentElement.style.setProperty(
      "--glass-alpha",
      String(Math.max(0.1, Math.min(0.95, Number(look.alpha) || 0.62)))
    );
  }
}

(async () => {
  // 首次加载先自己拉一次（此时窗口正在被创建，弹出来就已经是正确观感）
  try {
    applyLook(await invoke("tray_menu_look"));
  } catch {
    /* 拉不到就先用 CSS 默认值 */
  }

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

  // 每次弹出都由 Rust 把当前外观参数推过来（菜单页常驻，不推的话改完设置不会变）
  await listen("tray-menu-open", (e) => applyLook(e.payload));

  // 悬浮态防残留：与主窗口同一套机制（CSS 里 :hover 都挂在 html:not(.hovers-off) 下）。
  // 菜单窗口是**复用**的，上次收起时悬停的那一项会被 Chromium 一直当作 :hover，
  // 再弹出来就带着高亮。所以每次弹出先屏蔽，鼠标真动过再放开。
  const html = document.documentElement;
  const off = () => html.classList.add("hovers-off");
  const on = () => html.classList.remove("hovers-off");
  window.addEventListener("mousemove", on, { passive: true });
  window.addEventListener("mousedown", on, { passive: true });
  on();

  // 点别处 → 窗口失焦 → 自动收起（菜单的通用行为）
  await win.onFocusChanged(({ payload: focused }) => {
    if (focused) off();
    else hide();
  });
})();
