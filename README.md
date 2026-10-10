# 轻贴 — 本地剪贴板管理工具

轻贴（self_clipboard）是一款 Windows 桌面剪贴板历史管理工具。

- **v2（当前，`main`）**：Tauri 2 + 系统 WebView2，后台管理风格 UI，Win10 起支持真·毛玻璃。
- **v1（已归档）**：Electron 实现，保留在 `legacy/electron-v1` 分支，tag `v1.3.11`。

详细设计见 [DESIGN.md](./DESIGN.md)，v1 的审查结论见 [CODE_REVIEW.md](./CODE_REVIEW.md)。

## 技术栈

| 技术 | 用途 |
|------|------|
| Tauri 2 | 桌面框架（Rust + 系统 WebView2，安装包 < 10MB） |
| Rust | 剪贴板监听、窗口材质、存储 |
| HTML/CSS/JS | 前端 UI（原生，无框架） |
| Vite | 前端开发服务器与打包 |

## 开发

> **环境要求：必须用系统 Node（E:\Program Files\nodejs）。**
> 托管的 Node 在调用 esbuild 时会 `EBUSY` 失败（`spawnSync` 被拦截）。

```bash
# 前端依赖
npm install

# 开发运行（会拉起 Vite + Tauri 窗口）
npm run tauri:dev

# 构建安装包
npm run tauri:build
```

## 目录

```
src-tauri/          Rust 后端
  src/effects.rs    Windows 材质：按 build 号分档（acrylic / mica / system-backdrop / solid）
  src/lib.rs        应用装配与 IPC 命令
src/               前端
index.html         入口
```

## 材质分档

| Windows | build | 材质 | 圆角 |
|---|---|---|---|
| Win11 22H2+ | ≥ 22621 | `DWMWA_SYSTEMBACKDROP_TYPE` | 系统圆角 |
| Win11 21H2 | 22000 | Mica | 系统圆角 |
| Win10 1809–22H2 | 17763–19045 | `SetWindowCompositionAttribute` 真·acrylic | **≤ 10px** |
| 更低版本 / 失败 | — | 不透明兜底 | 12px |

启动时终端会打印实际 build 号与实际生效档位；不接受「调用没报错就算成功」。
