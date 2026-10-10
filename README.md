<div align="center">
  <img src="assets/readme/banner.svg" alt="轻贴 —— 轻量、本地优先的 Windows 剪贴板管理器" width="820">
  <p><b>复制过的东西，随时找回来。</b><br/>文本、图片、文件全部留在本机，安全、干净、即取即用。</p>

  <p>
    <a href="https://github.com/kunge9106/self_clipboard/releases/latest"><img src="https://img.shields.io/github/v/release/kunge9106/self_clipboard?label=release&sort=semver&color=2ea043" alt="Release"></a>
    <img src="https://img.shields.io/badge/platform-Windows%2010%2B-0a7cd6" alt="Platform">
    <img src="https://img.shields.io/badge/Tauri-2-24c8db?logo=tauri&logoColor=white" alt="Tauri 2">
    <img src="https://img.shields.io/badge/installer-1.3%20MB-4b6fe0" alt="Installer size">
  </p>
</div>

---

## 这是什么

**轻贴**是一款 Windows 桌面剪贴板历史管理器。它安静地待在托盘里，把你复制过的每一条内容记下来——文本、图片、文件——需要的时候打开窗口，搜一下、点一下，内容就回到剪贴板了。

它是一个**单机小工具**：没有账号、没有云端、没有网络请求。所有内容都写在你自己指定的目录里，随时可以打开看、随时可以清掉。

> **轻**是字面意思：安装包 **1.3 MB**，常驻内存约 **120 MB**（同类 Electron 方案的安装包通常在 80–150 MB）。上一代基于 Electron 的 v1 安装包是 89 MB，这一代缩小了约 70 倍。

## 界面预览

![轻贴主界面](assets/readme/preview.svg)

左侧是分类导航，右侧是搜索框 + 历史列表，底部是条数与版本。卡片上右键就能复制、置顶、收藏、删除。

## 功能

**采集**

- 文本、图片、文件（资源管理器里复制的文件）
- 事件驱动监听，不是轮询——空闲时几乎不占 CPU
- 内容全部落盘，内存里只保留索引元数据

**查找**

- 实时搜索，文本正文与文件名都能命中
- 五个分类：历史 / 文本 / 图片 / 文件 / 收藏
- 置顶**按分类独立**：在「文本」里置顶，不会影响它在「历史」里的位置

**整理**

- 收藏与置顶，清理历史时自动豁免
- 保留天数可调（0 = 永久保留）
- 缓存目录可迁移，实时显示占用大小
- 文件条目失效检测：原文件被移动或删除会标「已失效」，点击时明确告诉你跳过了哪几个

**外观**

- 跟随系统深色模式
- 「材质浓度」滑块，实时调节窗口通透度
- 「圆角模式」开关，在圆角通透与直角毛玻璃之间切换（见下）

**后台**

- 托盘图标常驻，右键菜单与主界面同一套样式与材质
- 开机自启
- 关闭时后台运行（隐藏到托盘，而不是退出）
- 退出时可自动清理缓存，可选是否连置顶/收藏一起清

## 下载安装

前往 [Releases](https://github.com/kunge9106/self_clipboard/releases/latest) 下载 `light_clip_v2.0.0_setup.exe`，双击安装即可。

- 安装器为中文界面。
- 需要 **Windows 10 (1809) 或更高版本**。
- 安装包使用 WebView2 **在线引导**模式。Win11 与较新的 Win10 已内置 WebView2；若系统没有，安装时会联网下载引导包。

## 快速上手

1. 安装后从开始菜单或托盘打开轻贴，窗口会显示当前已记录的内容（首次为空）。
2. 随便复制一段文字或一张图片，回到窗口即可看到新条目。
3. 在条目上**右键**：复制 / 置顶 / 收藏 / 删除。
4. 想找东西就在顶部搜索框打字。
5. 想记录资源管理器里复制的文件，去 **设置 → 文件复制** 打开开关（默认关闭）。

> **关于「文件复制」默认关闭**：文件条目记录的是文件路径引用，而不是文件副本；原文件被移动或改名后就粘不出来了。因为它牵涉真实磁盘路径，默认不采集，需要时再打开。

## 材质与圆角

窗口材质按系统版本自动分档，启动时会在终端打印**实际生效**的档位（不接受「调用没报错就算成功」）：

| 系统 | build | 材质 | 圆角上限 |
|---|---|---|---|
| Win11 22H2+ | ≥ 22621 | `DWMWA_SYSTEMBACKDROP_TYPE` 系统级 acrylic | 12px（系统圆角） |
| Win11 21H2 | 22000 | Mica | 12px（系统圆角） |
| Win10 1809 – 22H2 | 17763 – 19045 | `SetWindowCompositionAttribute` 真·acrylic | 20px |
| 更低版本 / 调用失败 | — | 不透明兜底 | 12px |

在 Win10 上，「圆角」和「系统模糊」有个无法同时满足的地方：acrylic 始终铺满**窗口矩形**，`SetWindowRgn` 对它无效，所以圆角越大，四角露出的方形模糊就越多。轻贴把这个取舍交给你：

- **圆角模式（开）**：关掉系统模糊，窗口四角真透明、100% 干净，代价是背后的壁纸不再被模糊。
- **直角模式（默认）**：保留系统毛玻璃，窗口用直角，矩形与 acrylic 完全吻合，同样没有任何残留。

## 开发

**环境要求**

- Rust（stable，MSVC 工具链）
- Node.js 18+
- WebView2 Runtime

> 注意：本项目在 Windows 上**使用系统 Node**（而非某些托管环境自带的 Node）构建前端，托管 Node 调用 esbuild 时可能因 `spawnSync` 被拦截而 `EBUSY` 失败。

```bash
# 安装前端依赖
npm install

# 开发运行（自动拉起 Vite 与 Tauri 窗口）
npm run tauri:dev

# 构建安装包（产物在 src-tauri/target/release/bundle/nsis/）
npm run tauri:build
```

**技术栈**

| 技术 | 用途 |
|---|---|
| Tauri 2 | 桌面框架（Rust + 系统 WebView2） |
| Rust | 剪贴板监听、窗口材质、存储、托盘 |
| 原生 HTML / CSS / JS | 前端，无框架 |
| Vite | 前端开发服务器与打包 |

**目录结构**

```
self_clipboard/
├─ src/                     前端（原生 HTML/CSS/JS）
│   ├─ main.js              主界面逻辑：渲染、搜索、右键菜单、设置
│   ├─ styles.css           主界面样式（材质变量、深色模式）
│   ├─ tray.js / tray.css   托盘右键菜单页
│   └─ assets/logo.png
├─ index.html               主窗口入口
├─ tray.html                托盘菜单窗口入口
├─ src-tauri/
│   ├─ src/
│   │   ├─ lib.rs           应用装配与 IPC 命令注册
│   │   ├─ clipboard.rs     剪贴板监听、多格式解析、存储与清理
│   │   ├─ effects.rs       Windows 材质分档、圆角、自定义拖动/缩放
│   │   ├─ settings.rs      config.json 持久化
│   │   ├─ tray.rs          托盘图标与自绘菜单窗口
│   │   └─ autostart.rs     开机自启（注册表）
│   └─ tauri.conf.json      窗口、打包、WebView2 参数
├─ assets/readme/           README 用的图
├─ DESIGN.md                设计文档（架构、材质取舍、决策记录）
└─ CODE_REVIEW.md           v1（Electron）的代码审查结论
```

## 数据存在哪

默认目录是 `%APPDATA%\com.kunge.selfclipboard\clipboard\`，可在设置里改成任意路径：

```
clipboard/
├─ index.json        索引：id / 类型 / 预览 / 时间 / 置顶与收藏 / 大小
├─ data/<id>.txt     文本正文
├─ images/<id>.bmp   图片
└─ files/<id>.txt    文件清单（每行一个完整路径）
```

内存里只保留索引元数据；正文、图片、文件清单都按需从磁盘读取，列表滚出视野后即释放。删除条目、清理缓存、退出清理都会同步清掉对应的落盘文件。

## 路线图

v2.0 已经可用，后续仍在推进：

- [x] 文本 / 图片 / 文件采集，分类浏览与搜索
- [x] 收藏、按分类独立的置顶、保留天数
- [x] 托盘常驻与自绘菜单、开机自启
- [x] Win10/11 分档材质、材质浓度、圆角模式
- [x] 内存优化（渐进渲染、图片降采样、视野外释放位图）
- [ ] 全局快捷键唤出
- [ ] 快捷面板形态（光标附近弹出、Enter 直接粘贴回原窗口）
- [ ] 富文本（CF_HTML / RTF）与链接识别
- [ ] 隐私排除（按来源进程跳过密码管理器等）
- [ ] 列表虚拟滚动（应对十万条量级）

## 常见问题

**托盘图标在哪？**
程序一运行就在，始终常驻。右键出菜单（显示轻贴 / 退出），双击唤回窗口。

**为什么内存不是几十 MB？**
窗口由系统 WebView2 渲染，多进程模型本身就有基础开销；在保证观感（真毛玻璃、图片缩略图）的前提下已尽量压到底。

**点了文件条目却提示找不到文件？**
文件条目存的是路径引用，不是副本。原文件被移动、改名或删除后就失效了——界面会把它标成「已失效」，点击时也会告诉你跳过了哪几个。

**能在 Win7 上用吗？**
不能。最低 Windows 10 1809。

## 贡献

欢迎提 Issue 与 PR。仓库使用分支模型：`main` 只接受来自 `develop` 的合并，功能/修复请开 `feature/*`、`fix/*` 分支，提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org/)。

## 开源协议

本项目目前**尚未添加开源协议文件**。在协议确定前，代码默认保留所有权利；如果你希望复用其中的部分实现，请先开 Issue 说明用途。

## 致谢

- [Tauri](https://tauri.app/) —— 让「小安装包 + 原生能力」成为可能
- 窗口材质的实现参考了大量社区实证（`SetWindowRgn` 对 acrylic 无效、Win10 accent API 只有单色 tint 等），相关结论记录在 [DESIGN.md](./DESIGN.md) 的决策记录中
