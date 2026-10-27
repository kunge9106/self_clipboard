import { defineConfig } from "vite";
import { resolve } from "node:path";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 5174 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    // 多入口：index.html 主界面，tray.html 托盘菜单，panel.html 快捷面板，shot.html 截图覆盖层。
    // 后三个由 Tauri 端按 `WebviewUrl::App("…")` 加载，必须一起产出到 dist/。
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        tray: resolve(__dirname, "tray.html"),
        panel: resolve(__dirname, "panel.html"),
      },
    },
  },
});
