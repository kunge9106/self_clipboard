import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

const appWindow = getCurrentWindow();

/** 材质档位 → 实际可用的圆角上限（由 Rust 端判定，前端不自行猜测） */
const TIER_CLASS = {
  "system-backdrop": "tier-system-backdrop",
  mica: "tier-mica",
  acrylic: "tier-acrylic",
  solid: "tier-solid",
};

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

  // Win11 可用系统圆角，放宽；其余档位用 Rust 给的 maxRadius
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
  });
}

async function main() {
  await applyEffectInfo();
  applyTheme(await appWindow.theme().catch(() => "light") === "dark");

  // 跟随系统主题
  await appWindow.onThemeChanged(({ payload }) => applyTheme(payload === "dark"));

  bindWindowControls();
  bindNav();
}

main();
