# Project Context — self_clipboard

> 最后更新时间：2026-06-04 14:10

## 项目基本面

- **项目名称**：self_clipboard
- **类型**：Windows 桌面剪贴板管理工具
- **定位**：轻量、常驻、VisionOS 风格毛玻璃剪贴板历史管理器
- **当前状态**：开发中 v1.0.0
- **目标用户**：Windows 桌面用户，需要便捷管理剪贴板历史的人
- **GitHub**：https://github.com/kunge9106/self_clipboard
- **当前分支**：dev/visionos-ui（领先 origin 4 个 commit，未推送）

## 技术栈

| 技术 | 版本 | 用途 |
|------|------|------|
| Node.js | v24.14.0 | 运行环境 |
| Electron | ^42.3.2 | 桌面窗口框架 |
| HTML/CSS/JS | — | 渲染进程 UI |

## 项目目录结构

`
clipboard/
├── main.js                # 主进程：窗口创建、毛玻璃效果、剪贴板监听、IPC
├── preload.js             # 预加载脚本：安全桥接主进程与渲染进程
├── test/
│   └── app-test.js        # 集成测试（TDD 红绿循环，10 项）
├── package.json           # 项目配置与依赖
├── .gitignore             # Git 忽略配置
├── README.md              # 项目说明
├── PROJECT_CONTEXT.md     # 项目记忆文件（本文件）
├── renderer/
│   ├── index.html         # 主界面 HTML 结构
│   ├── style.css          # 浅色毛玻璃主题样式
│   └── app.js             # 渲染进程前端逻辑
└── node_modules/          # 依赖包（不纳入版本控制）
`

## 核心功能

1. **剪贴板自动监听**：主进程每 1000ms 轮询系统剪贴板，检测文本和图片变化
2. **历史记录管理**：不限制条数，自动去重，新内容插入列表顶部
3. **搜索过滤**：顶部搜索框，根据关键词实时过滤历史记录（不区分大小写）
4. **点击复制**：点击任意条目将内容写回系统剪贴板，附带 ✓ 动画反馈
5. **右键删除**：右键点击条目弹出上下文菜单，可选"删除"移除单条记录
6. **时间格式化**：秒/分钟/小时前相对时间，过 0 点后显示 HH:MM:SS
7. **清空全部**：一键清空所有历史记录
8. **设置面板**：左上角齿轮按钮，包含"开机自启"开关（通过 Windows 注册表真实实现）
9. **窗口控制**：关闭按钮 + 最小化按钮（macOS 风格圆点，hover 变红）
10. **窗口拖拽**：全窗 JS 拖拽（通过 IPC 发送 setBounds），仅交互元素排除
11. **搜索框失焦**：点击搜索框外任意位置输入框自动 blur
12. **空状态**：居中 SVG 图标 + "暂无相关记录" + "您暂时没有任何剪贴板历史记录"

## UI 主题

- **风格**：VisionOS / Apple Vision Pro Glassmorphism，浅色磨砂毛玻璃
- **配色**：浅色（白色半透明），冰蓝色 accent
- **窗口玻璃**：Windows Acrylic（setBackgroundMaterial("acrylic")）+ CSS backdrop-filter:blur(40px) saturate(1.3)
- **内层布局**：.window（外层容器）→ .window-inner（玻璃层 + 裁剪），分离阴影与玻璃
- **弹窗玻璃**：设置面板和右键菜单使用 rgba(255,255,255,0.72) + 圆角 14px + 阴影，模拟 iOS 毛玻璃
- **窗口尺寸**：默认 300×485（5:6.8 比例），最小限制 300×485，resizable: true
- **窗口置顶**：alwaysOnTop: false，非置顶模式
- **圆角体系**：
  - 窗口外层直角（DWM acrylic 限制），内层 .window-inner 圆角 18px
  - 卡片圆角 12px，搜索框圆角 12px
  - 按钮 50%（圆形）
- **字体**：-apple-system, "Microsoft YaHei", "Segoe UI", "PingFang SC", sans-serif
- **动画**：条目入场 slideFadeIn（0.25s cubic-bezier），hover 上浮 2px，按钮 active scale(0.85)

## 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 无边框窗口 | frame: false | 自定义毛玻璃圆角窗口 |
| 窗口拖拽 | JS + IPC setBounds（非 CSS -webkit-app-region:drag） | 避免 drag 吞掉鼠标事件，保证所有交互元素正常响应 |
| 禁止 F12 | keydown 拦截 | 用户要求禁用开发者工具 |
| 浅色主题 | 白色 + 冰蓝色 | VisionOS 风格 |
| 搜索纯前端 | 渲染进程 filterItems() | 所有数据已在渲染进程，无需 IPC |
| 开机自启 | Windows 注册表 HKCU\...\Run | 用户要求真实实现，非 Electron API 模拟 |
| 弹窗毛玻璃 | 纯 CSS 模拟（白色半透明 + 圆角 + 阴影） | backdrop-filter 在子元素中不生效 |
| 上下文隔离 | contextIsolation: true | 安全性考虑 |
| 轮询监听 | 1000ms Polling | 剪贴板 Viewer API 不稳定 |
| 日志语言 | 英文 | 避免 Windows 控制台编码导致的乱码 |

## 窗口控制布局

- **设置按钮**：左上角 top:4px; left:12px，16×16 圆形，风格与 win-btn 一致
- **最小化/关闭按钮**：右上角 top:4px; right:16px，gap:8px，16×16 圆形
- **hover 颜色**：均统一为红色系 #E04040
- **搜索框**：padding:28px 16px 4px，高度 30px
- **工具栏**：底部 padding:1px 16px 6px，显示"共 X 条"和清空按钮

## 弹窗行为

- **设置面板**：点击齿轮切换显示，点击外部区域关闭
- **上下文菜单**：右键条目显示，点击外部或点击选项关闭
- **搜索框失焦**：点击 .window 区域中非 .search-box 的部分即 blur

## 测试

- **文件**：test/app-test.js
- **用例数**：10 项
- **覆盖**：窗口结构、圆角、按钮尺寸、最小尺寸、文本截断、无图标、F12 禁用、搜索框、IPC 桥接、无历史限制
- **运行**：node test/app-test.js

## 已知问题

- 搜索框点击 padding 区域不会 blur（因为拖拽排除逻辑未覆盖 .search-area）
- 本地位于 dev/visionos-ui 分支，领先远程 4 个 commit，尚未推送

## 打包构建

### 构建命令

```powershell
# 完整打包（NSIS 安装包）
npx electron-builder --win --x64

# 仅解包目录（无安装包，不需要 winCodeSign）
npx electron-builder --dir
```

### 构建配置

- **打包工具**: electron-builder ^26.8.1
- **目标格式**: NSIS 安装包（.exe）
- **任务栏图标**: assets/icon.ico（多尺寸 256/64/48/32/24/16px）
- **压缩**: maximum（LZMA2）
- **后处理钩子**: scripts/afterPack.js — 构建完成后自动删除无用文件

### 体积优化

- **无用语言包**: 删除 53 个非必需语言包（仅保留 zh-CN + en-US）→ 节省 ~47MB
- **无用 DLL**: 删除 dxcompiler(24.5MB)、vk_swiftshader(5.3MB)、libGLESv2(7.6MB) 等 → 节省 ~46MB
- **总计节省**: ~93MB
- **预期安装包**: ~55-60MB（原 97MB）
- **解压后目录**: ~253MB（原 ~346MB）

> ⚠ 安装包制作需要下载 winCodeSign（GitHub 资源），在中国大陆可能需要 VPN。

## 待办事项

| 序号 | 日期 | 变更内容 | 影响范围 |
|---|------|----------|----------|
| 23 | 2026-06-04 14:10 | 重写 PROJECT_CONTEXT.md 补全至最新 commit 的所有功能记录 | PROJECT_CONTEXT.md |
| 22 | 2026-06-04 13:15 | feat: 设置+开机自启(注册表)+右键删除+全窗JS拖拽+搜索blur+空状态icon+弹窗玻璃+alwaysOnTop:false+默认300×485 | main.js, preload.js, renderer/*, test/app-test.js |
| 21 | 2026-06-04 12:45 | 全面重构玻璃效果：去除全窗蒙白/玻璃仅限卡片/优化blur+saturate+contrast算法/卡片高光/alwaysOnTop:false+失焦隐藏+全局快捷键 | main.js, renderer/style.css, test/app-test.js |
| 20 | 2026-06-04 11:17 | 移除CSS backdrop-filter消除双层毛玻璃 + 窗口失焦消失 + 圆角恢复18px | renderer/style.css |
| 19 | 2026-06-04 11:09 | 恢复acrylic真玻璃效果 + 圆角14px + 5:6.8尺寸(340×462, min 300×408) | main.js, renderer/style.css, test/app-test.js |
| 18 | 2026-06-04 10:50 | 移除setBackgroundMaterial消除直角阴影 + transparent:true + minHeight:350 | main.js, test/app-test.js |
| 17 | 2026-06-04 10:20 | 内外层结构分离修复直角阴影 + 默认最小窗口 + 按钮hover统一红色 + 搜索框缩小30px | renderer/*, main.js |
| 16 | 2026-06-04 09:56 | macOS风格按钮 + drop-shadow修复直角阴影 + 无限制历史 + 文本截断 | renderer/*, main.js, test/app-test.js |
| 15 | 2026-06-03 | 圆角统一24px/16px + 搜索框外可拖拽 + 右上角关闭按钮 | renderer/*, preload.js, main.js |
| 14 | 2026-06-03 | 拖拽修复 + 去除条目图标 + 禁用F12 + 新建dev/visionos-ui分支 | renderer/*, test/app-test.js |
| 13 | 2026-06-03 | VisionOS 风格 UI 重设计 + 搜索功能 + 窗口拖拽修复 | renderer/*, test/app-test.js |
| 12 | 2026-06-03 | 日志时间改为单列格式 yyyy-mm-dd HH:MM（北京时间） | PROJECT_CONTEXT.md |
| 11 | 2026-06-03 | 删除标题栏/icon + 控制台日志改英文防乱码 | UI、调试、文档 |
| 10 | 2026-06-03 | 修复剪贴板监听无响应 + 增加调试日志 | main.js, app.js |
| 9 | 2026-06-03 | 初始版本：Electron 剪贴板工具全部功能 | 项目全局 |



