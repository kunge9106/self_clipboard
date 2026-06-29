# Project Context — self_clipboard

> 最后更新时间：2026-06-29

## 项目基本面

- **项目名称**：self_clipboard（中文名：轻贴）
- **类型**：Windows 桌面剪贴板管理工具
- **定位**：轻量、常驻、VisionOS 风格毛玻璃剪贴板历史管理器
- **当前状态**：开发中 v1.3.9
- **目标用户**：Windows 桌面用户，需要便捷管理剪贴板历史的人
- **GitHub**：https://github.com/kunge9106/self_clipboard

## 技术栈

| 技术 | 版本 | 用途 |
|------|------|------|
| Node.js | v24.x | 运行环境 |
| Electron | ^42.3.2 | 桌面窗口框架 |
| HTML/CSS/JS | — | 渲染进程 UI |

## 核心功能

1. **剪贴板自动监听**：主进程每 1000ms 轮询系统剪贴板，检测文本和图片变化
2. **历史记录管理**：不限制条数，自动去重，新内容插入列表顶部；自动清理超过 3 天的条目
3. **历史持久化**：历史记录保存到 JSON 文件，重启应用后自动加载；图片保存为文件，不存 base64
4. **搜索过滤**：顶部搜索框，根据关键词实时过滤历史记录（不区分大小写）
5. **点击复制**：点击任意条目将内容写回系统剪贴板，附带 ✓ 动画反馈
6. **右键删除**：右键点击条目弹出上下文菜单，可选"删除"移除单条记录
7. **时间格式化**：秒/分钟/小时前相对时间，过当日 0 点后显示 HH:MM:SS
8. **清空全部**：一键清空所有历史记录
9. **设置面板**：左上角齿轮按钮，包含：开机自启开关（通过 Windows 注册表真实实现）、关闭时后台运行、缓存目录设置、关闭时清理缓存
10. **窗口控制**：关闭按钮（弹出选择：后台运行/退出/取消）+ 最小化按钮（macOS 风格圆点，hover 变红）
11. **窗口拖拽**：全窗口 JS 拖拽（通过 IPC 发送 setBounds），交互元素排除
12. **托盘**：关闭时可选隐藏到系统托盘，托盘右键有"打开界面"和"退出"
13. **搜索框失焦**：点击搜索框外任意位置输入框自动 blur
14. **空状态**：居中 SVG 图标 + "暂无相关记录" + "您暂时没有任何剪贴板历史记录"
15. **图片缓存**：图片检测后自动保存文件到缓存目录（优先 D/E/F 盘），设置可改路径
16. **深色模式**：自动跟随 Windows 主题切换，毛玻璃适配深色/浅色

## UI 主题

- **风格**：VisionOS / Apple Vision Pro Glassmorphism，浅色磨砂毛玻璃
- **配色**：浅色（白色半透明），冰蓝色 accent
- **窗口玻璃**：Windows Acrylic（setBackgroundMaterial("acrylic")）+ CSS backdrop-filter:blur(40px) saturate(1.3)
- **弹窗玻璃**：设置面板和右键菜单使用 rgba 半透明 + 圆角 14px + 阴影，模拟 iOS 毛玻璃
- **窗口尺寸**：默认 300×485（5:6.8 比例），最小限制 300×485，resizable: true
- **窗口置顶**：alwaysOnTop: false，非置顶模式
- **字体**：apple-system, "Microsoft YaHei", "Segoe UI", "PingFang SC", sans-serif
- **动画**：条目入场 slideFadeIn（0.25s cubic-bezier），hover 上浮 2px，按钮 active scale(0.85)

## 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 无边框窗口 | frame: false | 自定义毛玻璃圆角窗口 |
| 窗口拖拽 | JS + IPC setBounds | 避免 drag 吞掉鼠标事件，保证所有交互元素正常响应 |
| 禁用 F12 | keydown 拦截 | 用户要求禁用开发者工具 |
| 浅色主题 | 白色 + 冰蓝色 | VisionOS 风格 |
| 搜索纯前端 | 渲染进程 filterItems() | 所有数据已在渲染进程，无需 IPC |
| 开机自启 | Windows 注册表 HKCU\...\Run | 用户要求真实实现，非 Electron API 模拟 |
| 弹窗毛玻璃 | 纯 CSS 模拟（白色半透明 + 圆角 + 阴影） | backdrop-filter 在子元素中不生效 |
| 上下文隔离 | contextIsolation: true | 安全性考虑 |
| 轮询监听 | 1000ms Polling | 剪贴板 Viewer API 不稳定 |
| 日志语言 | 英文 | 避免 Windows 控制台编码导致的乱码 |
| 历史持久化 | JSON 文件（userData/history.json） | 重启保留历史记录 |
| 图片存储 | 文件系统（非 base64） | 减少内存占用 |
| 3天自动清理 | 启动时过滤+删除过期图片 | 用户要求只保留最近3天 |

## 打包构建

### 构建命令
```
# 测试运行
.\node_modules\.bin\electron.cmd .

# 打包安装包（可能需要 VPN 下载 winCodeSign）
npx electron-builder --win --x64
```

### 构建配置
- **打包工具**: electron-builder ^26.8.1
- **目标格式**: NSIS 安装包（.exe）
- **压缩**: maximum（LZMA2）
- **任务栏图标**: assets/icon.ico（多尺寸 256/64/48/32/24/16px）

## 变更日志

| 序号 | 日期 | 变更内容 | 影响范围 |
|---|------|----------|----------|
| 28 | 2026-06-29 | feat: 图片缓存优化+历史持久化+3天自动清理 — 图片存储改为文件（非base64），减少内存；新增 saveHistory/loadHistory JSON 持久化；启动时自动过滤仅保留最近3天条目并删除过期图片；设置面板新增缓存目录选择和清理开关；修复双分号和缺失分号 | main.js, preload.js, renderer/app.js, renderer/index.html, renderer/style.css, package.json(1.3.9) |
| 27 | 2026-06-16 | fix: IME 候选框变灰修复+首次切换主题玻璃消失重试+窗口失焦重设 acrylic+版本号自动更新(1.3.5)+asar 压缩+图标签名恢复 | main.js, preload.js, renderer/app.js, package.json, renderer/index.html |
| 26 | 2026-06-15 | fix: 大量条目时窗口无法拖拽 — window-inner 设 drag 整体可拖拽，search-area 去掉 no-drag，按钮 z-index 升级到 110，drag-bar-top 改 pointer-events:none | renderer/style.css, renderer/index.html |
