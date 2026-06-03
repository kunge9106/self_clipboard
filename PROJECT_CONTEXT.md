# Project Context — self_clipboard

> 最后更新时间：2026-06-03

## 项目基本面

- **项目名称**：self_clipboard
- **类型**：Windows 桌面剪贴板管理工具
- **定位**：轻量、常驻、透明毛玻璃效果的剪贴板历史管理器
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
├── main.js                # 主进程：窗口创建、毛玻璃效果、剪贴板监听、IPC 通信
├── preload.js             # 预加载脚本：安全桥接主进程与渲染进程
├── package.json           # 项目配置与依赖
├── .gitignore             # Git 忽略配置
├── README.md              # 项目说明
├── renderer/
│   ├── index.html         # 主界面 HTML 结构
│   ├── style.css          # 毛玻璃主题样式
│   └── app.js             # 渲染进程前端逻辑
└── node_modules/          # 依赖包（不纳入版本控制）
```

## 核心功能

1. **剪贴板自动监听**：主进程每 500ms 轮询系统剪贴板，检测文本和图片变化
2. **历史记录管理**：最大保存 100 条，自动去重，新内容插入到列表顶部
3. **点击复制**：点击任意条目将内容写回系统剪贴板，附带 ✓ 动画反馈
4. **时间格式化**：秒/分/时前相对时间，过 0 点后显示固定时间
5. **清空全部**：一键清空所有历史记录
6. **毛玻璃效果**：Windows Acrylic/Mica 系统效果 + CSS backdrop-filter 双保险

## 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 无边框窗口 | `frame: false` | 实现自定义标题栏，配合透明毛玻璃效果 |
| 上下文隔离 | `contextIsolation: true` | 安全性考虑，渲染进程不直接访问 Node.js |
| 轮询监听 | 500ms 间隔 Polling | 剪贴板 Viewer API 在某些场景不稳定，轮询更可靠 |
| 置顶显示 | `alwaysOnTop: true` | 符合剪贴板工具随开随用的定位 |
| 国产化镜像 | npmmirror.com | 国内网络环境下 Electron 二进制下载加速 |

## 已知问题

- 暂无

## 待办事项

- [ ] 打包为独立 exe（electron-builder）
- [ ] 支持快捷键唤起
- [ ] 支持固定收藏条目
- [ ] 搜索过滤功能
