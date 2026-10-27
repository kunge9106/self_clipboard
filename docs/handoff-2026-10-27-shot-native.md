# 交接：截图功能改造（2026-10-27 晚）

> 给「回家继续」用。这份文档自包含，不需要看聊天记录。

## TL;DR

截图功能今天修了一整天，**10 条分支已合并进 `develop` 并推送到 origin**。
现有实现（Tauri WebView 覆盖层）已经能用，但架构本身有根子上的问题，决定**推翻重写为
纯 Win32 原生实现**。方案已定稿、代码未落地 —— 回家从「原生重构」这一段接着做。

---

## 一、今天已经修好并合并的（`develop` 已推 origin）

按时间顺序，每条都是独立分支 `--no-ff` 合入：

| 提交 | 修了什么 |
|---|---|
| `222fe73` | 抢焦点必须在主线程（`AttachThreadInput` 挂错线程等于没挂） |
| `87b3d4b` | `focused(true)` + `SetForegroundWindow`，主线程重试 |
| `3b89786` | Alt 注入（学 tao 的 `force_window_active`）+ 鼠标钩子补偿 |
| `924cf77` | 提交幂等 + 前端防重入，修「复制失败：没有待处理的截图」 |
| `8930a18` | 覆盖层改 `WS_EX_NOACTIVATE`（不再抢焦点）+ 窗口识别/单击截整窗 |
| `c6eaa1f` | 预览改 JPEG、接住「已在按住的鼠标」、单击只选中不提交、悬停压暗 |
| `3f3f7dc` | 按钮/双击/右键改 `mousedown`（NOACTIVATE 下 `click` 不合成） |
| `6f432f4` | 编码/写剪贴板移出主线程（同步 Tauri 命令堵死事件循环） |
| `ffd77ad` | 按下/移动/抬起全部钩子兜底；修钩子开关竞态 |
| `618bb62` | 提前置 `armed`；down/up 带时间戳防迟到事件 |
| `98790b3` | 连按防抖；坐标基准改用真实屏幕尺寸 |
| `a120440` | 预览改 GDI+ 编码（300ms → 30–50ms），全尺寸不再模糊 |
| `36a51da` | 截图结束把焦点还回去（修「切窗口必须先点一下」） |

**当前体验**：按 `Alt+Q` 约 150ms 内出画面，框选/拖动/识别窗口/工具条都能用，
图能进历史。**但**偶发问题仍存在（见下）。

---

## 二、为什么还要推翻重写（重要）

所有问题的共同根源：**用 WebView 画覆盖层、让它参与输入**。

`WS_EX_NOACTIVATE`（不抢焦点）窗口里的 Chromium：
- 不合成 `click` / `dblclick` / `contextmenu` —— 按钮点了没反应
- `mousedown` / `mousemove` / `mouseup` 也不保证每次到达
- 抢焦点（`SetForegroundWindow` / `AttachThreadInput` / Alt 注入）对后台进程被系统**静默拒绝**

所以今天修一个冒一个：焦点、事件、时序、连按、焦点还回……全是同一类病。

**结论**（已定稿）：WebView 从截图链路里彻底下线，改为 **Win32 自建窗口 + 自己跑消息
循环 + GDI 绘制**。输入不再经过渲染进程，所有问题从根上消失。

### 参考实现（已调研）

- **crates.io `interactive-screenshot`**（stilltid，585 行纯 Win32）—— 就是这个做法：
  `WS_EX_NOACTIVATE` 覆盖层 + 自己 `GetMessageW` 循环 + GDI 绘制 + `EnumWindows`
  窗口识别 + cloaked 过滤。**可以读它的源码**（`cargo doc --open` 或 GitHub）。
- **winshotx**（Rust + Tauri 2，GitHub `Mun1to/winshotx`）—— 同架构，`docs/TRAMPAS.md`
  记录了 7 个 Windows/Tauri 坑（「第一次点击被系统吃掉」「focused(false) 只在首次有效」
  等），今天都撞上了。**必读**。

---

## 三、待办：原生重构（回家从这里继续）

### 要做的

新建 `src-tauri/src/shot_native.rs`（约 500–800 行），改造 `src-tauri/src/screenshot.rs` 的
`start()`。**必须保留的 UI**（用户明确要求）：

1. **工具条**：选区右下外侧（放不下翻上面），白底圆角，红叉=取消 / 绿勾=完成
2. **八向调整手柄**：四角四边共 8 个（视觉 6px，命中 12px）
3. **尺寸角标**：选区上方黑底白字「宽 × 高」（物理像素），贴顶边时翻下方
4. **悬停窗口识别**：移动时高亮光标下窗口（压暗其余 + 2px 蓝框），**单击（位移<4px）选中
   该窗口**（显示选区+工具条，不直接提交）
5. 自由框选、选区内拖动平移、右键/Esc 取消、Enter/双击完成

### 技术要点

- **独立线程**创建窗口 + 跑消息循环（Tauri 主线程是事件循环，不能自己 `GetMessage`）
- 窗口样式：`WS_POPUP` + `WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`
- GDI **双缓冲**（内存 DC + 兼容位图，`WM_PAINT` 里画完再 `BitBlt`），防闪烁
- 压暗遮罩用 `AlphaBlend`（`msimg32.lib`，`BLENDFUNCTION{AC_SRC_OVER,0,128,0}`），
  只对选区**外**的四个矩形做
- 工具条位置：右下外侧 8px；超屏底翻上面；再不行压内侧。宽 78 高 42
- 完成后调现有的 `crop_to_dib` + `write_shot_to_clipboard`（**不要重写**）
- Esc/Enter 仍靠**全局键盘钩子**转发（NOACTIVATE 窗口收不到键盘），现有 `key_proc` 改造即可

### 可直接复用的现有代码（`src-tauri/src/screenshot.rs`）

- `grab(left, top, w, h)` — GDI 抓屏，返回 BGRA
- `list_windows() -> Vec<[i32;4]>` — EnumWindows + 过滤（可见/非最小化/有标题/非
  TOOLWINDOW/非 cloaked/跳过自身/排除 Progman、WorkerW、Shell_TrayWnd）
- `crop_to_dib()` / `crate::clipboard::write_shot_to_clipboard()`
- `crate::effects::Rect`（可当 Win32 RECT）
- `crate::panel::force_foreground(hwnd)` — 只在「结束把焦点还回去」时用
- `crate::log_print!` / `log_warn!` / `log_err!`

**可以删掉**：`shot.html`/`shot.js`/`shot.css` 对应的 WebView 逻辑、`shot_image`/
`shot_show`/`shot_windows`/`shot_pointer` 命令、`gdiplus_jpeg`/`encode_preview`、
「首次按下补偿」鼠标钩子。

### 分支

新建 `feat/shot-native-core`（本地已有同名分支，指向 `36a51da`，无新内容，
可直接在其上开工）。**不直推 main**，`--no-ff` 合入 `develop`，不删任何分支。

---

## 四、环境与验证方法

```bash
# 进入仓库根目录（clone / pull 后的路径，家里和单位不一样，别写死）
cd <仓库根>
# Cargo.toml 在 src-tauri/ 下（不是仓库根）

# 测试
cd src-tauri && cargo test --lib     # 当前 67 个测试全过

# 开发自测（两个终端）
# 终端1：npm run dev          （vite，5173）
# 终端2：cargo build 然后双击 src-tauri/target/debug/self-clipboard.exe
```

**日志**（诊断一切问题先看它）：`<cache_dir>/log/Clipboard.log`。
`cache_dir` 由**当前这台机器**的配置决定，在
`%APPDATA%\com.kunge.selfclipboard\config.json` 的 `cache_dir` 字段里读
（家里和单位的缓存目录不一样，**以当前机器的配置文件为准**）。
不是 `%LOCALAPPDATA%\轻贴` 那个位置。

**判定标准**：日志里应能看到 `[shot] 会话 #N 开始` → `[shot] 已复制选区 …`；
`[fe/shot]` 是前端上报。

### 发版约定

- 合并到测试分支后**先不出安装包**，用 `npm run tauri dev` 自测
- **确认 OK 后**才 `npm run version:set X.Y.Z-beta.N` + `npm run tauri build`
- 交付物必须 `npm run tauri build`（`cargo build --release` 是 dev 版）
- 构建前关闭 `self-clipboard.exe`，否则锁产物

---

## 五、其他约定（红线，别踩）

- **只能用 bash**，禁止 PowerShell
- **每改动一分支**，`main` 只经 PR 且 `--no-ff`（禁 squash），**收到用户指令前不自行合并**
- **不删任何分支**
- Token/PAT 不写入 URL/配置/代码/提交信息
- 构建产物、临时脚本可 `rm -rf` 直删（不进回收站）；仓库已跟踪文件和个人资料除外
- Commit 正文写「为什么」（动机/根因/取舍/副作用），不写 diff 里能看出的

---

## 六、下一步建议（按优先级）

1. **原生重构**（上面第三节）—— 这是唯一能根治的路
2. 多屏 / DPI 实测（今天只在单屏 1920×1080 验过）
3. 微信/QQ 图片复制的「一变多」问题（记忆里另一条线，今天没动）
4. 发版前把 `CHANGELOG.md` 补一节，`touch src-tauri/src/about.rs` 再编译
   （CHANGELOG 是编译期内嵌的）

---

*生成于 2026-10-27 17:12，当时状态：`develop` = `286a2a6`（已推 origin），
`feat/shot-native-core` = `36a51da`（无新内容）。*
