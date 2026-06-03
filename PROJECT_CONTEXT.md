# Project Context — self_clipboard

> 最后更新时间：2026-06-03 17:50

## 项目基本面

- **项目名称**：self_clipboard
- **类型**：Windows 桌面剪贴板管理工具
- **定位**：轻量、常驻、VisionOS 风格毛玻璃剪贴板历史管理器
- **当前状态**：开发中 v1.0.0
- **目标用户**：Windows 桌面用户，需要便捷管理剪贴板历史的人
- **GitHub**：https://github.com/kunge9106/self_clipboard

## 技术栈

| 技术 | 版本 | 用途 |
|------|------|------|
| Node.js | v24.14.0 | 运行环境 |
| Electron | ^42.3.2 | 桌面窗口框架 |
| HTML/CSS/JS | — | 渲染进程 UI |

## 项目目录结构

```
clipboard/
├── main.js                # 主进程：窗口创建、毛玻璃效果、剪贴板监听、IPC
├── preload.js             # 预加载脚本：安全桥接主进程与渲染进程
├── test/
│   └── app-test.js        # 集成测试（TDD 红绿循环）
├── package.json           # 项目配置与依赖
├── .gitignore             # Git 忽略配置
├── README.md              # 项目说明
├── PROJECT_CONTEXT.md     # 项目记忆文件（本文件）
├── renderer/
│   ├── index.html         # 主界面 HTML 结构（VisionOS 风格）
│   ├── style.css          # 浅色毛玻璃主题样式
│   └── app.js             # 渲染进程前端逻辑（含搜索）
└── node_modules/          # 依赖包（不纳入版本控制）
```

## 核心功能

1. **剪贴板自动监听**：主进程每 1000ms 轮询系统剪贴板，检测文本和图片变化
2. **历史记录管理**：最大保存 100 条，自动去重，新内容插入到列表顶部
3. **搜索过滤**：顶部搜索框，根据关键词实时过滤历史记录（不区分大小写）
4. **点击复制**：点击任意条目将内容写回系统剪贴板，附带 ✓ 动画反馈
5. **时间格式化**：秒/分钟/小时前相对时间，过 0 点后显示固定时间
6. **清空全部**：一键清空所有历史记录
7. **毛玻璃效果**：Windows Acrylic/Mica + CSS backdrop-filter: blur(40px)

## UI 主题

- **风格**：VisionOS / Apple Vision Pro Glassmorphism
- **配色**：浅色（白色 + 冰蓝色），半透明磨砂
- **容器**：圆角 20px，背景 rgba(255,255,255,0.35)，阴影 0 10px 40px
- **卡片**：条目为独立悬浮卡片，圆角 14px，hover 上浮 2px
- **搜索框**：44px 高，圆角 14px，带放大镜图标
- **字体**：标题 16px Semibold #222，时间 12px #999
- **动画**：条目入场 slideFadeIn，hover 提升，focus 光晕

## 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 无边框窗口 | `frame: false` | 自定义毛玻璃圆角窗口 |
| 窗口拖拽 | 整窗可拖拽，仅交互元素 no-drag | 用户要求点击空白区域即可拖拽 |
| 浅色主题 | 白色 + 冰蓝色 | 根据 UI 设计文件改为 VisionOS 风格 |
| 搜索纯前端 | 渲染进程 filterItems() | 所有数据已在渲染进程，无需 IPC |
| 上下文隔离 | `contextIsolation: true` | 安全性考虑 |
| 轮询监听 | 1000ms Polling | 剪贴板 Viewer API 不稳定 |

## 待办事项

- [ ] 打包为独立 exe（electron-builder）
- [ ] 支持快捷键唤起
- [ ] 支持固定收藏条目

## 变更日志

| 日期与时间 | 变更内容 | 影响范围 |
|------------|----------|----------|
| 2026:06:03 18:10 | 拖拽修复 + 去除条目图标 + 禁用F12，新建分支 dev/visionos-ui | renderer/*, test/app-test.js, git |
| 2026:06:03 17:50 | VisionOS 风格 UI 重设计：浅色主题、搜索框、卡片式条目、动画、拖拽修复 | renderer/*, test/app-test.js |
| 2026:06:03 17:36 | 合并日志时间为单列格式 yyyy:mm:dd xx:xx | PROJECT_CONTEXT.md |
| 2026:06:03 17:10 | 删除标题栏和图标、控制台日志改用英文防乱码、增加变更日志章节 | UI、调试、文档 |
| 2026:06:03 16:50 | 修复剪贴板监听无响应：增加调试日志、优化轮询逻辑 | main.js, app.js |
| 2026:06:03 16:34 | 初始版本：实现 Electron 剪贴板工具全部功能 | 项目全局 |

