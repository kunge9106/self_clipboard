# 轻贴 v1（Electron）代码审查记录

## 一、审查范围与版本

- 对象：`main.js`（823 行）、`preload.js`、`renderer/app.js`（306 行）、`renderer/index.html`、`package.json`、`scripts/afterPack.js`
- 版本：`v1.3.10`，Electron `^42.3.2`
- 平台：Windows

上述 v1 代码已不在当前仓库中。v2 使用 Tauri 重写，v1 归档于分支 `legacy/electron-v1`。本文件记录 v1 归档期的审查结果与修复情况，不描述当前代码。

本次记录问题 28 项，编号 P0-1 至 P2-20 按严重程度排列，另有逻辑缺陷 B1 至 B8。P0-1 按仓库所有者要求保留，其余问题已在当次迭代修复，逐条见第六节。

## 二、安全问题

### P0-1 剪贴板历史明文落盘、无条数上限、无排除机制

位置：`main.js:112-120`、`main.js:421-424`、`main.js:22`、`main.js:206`

```js
fs.writeFileSync(HISTORY_PATH, JSON.stringify(history), "utf-8");
```

现象与成因：

- `history.json` 以明文 JSON 写入 `%APPDATA%`。剪贴板会承载密码、Token、私钥、身份证号，本地任意进程、恶意软件、取证工具均可一次性读取全部历史。
- 代码注释写明不限制历史记录条数，见 `main.js:22`。文件随使用时间持续增长，风险随时间累积。
- 图片以 base64 内联写入 `history.json`，见 `main.js:206`，单条记录可达数十 MB。

建议：

1. 增加条数上限，例如 1000 条，并对单条内容截断，例如 1MB。
2. 提供按来源进程或窗口标题排除的隐私模式，用于密码管理器、银行站点。前台进程名可用 `active-win` 或 `GetForegroundWindow` 获取。
3. 检测到密钥特征或用户手动标记的敏感条目只保留在内存，不落盘。
4. 中远期改用 `safeStorage` API 加密存储，或迁移到 SQLite + WAL。

状态：未修复。仓库所有者要求保留无条数上限，自用场景接受该风险。`RETENTION_DAYS` 默认值仍为 3，与无上限冲突，需要无上限时改为 0。

### P0-2 `shell.openPath()` 可被配置注入劫持为任意执行

位置：`main.js:370-375`、`main.js:381-385`

```js
ipcMain.handle("set-config", (event, cfg) => { saveConfig(cfg); });   // 无校验，全量覆写
ipcMain.handle("open-cache-dir", () => { shell.openPath(getImageDir()); });
```

现象与成因：

- `set-config` 对渲染进程传入的对象不做字段白名单校验，直接 `JSON.stringify` 落盘。
- 落盘后的 `imageCacheDir` 被 `getImageDir()` 用于 `mkdirSync(recursive:true)` 与 `shell.openPath()`。
- `shell.openPath()` 对可执行文件是执行语义。攻击者通过 XSS 或本地篡改 `config.json` 将 `imageCacheDir` 指向 `.exe` / `.bat` / `.lnk` 后，点击缓存目录即触发本地代码执行。
- 同一路径进入 `getImageDir()` 的 `mkdirSync`，可创建任意目录。

原修复方案：

```js
const ALLOWED_KEYS = ["trayOnClose", "cleanImagesOnClose", "imageCacheDir"];
ipcMain.handle("set-config", (event, cfg) => {
  const next = {};
  for (const k of ALLOWED_KEYS) if (k in cfg) next[k] = cfg[k];
  if (next.imageCacheDir) {
    const p = path.resolve(next.imageCacheDir);
    if (!fs.statSync(p, {throwIfNoEntry:false})?.isDirectory()) delete next.imageCacheDir;
    next.imageCacheDir = p;
  }
  IMAGE_DIR = null;
  saveConfig(next);
});
```

`open-cache-dir` 增加前置判断：`fs.statSync(dir).isDirectory()` 成立时才调用 `shell.openPath`，否则改用 `shell.showItemInFolder`。

状态：已修复。落地时白名单命名为 `CONFIG_KEYS`，`imageCacheDir` 强制 `path.resolve` 与 `isDirectory()` 校验，非目录直接拒绝。

### P0-3 `asar: false` 与默认缓存目录位于安装目录

位置：`package.json:25`、`main.js:32-48`

现象与成因：

- `asar: false` 使 `main.js`、`preload.js`、`renderer/*` 以明文散落在安装目录。可直接改写 `main.js` 注入代码，下次启动生效；安装包无数字签名时，这是持久化后门入口。
- 默认缓存目录取 `path.dirname(process.execPath)`，通常是 `Program Files` 下的目录。普通用户无写权限，写入失败或静默触发 UAC；数据写入程序目录也违反 Windows 应用规范，卸载与升级时易丢失。

修复：`asar` 改回 `true`；默认缓存目录改为 `app.getPath("userData")`。坚持 `asar: false` 时必须增加代码签名。

状态：已修复。`asar` 置为 `true` 并补 `asarUnpack: ["assets/**"]`，图标走解包以规避 asar 下 `nativeImage` 加载问题；`getDefaultCacheDir()` 固定返回 `app.getPath("userData")`。

### P0-4 生产构建未关闭 DevTools

位置：`main.js:572`、`renderer/app.js:5`

`devTools: true`；渲染进程仅在 `renderer/app.js:5` 用 `preventDefault()` 拦截 F12。

现象与成因：F12 被拦截，但 `Ctrl+Shift+I`、`Ctrl+Shift+J`、页面右键菜单、`--remote-debugging-port` 启动参数均可绕过。配合 P0-2 与 P0-3，攻击者取得 DevTools 后可在渲染进程任意调用 `window.clipboardAPI.*`，其中 `set-config` 无校验，形成完整利用链。

修复：

```js
devTools: !app.isPackaged,
```

主进程增加导航与新窗口拦截：

```js
mainWindow.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
mainWindow.webContents.on("will-navigate", (e, url) => {
  if (!url.startsWith("file://")) e.preventDefault();
});
```

状态：已修复。`devTools` 改为 `isDev`；新增 `setWindowOpenHandler(deny)` 与 `will-navigate` 拦截非 `file://` 导航。渲染进程另加 `contextmenu` 屏蔽、F12 与 `Ctrl+Shift+I/J/C` 拦截、`sandbox: true`。

### P0-5 缺少 CSP，渲染进程无纵深防御

位置：`renderer/index.html`

`renderer/index.html` 无 `<meta http-equiv="Content-Security-Policy">`。渲染进程文本赋值使用 `textContent`，无注入面；但 `img.src = item.content` 直接消费历史数据，历史数据被污染或后续引入模板引擎时即构成 XSS。

修复：

```html
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; script-src 'self'; style-src 'self'; img-src file: data:; connect-src 'none'">
```

状态：已修复。`index.html` 加入 meta CSP，`default-src 'none'`，仅放开 `self` / `file:` 的脚本样式与 `file: data:` 图片。

### P1-6 `deleteImageFile` 前缀匹配可删除缓存目录外文件

位置：`main.js:82-95`

```js
if (p.indexOf(dir) === 0) fs.unlinkSync(p);
```

`dir = "C:\Cache"` 时，`p = "C:\CacheBackdoor\x.png"` 同样通过检查。

修复：

```js
const rp = path.resolve(p), rd = path.resolve(dir);
if (rp.startsWith(rd + path.sep)) fs.unlinkSync(rp);
```

状态：已修复。新增 `isPathInside()`，用 `path.resolve` 与 `path.sep` 前缀校验，`deleteImageFile` 与 `copy-to-clipboard` 加载图片均调用该校验。

### P1-7 `execSync` 拼接命令并经 `cmd.exe` 中转

位置：`main.js:446`、`main.js:473`、`main.js:483`、`main.js:443`

- 用模板字符串把 `process.execPath` 拼进 `reg add`。路径可控，但仍为拼接模式，且 `reg delete` 无条件执行。
- `execSync` 经 `cmd.exe /d /s /c` 中转，每次调用额外拉起一个 cmd 进程，约 30–80ms，并受 `PATH` 环境污染影响。
- `get-auto-start` 见 `main.js:443`，在主进程同步阻塞 3s 超时，期间 UI 无响应。

修复：改用 `spawnSync("reg", [...], { windowsHide: true })` 传参数数组，`shell: false`；或直接使用 `app.setLoginItemSettings()`，跨平台且不操作注册表。

状态：已修复。改用 `spawnSync("reg", [...args])`，参数数组传参、无 shell 中转、`windowsHide: true`。

### P1-8 文件 URL 解析重复且不完整

位置：`main.js:86-89`、`main.js:282-284`、`main.js:432-433`、`main.js:203`

`replace(/^file:\/\/\//,"").replace(/\//g,"\\")` 在三处重复。

- 写入时用 `encodeURI`，见 `main.js:203`；读取时未做 `decodeURIComponent`。含中文或空格的路径出现编码不一致。
- 手动拼接路径无法处理包含 `file://` 字面量的合法路径。

修复：抽出公共函数 `fileUrlToPath(url)`，用 `decodeURIComponent` 与 `path.normalize` 统一处理。

状态：已修复。统一为 `fileUrlToPath()` 与 `pathToFileUrl()`，包含 `decodeURIComponent`。

## 三、性能问题

### P0-9 轮询每秒对剪贴板图片做整幅 PNG 编码

位置：`main.js:194-210`

```js
const img = clipboard.readImage();
if (!img.isEmpty()) {
  const hash = getImageHash(img);   // → img.toPNG() 全量编码
```

剪贴板中存在图片时，即使该图片是十分钟前复制，每秒仍执行一次完整 PNG 编码。4K 截图单次编码约 100–300ms，并分配数十 MB 内存，表现为持续 10%–30% CPU 占用与每秒数十 MB 内存波动。

修复：

1. 编码前先做廉价判重：比较 `img.getSize()` 的宽高，再用 `img.toBitmap()` 采样若干像素，判定为不同时才调用 `toPNG()`。
2. 编码与写盘移出主进程：使用 `worker_threads`，至少改用 `fs.promises.writeFile`。
3. 根本方案：用 `AddClipboardFormatListener` 替代轮询，通过 `ffi-napi`、`node-addon-api` 或 Electron 的 `clipboard` 配合原生模块实现事件驱动。
4. 最小改动：`POLL_INTERVAL` 提高到 1500–2000ms，并在窗口隐藏或系统空闲时降频。

状态：已修复。新增 `getImageFingerprint()`，用尺寸与 `toBitmap()` 采样生成指纹，指纹未变时跳过 PNG 编码；删除 `getImageHash()` 内的 `toPNG()`，新图只编码一次。

### P0-10 `saveHistory()` 每次新增全量同步写盘

位置：`main.js:114-120`、`main.js:241`

用户每次复制触发 `JSON.stringify(全量 history)` 与 `writeFileSync`，阻塞主进程。历史达到 10MB 量级时，单次序列化与写入为 50–200ms，表现为复制后界面停顿，期间轮询与 IPC 全部停滞。

修复：写入 debounce 500ms 合并并空闲写入；改为增量存储，使用 SQLite 或按日期分片 JSON；至少改用 `fs.promises.writeFile`。

状态：已修复。`saveHistory()` 改为 800ms debounce 加 `fs.promises` 异步写，`before-quit` 用 `saveHistoryNow()` 强制落盘。

### P0-11 `sendHistoryToRenderer()` 每次全量推送历史

位置：`main.js:261-269`

每次新增都执行 `send("clipboard-history", history)`，全量序列化跨进程传输。历史 5000 条时单次传输可达数 MB，IPC 序列化开销显著。

修复：推送增量 `{ op: "add", item }` 与 `{ op: "remove", id }`，渲染进程本地维护列表；首屏分页，一次 100 条加滚动加载。

状态：已修复。改为 `init` / `add` / `remove` / `clear` 增量操作，渲染进程本地维护列表。

### P1-12 渲染进程 `render()` 全量重建 DOM，每个条目绑定独立闭包

位置：`renderer/app.js:39-89`

```js
listEl.innerHTML = "";                        // 全量销毁
for (const item of di) {
  const div = document.createElement("div");
  div.addEventListener("click", async () => {...});   // 每个条目一个闭包
}
```

- 每次历史更新销毁并重建全部节点，上千条时触发大量 reflow。
- 每个条目一个闭包监听器，1000 条即 1000 个常驻监听器。
- 图片条目直接 `img.src = 原始 PNG 路径`，无缩略图、无懒加载，100 张 4K 截图同屏占用数 GB 显存与内存。

修复：事件委托，在 `listEl` 上挂一个 `click`，用 `e.target.closest(".item").dataset.id` 定位；增量 diff，只在新增与删除时操作对应节点；保存时同步生成 200px 宽的 `thumb_*.webp`，列表只加载缩略图并设置 `loading="lazy"`；虚拟滚动，只渲染可视区与缓冲区。

状态：已修复。改为 `listEl` 事件委托；保存时生成 240px 缩略图，列表用缩略图并加 `loading="lazy"` 与 `decoding="async"`；分批渲染，`RENDER_LIMIT_STEP = 150`，滚动到底加载下一批。

### P1-13 `startTimer()` 每秒全量过滤并遍历 DOM 更新时间

位置：`renderer/app.js:92-100`

```js
timer = setInterval(() => {
  const items = filterItems(searchQuery);   // 每秒全量 filter，生成新数组
  els.forEach((el, i) => el.textContent = formatTime(items[i].timestamp));
}, 1000);
```

每秒一次 O(n) 过滤与 O(n) DOM 写入。`formatTime` 在「X 秒钟前」粒度下每 60 秒才变化一次，其余 59 次为无效写入，并触发 layout。

修复：间隔提高到 30s，或按最小粒度变化周期动态调整；只更新可见项；`textContent` 相同时跳过赋值。

状态：已修复。定时器由 1s 改 30s，`textContent` 相同时跳过赋值，用 `idIndex` Map 替代 `find`。

### P1-14 拖拽窗口时每个 `mousemove` 触发一次 IPC 与 `setBounds`

位置：`renderer/app.js:199-208`、`main.js:393-398`

```js
window.clipboardAPI.dragWindow(dx, dy);     // 每个 mousemove 事件一次 IPC
mainWindow.setBounds(...);                   // 每次同步重排窗口
```

一个 `mousemove` 序列每秒触发 60–120 次，每次两次跨进程往返加 DWM 重排，拖拽掉帧、跟手性差。

修复：渲染进程用 `requestAnimationFrame` 合并位移，每帧最多一次；主进程用 `mainWindow.setPosition()` 替代 `setBounds()`；或使用 CSS `-webkit-app-region: drag` 交给系统原生拖拽。

状态：已修复。渲染进程用 `requestAnimationFrame` 合并位移，每帧最多一次 IPC；主进程改用 `setPosition`。

### P1-15 `get-image-cache-info` 每次全量 `readdirSync` 与 `statSync`

位置：`main.js:353-368`、`renderer/app.js:112`

`renderer/app.js:112` 在每次历史更新时调用 `refreshCacheStats()`，每次复制即对缓存目录做一次全量遍历与 stat。缓存 2000 个文件时单次阻塞主进程 200ms 以上。

修复：内存维护 `{count, size}` 计数器，保存与删除文件时增量更新，仅在启动和切换目录时全量重算。

状态：已修复。`cacheStats` 增量维护，保存时计数与体积累加，删除或切换目录时标脏。

### P1-16 保存图片使用 `writeFileSync` 同步写入大 PNG

位置：`main.js:74`

数十 MB 的 PNG 同步写入直接冻结主进程。改为 `fs.promises.writeFile`。

状态：已修复。`saveImageToFile` 改为 `await fs.promises.writeFile`。

### P1-17 启动时 `loadHistory()` 同步全量读取并循环删除文件

位置：`main.js:122-140`

冷启动同步执行 `readFileSync`、大文件 `JSON.parse` 与逐个 `unlinkSync`，阻塞首屏。改为异步读取并在后台清理。

状态：第六节修复记录未列出此项，状态待确认。

### P1-18 高频 `console.log`

位置：`main.js:185`、`main.js:586-588`

`main.js:185` 每秒打印剪贴板文本预览；`main.js:586-588` 把渲染进程全部日志转发到主进程 stdout。打包后 stdout 重定向到文件时形成持续磁盘写入，且日志包含剪贴板内容，叠加隐私风险。

修复：统一封装 `log()`，仅在 `isDev` 时输出；生产版关闭或写滚动日志。

状态：已修复。新增 `log()` 仅在 dev 输出，`console-message` 转发也仅在 dev 启用。

### P1-19 `loadConfig()` 每次 IPC 调用都同步读盘并 `JSON.parse`

位置：`main.js:498-506`

`close-window`、`get-config`、`updateCacheDisplay` 频繁调用。加入内存缓存并在写入时失效。

状态：已修复。新增 `configCache` 内存缓存，`saveConfig` 时更新。

### P2-20 窗口 blur 时无条件重设 acrylic

位置：`main.js:629-635`

每次失焦用 `setTimeout` 重设 `setBackgroundMaterial("acrylic")`。频繁切换窗口时造成 DWM 反复重建材质、闪烁与额外开销。加节流，例如 500ms 内只执行一次。

状态：已修复。`refreshAcrylic()` 增加 500ms 节流。

## 四、逻辑缺陷

| 编号 | 位置 | 现象与成因 | 修复方式与状态 |
|---|---|---|---|
| B1 | `main.js:157-167` | `getImageHash` 只哈希 PNG 前 1024 字节。两张图只要 IHDR 与调色板头相同，同尺寸同来源截图即属常见情形，就会算出相同哈希，新复制的图被判为未变化而丢失。`hash = hash & hash` 为无意义的自与操作。 | 删除旧 `getImageHash`，精确判重改用 `crypto` sha1 全量哈希。已修复 |
| B2 | `main.js:421-424`、`main.js:261` | `get-history` 返回过滤 3 天后的数据，`sendHistoryToRenderer` 推送未过滤的全量。启动时显示 3 天内，新增一条后 UI 变成全部历史，前后不一致。 | 统一 `getVisibleHistory()`，`get-history` 与推送同源。已修复 |
| B3 | `main.js:244` | `addHistoryItem` 末尾多调用一次 `clipboard.readText()`。 | 删除该次调用。已修复 |
| B4 | `main.js:226` | `id: Date.now() + Math.random()`，同一毫秒内多次新增时浮点相加可能丢精度。 | 改用 `crypto.randomUUID()`，`delete-item` 兼容旧 numeric id。已修复 |
| B5 | `renderer/app.js:36` | 图片搜索逻辑 `lq.includes("图片")` 语义不符预期，搜索任意包含「图片」的词会返回全部图片。 | 改为 `图片` / `image` / `tupian` 精确匹配。已修复 |
| B6 | `main.js:274-291` | `copy-to-clipboard` 未校验 `item.content` 类型，非字符串时 `startsWith` 抛异常，形成 IPC 未捕获异常。 | `copy-to-clipboard` 校验 `item.content` 类型与来源。已修复 |
| B7 | `renderer/app.js:143` | `blur` 回调引用 `dialogOverlay`，该 const 定义在下方 229 行。运行时已初始化，不报错，但可读性差。 | 修复记录未列出此项，状态待确认 |
| B8 | `main.js:22` | 注释「不限制历史记录条数」与 UI 文案「仅保留近三天内容」矛盾。实际只在启动时做一次 3 天过滤，运行期间不清理。 | 抽出 `RETENTION_DAYS = 3` 常量，改为 0 即真正的无上限。已修复 |

## 五、修复优先级

第一批，安全底线：

1. P0-1 历史条数与单条大小上限、隐私排除
2. P0-2 `set-config` 字段白名单与 `openPath` 目录校验
3. P0-3 `asar: true` 与缓存目录迁至 `userData`
4. P0-4 `devTools: !app.isPackaged` 与导航拦截
5. P0-5 CSP
6. P1-6 路径 `resolve` 校验

第二批，性能体感：

7. P0-9 图片判重前置，收益最高，直接解决 CPU 常驻占用
8. P0-10 与 P0-11 历史存储 debounce 与增量推送
9. P1-12 事件委托、缩略图、懒加载
10. P1-13 与 P1-14 定时器与拖拽节流

第三批，架构演进：

11. 历史存储迁移 SQLite
12. 轮询改 Windows 剪贴板监听，事件驱动
13. 补 `app.setLoginItemSettings()` 替换注册表操作
14. 单元测试，`test/app-test.js` 当前 25 行，覆盖几乎为零

## 六、修复记录

### 安全

| 项 | 处理 |
|---|---|
| P0-2 `set-config` 劫持 | 新增 `CONFIG_KEYS` 白名单，含 `trayOnClose` / `cleanImagesOnClose` / `imageCacheDir`；`imageCacheDir` 强制 `path.resolve` 与 `isDirectory()` 校验，非目录直接拒绝 |
| P0-2 `shell.openPath` 执行风险 | `open-cache-dir` 先执行 `statSync().isDirectory()`，非目录不调用 |
| P0-3 `asar: false` | 改为 `asar: true`，并加 `asarUnpack: ["assets/**"]`，图标走解包以规避 asar 下 `nativeImage` 加载问题 |
| P0-3 缓存在安装目录 | `getDefaultCacheDir()` 固定为 `app.getPath("userData")` |
| P0-4 DevTools | `devTools` 改为 `isDev`；新增 `setWindowOpenHandler(deny)` 与 `will-navigate` 拦截非 `file://` |
| P0-5 CSP | `index.html` 加 meta CSP，`default-src 'none'`，仅放开 `self` / `file:` 的脚本样式与 `file: data:` 图片 |
| P1-6 目录穿越 | 新增 `isPathInside()`，用 `path.resolve` 与 `path.sep` 前缀校验，`deleteImageFile` 与 `copy-to-clipboard` 加载图片均校验 |
| P1-7 `execSync` | 改 `spawnSync("reg", [...args])`，参数数组传参、无 shell 中转、`windowsHide: true` |
| P1-8 `file://` 解析 | 统一为 `fileUrlToPath()` 与 `pathToFileUrl()`，含 `decodeURIComponent` |
| 渲染进程加固 | `contextmenu` 默认屏蔽，F12 与 `Ctrl+Shift+I/J/C` 拦截，`sandbox: true` |

### 性能

| 项 | 处理 |
|---|---|
| P0-9 每秒 `toPNG` | 新增 `getImageFingerprint()`，用尺寸与 `toBitmap()` 采样做廉价指纹，指纹未变则完全跳过 PNG 编码，仅指纹变化时调用 `toPNG()` |
| P0-9 二次编码 | 删除 `getImageHash()` 内的 `toPNG()`，新图只编码一次 |
| P0-10 全量同步写盘 | `saveHistory()` 改为 800ms debounce 加 `fs.promises` 异步写；`before-quit` 用 `saveHistoryNow()` 强制落盘 |
| P0-11 全量推送 | 改为 `init` / `add` / `remove` / `clear` 增量 op，渲染进程本地维护列表 |
| P1-12 每条目闭包 | 改为 `listEl` 上的事件委托 |
| P1-12 大图解码 | 保存时同步生成 240px 缩略图 `thumb_*`，列表渲染优先用缩略图，加 `loading="lazy"` 与 `decoding="async"` |
| P1-12 无限列表 | 新增 `RENDER_LIMIT_STEP = 150` 分批渲染，滚动到底自动加载下一批 |
| P1-13 每秒刷 DOM | 定时器 1s 改 30s，`textContent` 相同时跳过赋值；用 `idIndex` Map 替代 `find` |
| P1-14 拖拽 IPC | 渲染进程用 `requestAnimationFrame` 合并位移，每帧最多一次 IPC；主进程改用 `setPosition` |
| P1-15 全量 stat | `cacheStats` 增量维护，保存时计数与体积累加，删除或切换目录时标脏 |
| P1-16 同步写图 | `saveImageToFile` 改 `await fs.promises.writeFile` |
| P1-18 日志刷屏 | 新增 `log()` 仅在 dev 输出；`console-message` 转发也仅 dev |
| P1-19 配置重复读盘 | `configCache` 内存缓存，`saveConfig` 时更新 |
| P2-20 acrylic 闪烁 | `refreshAcrylic()` 加 500ms 节流 |

### 逻辑

| 项 | 处理 |
|---|---|
| B1 哈希只取前 1024 字节 | 删除旧 `getImageHash`；精确判重改用 `crypto` sha1 全量哈希 |
| B2 过滤口径不一致 | 统一 `getVisibleHistory()`，`get-history` 与推送同源 |
| B3 多余 `readText()` | 删除 |
| B4 `Date.now() + Math.random()` | 改 `crypto.randomUUID()`；`delete-item` 兼容旧 numeric id |
| B5 图片搜索语义 | 改为 `图片` / `image` / `tupian` 精确匹配 |
| B6 IPC 参数未校验 | `copy-to-clipboard` 校验 `item.content` 类型与来源 |
| B8 保留策略矛盾 | 抽出 `RETENTION_DAYS = 3` 常量，改为 0 即真正的无上限 |

## 七、未处理项

P0-1 历史明文、无条数上限、无排除机制：按仓库所有者要求保留无条数上限。代码中 `RETENTION_DAYS` 默认仍为 3，与无上限冲突，需要无上限时将其改为 0。

## 八、附：凭据处理

克隆时使用了含 PAT 的 URL，PAT 已明文写入 `.git/config`。已执行清理：

```
git remote set-url origin https://github.com/kunge9106/self_clipboard.git
git config --local credential.helper manager
```

当前 `.git/config` 已无 token。两个 Token 曾在对话中明文出现，应视为已泄露，需到 GitHub Settings → Developer settings → Personal access tokens 吊销并重新签发。新 Token 配置到 Windows 凭据管理器，不写入 URL 或配置文件。
