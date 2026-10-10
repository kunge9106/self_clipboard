# 轻贴 · 设计文档

> **技术栈**：Tauri 2（Rust + 系统 WebView2）
> **窗口形态**：单窗口管理界面（无边框自绘）
> **系统支持**：Windows 10 1809 及以上
> **仓库**：https://github.com/kunge9106/self_clipboard
> **当前版本**：v2.0.0

## 怎么读这份文档

本文分两部分：

- **第 1 – 11 章：已实现的实现说明**。全部以 `v2.0.0` 的**实际代码**为准，写的是「现在是怎样」，不是「打算怎样」。
- **第 12 章：路线图**。集中存放开发前规划、但当前**尚未实现**的部分（全局热键、快捷面板、虚拟滚动等）。这些规划继续保留，后续按序推进。

每章尽量给出对应源码文件，便于对照。

---

## 一、项目定位

**一句话：把复制过的东西本地存下来，需要时搜得到、点一下就能拿回来。**

轻贴是一个**单机剪贴板历史管理器**：

- 常驻托盘，事件驱动监听剪贴板，空闲时几乎不占 CPU；
- 内容全部落盘到本地目录，无账号、无云端、无网络请求；
- 只在需要时打开窗口查找与取用。

核心指标：**安装包 < 2 MB、常驻内存尽量低、窗口观感与系统原生质感一致**。

它不是「输入加速器」（那需要全局热键 + 快捷面板 + 直接粘贴回原窗口，见第 12 章）。当前形态是**管理后台**：打开窗口 → 搜索/浏览 → 右键复制 → 回到目标程序粘贴。

---

## 二、技术栈与总体架构

### 2.1 技术选型

| 层 | 技术 | 说明 |
|---|---|---|
| 桌面框架 | Tauri 2 | 复用系统 WebView2，安装包小、无 Node 运行时 |
| 后端 | Rust | 剪贴板监听、窗口材质、存储、托盘、注册表 |
| 前端 | 原生 HTML / CSS / JS | **无框架**，无构建期依赖之外的开销 |
| 构建 | Vite | 多入口打包（`index.html` + `tray.html`） |
| 打包 | NSIS（Tauri bundler） | 简体中文安装器 |

### 2.2 为什么是 Tauri 而不是 Electron

| 维度 | Electron（v1） | Tauri 2（v2） |
|---|---|---|
| 安装包 | 89 MB | **1.3 MB** |
| 调 Win32 | 需 N-API addon，构建链路复杂 | Rust 直接 `extern "system"` 声明，零额外依赖 |
| 版本探测 / 材质 | 受框架封装限制 | 直接 `RtlGetVersion` + DWM 调用 |
| 进程模型 | 自带完整 Chromium + Node | 复用系统 WebView2 |

v1 的教训（`setBackgroundMaterial` 在 Win10 静默失效）正是「被框架封装挡住底层」的典型，v2 直接用原生 API。

### 2.3 模块划分

```
src-tauri/src/
├─ main.rs         入口（仅调用 lib::run）
├─ lib.rs          应用装配：初始化顺序、IPC 命令注册、窗口事件
├─ clipboard.rs    剪贴板监听、多格式解析、存储、清理、失效检测
├─ effects.rs      Windows 材质分档、圆角、去边框、自定义拖动与缩放
├─ settings.rs     config.json 持久化
├─ tray.rs         托盘图标与自绘菜单窗口
└─ autostart.rs    开机自启（手写注册表 FFI）
```

前端：

```
src/
├─ main.js         主界面：渲染、搜索、右键菜单、设置页
├─ styles.css      主界面样式（材质变量 / 深色模式 / 圆角开关）
├─ tray.js         托盘菜单页交互
├─ tray.css        菜单页样式（@import styles.css 复用变量）
└─ assets/logo.png
index.html         主窗口入口
tray.html          托盘菜单窗口入口
```

### 2.4 启动流程（`lib.rs`）

```
Builder::default()
  .setup(|app| {
      settings::init(app)      // 读 config.json → 全局 CONFIG
      clipboard::init(app)     // 建目录、载索引、补写文件清单、按天数清理、订阅监听
      setup_effects(app)       // 施加材质、去边框、装焦点钩子、注册窗口事件
      tray::create_tray(app)   // 托盘图标常驻
      main_window.show()       // 窗口创建时 visible=false，材质铺好后再显示
  })
  .build() → app.run(|_, event| {
      if RunEvent::Exit { clipboard::on_app_exit() }   // 退出统一清理
  })
```

**顺序有讲究**：材质必须在窗口 `show` 之前/同时就位，否则会看到「先白后糊」的闪。因此配置文件里把主窗口设为 `visible: false`，由 Rust 在 setup 末尾显式 `show()`。

---

## 三、剪贴板采集（`clipboard.rs`）

### 3.1 事件驱动，不轮询

v1 每秒 `readText()` + `readImage()`，即便做了指纹判重，`readImage()` 本身仍要每秒解码一次位图，CPU 空转。

v2 的做法：

```
AddClipboardFormatListener(main_hwnd)      // Vista 起全平台支持
   ↓ 系统剪贴板变化
WM_CLIPBOARDUPDATE                          // 投递到主窗口（WndProc 子类拦截）
   ↓
on_clipboard_update()
```

实现细节：对主窗口 HWND 做 **WndProc 子类化**，在消息循环里截获 `WM_CLIPBOARDUPDATE`，不需要单独的监听线程。

### 3.2 去重与「自己写回」的识别

```
GetClipboardSequenceNumber()
   ├─ 序列号未变        → 直接返回
   ├─ 剪贴板 owner 是本窗口 → 是我们自己 copy_back 写回去的，不记录（见 3.3）
   ├─ 序列号 == SKIP_SEQ → 跳过（记录后清空标记）
   └─ 否则 → 读取并入库
```

用序列号而不是内容哈希：**零成本且更可靠**（内容哈希要先把内容读出来，等于该省的不省）。

### 3.3 copy_back 的竞态与 owner 判定

点击条目会把内容写回剪贴板，系统随即投递 `WM_CLIPBOARDUPDATE`。这段有个**时序陷阱**：

- `copy_back` 跑在命令线程；
- `WM_CLIPBOARDUPDATE` 投递到主线程；
- 主线程**可能**在命令线程执行「标记跳过序列号」之前就处理了这条消息 —— 于是把自己写回的内容当成一次新复制，产生重复条目。

**解法**：以 `GetClipboardOwner() == 本窗口` 作为判定。写回后剪贴板 owner 一定是我们，主线程无论何时处理都能识别，**没有窗口期**。

### 3.4 格式支持与优先级

```
优先级：文本 > 文件 > 图片

CF_UNICODETEXT  → RawClip::Text
CF_HDROP        → RawClip::Files   （受「文件复制」开关控制，默认关闭）
CF_BITMAP / DIB → RawClip::Image
```

- **文本**：`GetClipboardData(CF_UNICODETEXT)`，取宽字符串。
- **文件**：`DragQueryFileW` 枚举完整路径。**注意**：剪贴板 HDROP 句柄归系统所有，**绝不能调 `DragFinish`**（详见 9.3）。
- **图片**：DIB → BMP 落盘。转换时要补 40 字节 `BITMAPINFOHEADER` 的前提处理与 14 字节文件头，并把 32bpp 的 alpha 强制置 255 —— 否则前端 canvas 会出现透明/黑块。

### 3.5 开关：文件复制默认关闭

`CF_HDROP` 的采集由 `settings.file_copy` 控制，**默认关闭**：

- 关闭时 `read_clipboard` 完全不看 `CF_HDROP`；
- 前端「文件」入口保留，点进去显示「文件复制功能已关闭」；
- 已存在的旧文件条目只是不展示，数据仍在磁盘上，打开开关即恢复。

---

## 四、存储设计（`clipboard.rs`）

### 4.1 目录布局

默认 `%APPDATA%\com.kunge.selfclipboard\clipboard\`，可在设置中改为任意路径：

```
clipboard/
├─ index.json          索引（元数据）
├─ data/<id>.txt       文本正文
├─ images/<id>.bmp     图片
└─ files/<id>.txt      文件清单（每行一个完整路径）
```

**内容一律落盘**，任何一条记录在缓存目录里都能找到对应的实体文件 —— 这是从 v1 继承并强化的硬约束。

### 4.2 索引结构

`index.json` 是一个 `HistoryItem` 数组：

| 字段 | 说明 |
|---|---|
| `id` | 唯一标识 |
| `kind` | `text` / `image` / `files` |
| `preview` | 预览文本（文本截断、文件为「名字 等 N 个文件」） |
| `size` | 字节数（估算） |
| `ts` | 时间戳（毫秒） |
| `pinned_views` | 在哪些分类里被置顶（见 4.4） |
| `favorite` | 是否收藏 |
| `text_path` / `img_path` / `files_path` | 对应实体文件的相对路径 |

内存中常驻的只有这份**元数据**；正文、图片、文件清单都按需从磁盘读取。

> **serde 注意**：`HistoryItem` 同时用于 IPC 与 `index.json` 持久化。**不能**整体加 `rename_all = "camelCase"` —— 旧 `index.json` 里的 `text_path` / `img_path` 会解析失败，文本与图片会集体丢路径。字段名需要驼峰时**单独**用 `#[serde(rename = "...")]`。

### 4.3 读写与清理

| 操作 | 行为 |
|---|---|
| 新增 | 写实体文件 → 入内存索引 → 存 `index.json` |
| 删除条目 | 删索引项 + 删对应实体文件（`data/` / `images/` / `files/`） |
| 保留天数 | `prune_retention(days)`，`0` = 永久保留；**置顶/收藏一律豁免** |
| 全清 | 清空三个内容目录 + 历史；可选是否连置顶/收藏一起清 |
| 占用统计 | `cache_info()` 返回目录、文件数、总大小 |
| 退出清理 | `on_app_exit()`，由 `RunEvent::Exit` 统一触发 |

置顶豁免的判定口径是「**在任何分类里置顶过**」（`pinned_views` 非空），避免在「历史」里把你在「文本」里置顶的内容误删。

### 4.4 置顶按分类独立

`pinned` 从布尔值演化为 `pinned_views: Vec<String>`，取值与前端视图名一致：`history` / `text` / `images` / `files` / `favorites`。

- 在「文本」里置顶 → 只在「文本」浮到顶部；
- 在「历史」里它仍在原位；

旧 `index.json` 的全局 `pinned: true` 在载入时迁移为 `pinned_views = ["history"]`，行为不变。该旧字段只读不写（`skip_serializing`）。

### 4.5 文件失效检测

文件条目存的是**路径引用**而非副本，原文件被移动 / 改名 / 删除后就粘不出来。整体策略：

- **渲染后**：前端异步调 `check_files_alive(ids)`（只做 `Path::exists()`，纯元数据查询），失效卡片压暗并标「已失效」；
- **点击时**：`copy_back` 逐个校验 —— 全部失效直接报错说明；**部分失效**只写回还在的那几个，并返回提示信息（如「已跳过 1 个」），由前端 toast。
- 因此 `copy_back` 的签名是 `Result<Option<String>, String>`，`Some(msg)` 表示「成功了但有情况要告诉你」。

`files_path` 缺失的老数据在启动时由 `backfill_files()` 补写清单并回存索引。

---

## 五、窗口材质与圆角（`effects.rs`）

**这是本项目技术含量最高、也是踩坑最多的部分。** 结论均来自实测与社区实证，不是推测。

### 5.1 分档策略

| 系统 | build | 手段 | 圆角上限 |
|---|---|---|---|
| Win11 22H2+ | ≥ 22621 | `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE)` | 12px（系统圆角） |
| Win11 21H2 | 22000 | `DwmSetWindowAttribute(DWMWA_MICA_EFFECT)` | 12px（系统圆角） |
| Win10 1809–22H2 | 17763 – 19045 | `SetWindowCompositionAttribute + ACCENT_ENABLE_ACRYLICBLURBEHIND` 真·acrylic | 20px |
| 更低版本 / 调用失败 | — | 不透明兜底（`solid`） | 12px |

**版本探测用 `RtlGetVersion`（ntdll），不用 `GetVersionEx`**：后者在没有 manifest 声明的进程里，Win10 上会返回 6.2（Win8）—— 这是「静默失败」的经典来源，必须避开。

### 5.2 全部手写 Win32 FFI，不引入 `windows` crate

只需要寥寥几个函数（`SetWindowCompositionAttribute`、`DwmSetWindowAttribute`、`RtlGetVersion`、拖拽与窗口相关调用），为它们拖入整个 bindings crate 不划算，且能避免与 tauri 内部依赖的 `windows` 版本漂移冲突。全部通过 `extern "system"` 声明。

### 5.3 Win10 的四难困境与最终取舍

社区实证（SO 63324480、vscode-vibrancy-continued、AutoIt 讨论等）与实测共同确认：

1. **`SetWindowRgn` 对 acrylic 完全无效** —— acrylic 恒铺满窗口矩形，区域裁剪切不掉它；
2. 区域裁剪是 **1-bit 的，没有抗锯齿** —— 圆角边缘会变成一排像素台阶（用户反馈的「圆角锯齿很重」正是它）；
3. **Win10 的 accent API 只有单色 tint，没有 luminosity 自适应层** —— 纯色背景上必然发平；
4. 只有 Win11 有 `DWMWA_BORDER_COLOR = NONE` 可以关掉 DWM 沿窗口矩形画的 1px 边框。

因此 **「Win10 + 系统模糊 + 圆角 + 四角无残留」四者不可兼得**。轻贴的解法是把取舍交给用户（设置项「圆角模式」，默认关）：

| 模式 | 做法 | 结果 |
|---|---|---|
| **直角（默认）** | 保留系统 acrylic；不做任何区域裁剪 | 窗口矩形与 acrylic **完全吻合 → 四角零残留**，且保留真模糊 |
| **圆角** | 关掉系统 blur（`clear_accent`），窗口为纯透明 layered | 四角**真透明、100% 干净**，圆角交给 CSS `border-radius`（自带抗锯齿）；代价是背后壁纸不再被模糊 |

**共同点：都不做 `SetWindowRgn` 裁剪**（`region_radius()` 恒返回 0）—— 它既挡不住 acrylic，又带来锯齿，干脆不用。

### 5.4 tint 压到 8%，浓度交给 CSS

`TINT_ALPHA = 0x14`（约 8%）。压得这么低的理由：acrylic 的 tint 是单色的、压不低就会在纯色背景上「发平发廉价」；把 tint 降到只剩模糊，**窗口的明暗改由 CSS 这一层负责**。

所以「材质浓度」滑块控制的是 CSS 变量 `--glass-alpha`（窗口主体的不透明度），可以在前端实时调整、无需重建窗口：

```
设置 → 材质浓度 [25 … 90]（默认 62）
        ↓ invoke set_material_alpha
Rust: settings.material_alpha = clamp(40,90) + reapply_material()（重刷 acrylic）
前端: --glass-alpha = value/100
```

### 5.5 去掉 DWM 画的 1px 边框

tao（Tauri 的窗口层）为了拿到 DWM 阴影会保留 `WS_CAPTION | WS_THICKFRAME`，Windows 于是沿**窗口矩形**画一圈 1px 边框 —— 在圆角下就表现为「四角的直角阴影」。解法是 `strip_window_frame()`：清除这些样式位并 `SetWindowPos(..., SWP_FRAMECHANGED)` 强制重算。

### 5.6 焦点钩子

失焦 / 聚焦时 Windows 会发 `WM_NCACTIVATE` 重绘原生标题栏，把材质与圆角冲掉。`install_focus_hook()` 拦截该消息，避免「聚焦后圆角消失」。

### 5.7 补涂与 nudge

Win10 的 acrylic 在窗口刚创建时不一定立刻铺开。显示后按 200 / 600 / 1200 ms 各补涂一次，并做一次 `SetWindowPos` 尺寸微调（nudge）触发 DWM 重算。启动日志会打印**实际生效档位**：

```
[effects] 补涂流程结束，最终档位：acrylic
```

---

## 六、窗口与自绘交互（`effects.rs` + 前端）

### 6.1 无边框窗口

配置：`decorations: false`、`transparent: true`、`shadow: false`、`visible: false`、`center: true`、`dragDropEnabled: false`。窗口尺寸 1060×690（最小 880×580），标题栏、按钮全部自绘。

### 6.2 自定义拖动

系统拖动在 Win10 acrylic 下会滞后。改为自定义：

```
按下标题栏 → drag_start 命令
   → SetCapture + WM_MOUSEMOVE
   → 16ms 节流（MOVE_THROTTLE_MS，≈60Hz，匹配 DWM 合成节奏）
   → SetWindowPos 移动
```

**节流是必需的**：高轮询率鼠标会让 DWM 事件积压，出现「幽灵窗口 / 光标漂移」（DevToys #1258 同源问题）。

### 6.3 自定义缩放

系统缩放循环（`WM_NCHITTEST` 返回缩放边缘）在 acrylic 下无法节流，缩放严重滞后。因此把边界判定收回：前端在窗口边缘放一圈热区（`.rz-layer` 的 8 个 `.rz`），拖动热区触发 `resize_start(edge)`，由 Rust 侧同样以 16ms 节流执行 `SetWindowPos`。

缩放**不改变布局**（前端纯 CSS 自适应，无按像素重排的逻辑）。

---

## 七、托盘（`tray.rs`）

### 7.1 常驻

程序一启动就创建托盘图标，**常驻**。旧实现是「勾选关闭时后台运行 + 点关闭窗口」才创建、取消勾选就移除 —— 用户看到的托盘时有时无。

「关闭时后台运行」开关保留原语义，只决定**点关闭按钮**的行为：开 → 隐藏到托盘；关 → 退出。

交互：右键出菜单；左键双击唤回主窗口。

### 7.2 自绘菜单

系统原生托盘菜单（`TrackPopupMenu`）的字体、配色、圆角都不可控，与本体割裂。改为弹一个无边框透明小窗口 `tray-menu`：

- 页面 `tray.html` + `src/tray.css`，后者 `@import "./styles.css"`，复用**同一套 CSS 变量**（材质色 / 描边 / 悬浮色 / 深色模式）；
- Rust 侧对菜单窗口施加与主窗口**完全相同**的 acrylic，每次弹出重刷一次；
- **圆角跟随「圆角模式」**：直角模式（默认）菜单也是直角（窗口矩形与 acrylic 吻合、四角零残留），圆角模式才用 CSS 圆角；
- 失焦自动收起；Esc 收起；点「显示轻贴」先收菜单再唤回主窗口；
- **懒创建**：第一次右键才建窗口，不右键完全不占第二个 WebView2 的内存；
- **外观参数每次弹出推送**：`MenuLook { solid, round, alpha, dark }` 由 Rust 算好随 `tray-menu-open` 事件下发（页面不做读取），保证「改完浓度/圆角再右键，菜单立刻跟着变」；页面首次加载用 `tray_menu_look` 拉一次。

菜单定位：默认弹在光标**上方**（托盘多在屏幕下缘），按显示器工作区夹取，放不下则翻到下方，多屏与负坐标显示器也算对。

### 7.3 WebView2 进程复用

菜单窗口的 `additional_browser_args` **必须与主窗口逐字一致**。WebView2 只有在「同一 user data 目录 + 完全相同的一组参数」下才会共享同一个浏览器进程，否则会另起一个进程组，**内存直接翻倍**。实现上直接从主窗口配置里读取那串参数，避免两处漂移。

---

## 八、前端（`src/`）

### 8.1 结构

`index.html`：

- `.titlebar`：品牌（logo + 「轻贴」）+ 窗口控制（最小化 / 最大化 / 关闭）
- `.sidebar`：历史 / 文本 / 图片 / 文件 / 收藏，底部固定「设置」
- `.toolbar`：搜索框（设置页隐藏）
- `.view`：列表或设置页
- `.statusbar`：左侧条数、中间版本号、右侧清空按钮
- `.rz-layer`：8 个缩放热区
- 退出确认弹窗

### 8.2 视图与过滤

```
history    全部
text       kind === "text"
images     kind === "image"
files      kind === "files"（且「文件复制」开启时才展示）
favorites  favorite === true
settings   设置页
```

搜索：对 `preview` 与文件名做小写包含匹配。排序：**在当前分类里置顶的在前**，其余按时间倒序。

### 8.3 条目操作

统一改为**右键菜单**（不再占用卡片上的按钮位）：复制 / 置顶（分类名）/ 收藏 / 删除。置顶项会显示作用范围，如「置顶（文本）」。

### 8.4 渲染与内存优化

内存是这一版的重点工程，四项措施：

1. **渐进渲染**：`PAGE_SIZE = 60`，用 `.load-more` 哨兵触发下一批，DOM 规模有上界，渲染内存不随历史条数增长。
2. **图片降采样**：缩略图统一缩到最长边 `IMAGE_MAX_EDGE = 512` 后编码（质量 0.82），前端不再持有原图。
3. **视野外释放解码位图**：`observeImages` 是**双向**的 —— 进入可视区（700px 内）加载 `src`，离开可视区则 `removeAttribute("src")` 释放解码后的位图，用 `data-img-loaded` 状态机管理。这一项是内存大户的真正解法：54 张图片的解码位图曾让整体占用冲到 207MB。
4. **Chromium 子系统裁剪**：`additionalBrowserArgs` 关闭大量未使用子系统（翻译、媒体路由、自动填充、后台网络、拼写检查、崩溃上报…），`--renderer-process-limit=1`，`--js-flags=--max-old-space-size=64` 收缩 V8 堆。

> 内存测量口径提醒：`tasklist` 的工作集**包含共享页**，会严重虚高；应看任务管理器的**专用工作集**或 `\Process(*)\Working Set - Private`。同一时刻两种口径能差出一倍以上。

### 8.5 深色模式

跟随系统：`getCurrentWindow().theme()` + `onThemeChanged`，切换 `body.dark` 类。Rust 侧在 `ThemeChanged` 事件里重刷材质。CSS 变量在 `body.dark` 作用域整体替换。

---

## 九、设置项（`settings.rs`）

持久化到 `%APPDATA%\com.kunge.selfclipboard\config.json`：

| 键 | 默认 | 说明 |
|---|---|---|
| `tray_on_close` | false | 关闭按钮 → 隐藏到托盘（而非退出） |
| `clean_on_close` | false | 退出时清理内容缓存与历史 |
| `clean_include_pinned` | false | 清理时是否连置顶/收藏一起清 |
| `clean_confirm_exit` | false | 退出前弹窗确认 |
| `retention_days` | 3 | 保留天数，`0` = 永久 |
| `material_alpha` | 62 | 材质浓度（25–90，Rust 侧夹到 40–90） |
| `round_mode` | false | 圆角模式（圆角 + 无系统模糊 / 直角 + 系统模糊） |
| `file_copy` | **false** | 是否记录文件复制（默认关闭） |
| `cache_dir` | null | 自定义缓存目录（null = app_data_dir/clipboard） |

开机自启：不落在 config.json，而是直接读写注册表 `HKCU\...\Run`（`autostart.rs`，手写 FFI）。

失败兜底：`config.json` 解析失败时打印日志并退回默认配置，不阻断启动。

---

## 十、IPC 命令清单（`lib.rs`）

| 命令 | 作用 |
|---|---|
| `effect_info` | 返回实际生效的材质档位 / build / 圆角上限 / 圆角模式 |
| `set_material_alpha` | 设置材质浓度并立即重刷 |
| `set_round_mode` | 切换圆角模式并立即生效 |
| `drag_start` / `resize_start` | 自定义拖动 / 缩放 |
| `get_history` | 取历史（元数据数组） |
| `get_image` | 取某条图片数据 |
| `copy_back` | 写回剪贴板（文本 / 图片 / 文件），返回可选提示 |
| `delete_item` / `delete_items` / `clear_all` | 删除 / 批量删除 / 清空 |
| `toggle_pin` / `toggle_favorite` | 置顶（带分类）/ 收藏 |
| `check_files_alive` | 批量检测文件条目是否失效 |
| `cache_info` / `pick_cache_dir` / `open_cache_dir` | 缓存占用 / 选目录 / 打开目录 |
| `get_settings` / `set_*` | 读取与修改各项设置 |
| `exit_app` | 前端确认后执行退出清理并退出 |
| `tray_show_main` / `tray_quit` / `tray_menu_look` | 托盘菜单用 |

权限（`capabilities/default.json`）覆盖 `main` 与 `tray-menu` 两个窗口：`core:default`、`core:window:*`（close / hide / show / set-focus / minimize / maximize / toggle-maximize / start-dragging / theme）、`core:webview:default`、`core:event:default`。

> **注意**：改动 `capabilities` 后必须重新 `cargo build`，权限才会在编译期嵌入 exe。

---

## 十一、数据与隐私边界

| 原则 | 落实 |
|---|---|
| 数据全本地 | 无网络请求；不引入任何上报 SDK |
| 内容落盘 | 每条记录都有对应实体文件，用户可直接查看/备份 |
| 目录可迁移 | 自定义缓存目录，改完立即生效 |
| 可一键清空 | 工具栏清空按钮 + 退出清理 + 保留天数 |
| 置顶/收藏保护 | 清理逻辑一律豁免，除非显式勾选「连置顶一起清」 |
| 敏感默认保守 | 文件复制默认关闭；不记录剪贴板原文到日志 |

---

## 十二、路线图（尚未实现）

以下是开发前规划、当前**有意未实现**的部分，按依赖顺序排列。每一项都保留了设计意图，供后续推进。

### 12.1 全局快捷键与快捷面板

**核心交互闭环**：

```
写文档中 ──► Ctrl+Shift+V（全局热键，可开关）
                 │
                 ▼
        快捷面板在光标附近弹出（唤起即聚焦搜索）
                 │
                 ├─► 打字搜索
                 ├─► ↑↓ 选择，Enter 粘贴 ──► 面板消失，焦点回到原窗口
                 └─► Esc / 失焦 ──► 面板消失
```

设计要点：

- `RegisterHotKey` 注册，默认 `Ctrl+Shift+V`，可录制自定义；**可一键禁用**（与其他软件抢键、演示防误触）；
- 热键冲突**明确提示，不静默失败**；
- `Enter` = 粘贴并关闭；`Ctrl+Enter` = 只复制不粘贴；
- 记住上一个前台窗口，用于粘贴回填；
- **先做半自动**（复制 + 关闭面板 + 焦点回上一窗口，用户按 `Ctrl+V`），验证是否真刚需，再考虑 `SetForegroundWindow + SendInput` 全自动。

### 12.2 更多格式

- 富文本 `CF_HTML` / `RTF`（供原格式粘贴、纯文本粘贴）；
- 链接识别（`http(s)://` 自动归类）。

### 12.3 隐私排除

按来源进程排除（密码管理器 / 银行 / 无痕窗口），提供一键暂停录制。

### 12.4 性能与规模

- 列表**虚拟滚动**（只渲染可视区 ± 若干屏），替代当前的渐进渲染，应对十万条量级；
- 索引**按月分片**，写盘只碰当前分片；
- 搜索改到 Rust 侧流式扫描（当前是前端在元数据上过滤）。

### 12.5 交互增强

- 多选合并复制（`Shift+↑↓`）；
- 条目编辑 / 另存为；
- 首次引导（介绍基本用法）；
- 单条 > 1MB 的「超大条目」专门处理（索引只记字符数 + 路径）。

---

## 十三、开发原则

1. **内容永不常驻内存**：只留索引元数据 + 可视区数据，正文按需读盘。
2. **系统能力优先于自研**：监听用 `AddClipboardFormatListener`、判重用 `GetClipboardSequenceNumber`、材质用系统 API；自研轮询与内容哈希全部淘汰。
3. **版本适配必须显式降级，绝不静默失败**：调用前先探测版本、分档，每档都有可观测日志与降级路径。
4. **调用系统 API 后必须验证是否真的生效**：「没报错」≠「生效了」，必要时补强制刷新（nudge）。
5. **主线程不做阻塞 IO**。
6. **一次只改一个变量，改完实测再继续**。
7. **性能改动要有前后数据**，不接受「感觉快了」。
8. **隐私默认优先**。
9. **不写没验证的代码**：每个功能点交付前跑通「写 → 验 → 报」。
10. **不引入不必要的原生依赖**，代价要写进文档。

---

## 十四、已解决问题记录

按「现象 → 根因 → 解法」记录，避免重复排查。

### 14.1 剪贴板

| 现象 | 根因 | 解法 |
|---|---|---|
| 复制文件后，**点击文件条目必崩**（进程无提示消失） | `read_clipboard` 读了 `CF_HDROP` 后调用 `DragFinish(h)`，但该句柄来自 `GetClipboardData`、内存归剪贴板所有。`DragFinish` 会 `GlobalFree` 它，而剪贴板仍持有同一句柄 → 下一次 `OpenClipboard()/EmptyClipboard()` **二次释放** → 堆损坏 | 删除 `DragFinish` 调用（连 `extern` 声明一起删，防回潮）。对应社区原话：「Calling DragFinish() will cause the application to crash on the next OpenClipboard() call」 |
| 自己复制的内容又被记一遍（重复条目） | `copy_back` 在工作线程，`WM_CLIPBOARDUPDATE` 投递到主线程，存在主线程抢跑窗口 | 用 `GetClipboardOwner() == 本窗口` 判定，无窗口期 |
| 图片不显示 / 全黑 | `dib_to_bmp` 漏了 40 字节 `BITMAPINFOHEADER` 前提，且 32bpp 的 alpha 未置 255 | 修正 DIB→BMP 拼接，强制 alpha=255 |
| 文件条目在缓存目录里没有实体 | 只把路径内联写在 `index.json` | 落盘 `files/<id>.txt`，并对老数据 `backfill_files()` |

### 14.2 窗口与材质

| 现象 | 根因 | 解法 |
|---|---|---|
| 材质静默失效 | `GetVersionEx` 在无 manifest 的进程里 Win10 返回 6.2（Win8），走错分支 | 改用 `RtlGetVersion` |
| 材质不渲染 | 调用成功但缺 DWM 重算 | 显示后 200/600/1200ms 补涂 + nudge |
| 拖动延迟 / 幽灵窗口 | 系统拖动在高轮询率鼠标下让 DWM 事件积压 | 自定义拖动 + 16ms 节流 |
| 缩放严重滞后 | 系统缩放循环无法节流 | 收回 `WM_NCHITTEST`，改自定义缩放热区 + 节流 |
| 四角有直角阴影 | tao 保留 `WS_CAPTION｜WS_THICKFRAME`，DWM 沿窗口矩形画 1px 边框 | `strip_window_frame()` 清样式位 + `SWP_FRAMECHANGED` |
| 聚焦后圆角消失 | `WM_NCACTIVATE` 触发原生标题栏重绘 | `install_focus_hook()` 拦截 |
| 圆角锯齿很重 | `SetWindowRgn` 区域是 1-bit、无抗锯齿 | 放弃区域裁剪，圆角交 CSS（`region_radius()` 恒 0） |
| 圆角与系统模糊无法共存 | `SetWindowRgn` 对 acrylic 无效，四角必有矩形残留 | 提供「圆角模式」二选一（见 5.3） |
| 纯色背景上材质「发平」 | Win10 accent API 只有单色 tint，无 luminosity 层 | tint 压到 8%，明暗改由 CSS 层负责 |

### 14.3 构建与交付

| 现象 | 根因 | 解法 |
|---|---|---|
| 编译报 `LNK1104 无法打开文件 ... self-clipboard.exe` | 应用正在运行时锁住 exe（与 `deps/` 下的硬链接） | 编译前先关掉应用 |
| 新前端改动没进 exe | 前端资源需要重新嵌入 | 改完前端 `touch src-tauri/src/lib.rs` 再 `cargo build` |
| `npm install` 报 `EBUSY` | 托管 Node 的 `spawnSync` 被拦截，esbuild 探版本失败 | 用系统 Node 跑 npm / vite |
| `schemars` 编译失败（`struct takes 3 generic arguments but 2 were supplied`） | `indexmap` 1.9.3 无 default feature，新版 rustc 上 build.rs 的 autocfg 探测失败 | `build-dependencies` 显式声明 `indexmap = { version = "1.9", features = ["std"] }` |
| NSIS 工具链下不下来 | `github.com` 被代理拦，Tauri 自带下载器断连 | 用 `gh release download` 手动拉 `nsis-3.11.zip` 与 `nsis_tauri_utils.dll`（校验值一致），按 Tauri 期望布局铺到 `%LOCALAPPDATA%\tauri\NSIS\` |
| gh 上传安装包后文件名中文丢失 | gh CLI 处理中文文件名有问题 | 改用 ASCII 文件名上传 |
| `tray.js` 构建失败 | 用了顶层 `await`，而 vite 目标含 `safari13` | 包进 async IIFE |

---

## 十五、技术选型决策记录（ADR）

| 编号 | 决策 | 备选 | 理由 | 代价 | 复审条件 |
|---|---|---|---|---|---|
| **ADR-001** | 用 **Tauri 2** | Electron + N-API、WPF + WebView2、WinUI 3 | 安装包 1.3MB（vs 89MB）；Rust 直调 Win32；无 node-gyp / ABI 问题；现有 CSS 可复用 | 需写 Rust | 若 Rust 维护成本过高 |
| **ADR-002** | 存储用 **JSON（索引 + 分文件实体）** | SQLite | 零依赖、可直接用记事本排查；本项目只需子串检索 | 全文检索能力弱 | 历史过 10 万条且检索变慢时 |
| **ADR-003** | 材质**手写 FFI** | `windows` crate | 只需几个函数；避免与 tauri 内部依赖版本漂移 | 需自行维护声明 | 需要的 API 大幅增多时 |
| **ADR-004** | **不做 `SetWindowRgn` 裁剪** | 区域裁剪出圆角 | 对 acrylic 无效且 1-bit 锯齿重 | 圆角只能二选一（见 5.3） | Win10 支持系统级圆角时 |
| **ADR-005** | 「圆角 / 直角」**交给用户选** | 固定一种 | 两种模式各有取舍，无法两全 | 多一个设置项与两条代码路径 | — |
| **ADR-006** | 托盘菜单**自绘窗口** | 原生 `TrackPopupMenu` | 原生菜单字体/配色/圆角不可控，与本体割裂 | 多一个 WebView2 窗口占用（懒创建） | 若内存增量不可接受，退化为原生自绘（无真模糊） |
| **ADR-007** | 文件复制**默认关闭** | 默认开启 | 涉及真实磁盘路径引用，且历史上有过崩溃；默认不采集最稳 | 用户需主动开启 | — |
| **ADR-008** | 置顶**按分类独立** | 全局置顶 | 「文本里置顶」不应影响它在「历史」里的位置 | 多一个 `pinned_views` 数组与迁移逻辑 | — |

---

## 十六、已知限制

- **无全局快捷键**（见 12.1）。唤出靠打开窗口或托盘双击。
- **无虚拟滚动**：当前用渐进渲染（每批 60 条）控制 DOM 规模；历史量极大时首屏之后的滚动仍会累积节点。
- **内存不可能压到几十 MB**：窗口由系统 WebView2 承载，多进程模型本身有基础开销；当前在保留真毛玻璃与图片缩略图的前提下已尽量压到底。
- **文件条目是引用而非快照**：原文件变动后粘出的是最新内容，删除后条目失效。
- **文件条目恒为「复制」语义**：只写 `CF_HDROP`、未写 `Preferred DropEffect`，从历史粘出来永远是复制（原 `Ctrl+X` 剪切不会删原文件）。
- **仅 Windows**：材质、托盘、自启均基于 Win32，代码里以 `#[cfg(windows)]` 隔离，未做跨平台适配。
- **Win10 材质观感受系统版本影响**：纯色背景上（尤其低浓度时）仍会比 Win11 平淡。

---

## 十七、性能与兼容性

### 17.1 目标与实测

| 指标 | 目标 | 现状 |
|---|---|---|
| 安装包体积 | < 10 MB | **1.3 MB** |
| 空闲 CPU | ≈ 0 | 事件驱动，无轮询 |
| 常驻内存 | 尽量低 | 启动约 120 MB（WebView2 基础开销）；大量带图历史下的峰值已通过「视野外释放位图」显著压低 |
| 冷启动到窗口可见 | < 1.5 s | 材质在显示前就位，无闪白 |

### 17.2 兼容性矩阵

| 系统 | build | 材质档位 | 圆角上限 |
|---|---|---|---|
| Win10 1809 | 17763 | acrylic | 20px |
| Win10 21H2 / 22H2 | 19044 / 19045 | acrylic | 20px |
| Win11 21H2 | 22000 | Mica | 12px |
| Win11 22H2 及以上 | ≥ 22621 | 系统级 acrylic | 12px |
| 更低版本 / 调用失败 | — | 不透明兜底 | 12px |

启动日志会打印实际 build 号与生效档位，不接受「调用没报错就算成功」。

---

## 十八、代码管理与发布

### 18.1 分支模型

| 分支 | 用途 |
|---|---|
| `main` | 稳定可发布，只接受来自 `develop` 的合并 |
| `develop` | 集成分支，日常开发汇总 |
| `feature/*` / `fix/*` | 功能与修复分支 |
| `legacy/electron-v1` | v1（Electron）归档，不再演进 |

**严禁在 `main` 直接提交或推送。**

### 18.2 提交规范

Conventional Commits：

```
feat / fix / perf / refactor / docs / test / build / chore

feat(clipboard): 文件条目清单落盘 files/<id>.txt
fix(clipboard): 移除对剪贴板 HDROP 的 DragFinish，修点击文件条目必崩
perf(memory): 视野外释放图片解码位图
```

### 18.3 版本号

SemVer `MAJOR.MINOR.PATCH`。`v1.3.x` 为 Electron 时代，v2 从 `v2.0.0` 起。

### 18.4 发布流程

1. `develop` → `main`（`--no-ff` 合并），打 tag；
2. `npm run tauri build` 产出 NSIS 安装包；
3. GitHub Release 附安装包（**用 ASCII 文件名**）；
4. Release notes 写明新增 / 修复 / 已知问题；
5. 安装包体积列为检查项。

### 18.5 安全红线

- Token / PAT 不写进 URL、配置文件、代码或提交信息（用系统凭据管理器）；
- 用户数据（`history` / `config` / 缓存）不入库；
- 剪贴板内容不进日志；
- 不提交未验证的代码。

---

## 附：术语

- **acrylic**：Windows 的半透明模糊材质，带噪点，覆盖窗口矩形。
- **Mica**：Win11 的材质，采样壁纸而非实时模糊，观感接近但不透明。
- **build 号**：Windows 内部版本号（`RtlGetVersion` 取得），是判断材质能力的依据。
- **nudge**：对窗口做一次微小的尺寸调整，用于强制 DWM 重算并重绘材质。
- **渐进渲染**：分批把条目插入 DOM（每批 60 条）并预渲染下一批的哨兵。
