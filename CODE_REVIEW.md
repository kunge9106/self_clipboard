# self_clipboard 代码审查报告

> 审查范围：`main.js` (823行) / `preload.js` / `renderer/app.js` (306行) / `renderer/index.html` / `package.json` / `scripts/afterPack.js`  
> 版本：v1.3.10 · Electron ^42.3.2 · 平台：Windows  
> 结论：**功能可用，但存在 5 个 P0 级隐患，建议在下个迭代集中修复**
>
> **更新**：除 P0-1（历史明文/无上限，用户明确要求保留无上限，自用场景接受）外，  
> 其余问题已在本次迭代中修复，详见文末「修复记录」。

---

## 一、安全问题

### P0-1 剪贴板历史明文落盘、无上限、无排除机制

`main.js:112-120 / 421-424`

```js
fs.writeFileSync(HISTORY_PATH, JSON.stringify(history), "utf-8");  // 明文
```

- 剪贴板天然会承载**密码、Token、私钥、身份证号**。`history.json` 以明文 JSON 写在 `%APPDATA%`，任何本地进程/恶意软件/取证工具都能一次性拖走用户全部剪贴板历史。
- 代码注释写明「不限制历史记录条数」(`main.js:22`)，意味着这个文件会**无限膨胀**，风险随时间线性放大。
- 图片以 base64 内联进 history.json 时（`main.js:206`），单条可达几十 MB。

**建议**

1. 增加条数上限（如 1000）+ 单条内容上限（如 1MB 截断）。
2. 提供「隐私模式」：支持按来源进程/窗口标题排除（如密码管理器、银行站点），可用 `active-win` 或 `GetForegroundWindow` 获取前台进程名。
3. 敏感条目（检测到密钥特征或用户手动标记）不落盘，仅内存保留。
4. 中远期：改用 `safeStorage` API 加密存储，或迁移到 SQLite + WAL。

---

### P0-2 `shell.openPath()` 可被配置注入劫持为任意执行

`main.js:370-375`，配合 `main.js:381-385`

```js
ipcMain.handle("set-config", (event, cfg) => { saveConfig(cfg); });   // 无校验，全量覆写
ipcMain.handle("open-cache-dir", () => { shell.openPath(getImageDir()); });
```

- `set-config` 对渲染层传来的对象**不做任何字段白名单校验**，直接 `JSON.stringify` 落盘。
- `imageCacheDir` 随后被 `getImageDir()` 用于 `mkdirSync(recursive:true)` 和 `shell.openPath()`。
- `shell.openPath()` 对**可执行文件是执行语义**，不是打开文件夹。一旦攻击者（XSS / 本地篡改 config.json）把 `imageCacheDir` 指向 `.exe/.bat/.lnk`，点击缓存目录即触发本地代码执行。
- 同理 `getImageDir()` 的 `mkdirSync` 会造成**任意目录创建**。

**修复**

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

并给 `open-cache-dir` 加前置判断：`fs.statSync(dir).isDirectory()` 才允许 `shell.openPath`，否则改用 `shell.showItemInFolder`。

---

### P0-3 `asar: false` + 默认缓存目录在安装目录

`package.json:25`、`main.js:32-48`

- `asar: false` 让 `main.js` / `preload.js` / `renderer/*` 以**明文散落在安装目录**。任何人可直接改写 `main.js` 注入代码，下次启动即生效；安装包若无数字签名，这是标准的持久化后门入口。
- 默认缓存目录取 `path.dirname(process.execPath)`（通常是 `Program Files\...`）。普通用户无写权限 → 静默 UAC 失败或写不进去；即便成功，把用户数据写进程序目录也违反 Windows 应用规范，卸载/升级时易丢数据。

**修复**：`asar: true`（默认），默认缓存目录改为 `app.getPath("userData")`；若坚持 `asar:false` 必须加上代码签名。

---

### P0-4 生产构建仍开放 DevTools

`main.js:572` `devTools: true`，`renderer/app.js:5` 仅 `preventDefault()` 拦截 F12。

- F12 被拦了，但 `Ctrl+Shift+I` / `Ctrl+Shift+J` / 页面右键菜单 / `--remote-debugging-port` 启动参数**全部绕得过去**。
- 配合 P0-2、P0-3，攻击者拿到 DevTools 后可在渲染层任意调用 `window.clipboardAPI.*`（`set-config` 无校验），形成完整利用链。

**修复**

```js
devTools: !app.isPackaged,
```

并在 `main.js` 加导航/新窗口拦截：

```js
mainWindow.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
mainWindow.webContents.on("will-navigate", (e, url) => {
  if (!url.startsWith("file://")) e.preventDefault();
});
```

---

### P0-5 缺少 CSP，渲染层无纵深防御

`renderer/index.html` 无任何 `<meta http-equiv="Content-Security-Policy">`。当前渲染层用 `textContent` 赋值文本（安全），但 `img.src = item.content` 直接消费历史数据；一旦历史数据被污染或将来引入模板引擎，就是直给的 XSS。

**修复**

```html
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; script-src 'self'; style-src 'self'; img-src file: data:; connect-src 'none'">
```

---

### P1-6 `deleteImageFile` 前缀匹配存在目录穿越

`main.js:82-95`

```js
if (p.indexOf(dir) === 0) fs.unlinkSync(p);
```

`dir = "C:\Cache"` 时，`p = "C:\CacheBackdoor\x.png"` 同样通过检查 → 可删除缓存目录之外的文件。

**修复**

```js
const rp = path.resolve(p), rd = path.resolve(dir);
if (rp.startsWith(rd + path.sep)) fs.unlinkSync(rp);
```

---

### P1-7 `execSync` 拼接命令、经 cmd.exe 中转

`main.js:446 / 473 / 483`

- 模板字符串拼 `process.execPath` 进 `reg add`。路径受控但仍是拼接模式，且 `reg delete` 无条件执行。
- `execSync` 走 `cmd.exe /d /s /c`，每次调用额外拉起一个 cmd 进程（约 30-80ms），且受 PATH 环境污染影响。
- `get-auto-start`（`main.js:443`）在主线程同步阻塞 3s timeout，UI 会卡住。

**修复**：改用 `spawnSync("reg", [...], { windowsHide: true })` 传参数数组，`shell: false`；或直接用 `app.setLoginItemSettings()`（Electron 官方 API，跨平台、无注册表操作）。

---

### P1-8 文件 URL 解析重复且脆弱

`main.js:86-89 / 282-284 / 432-433` 三处重复的 `replace(/^file:\/\/\//,"").replace(/\//g,"\\")`。

- 未 `decodeURIComponent`，但存的时候 `encodeURI`（`main.js:203`）→ 中文/空格路径会出现编码不一致。
- 手动字符串拼路径无法处理含 `file://` 字面量的合法路径。

**修复**：抽公共函数 `fileUrlToPath(url)`，用 `decodeURIComponent` + `path.normalize` 统一处理。

---

## 二、性能问题

### P0-9 轮询每秒对剪贴板图片做全量 PNG 编码（最大性能黑洞）

`main.js:194-210`

```js
const img = clipboard.readImage();
if (!img.isEmpty()) {
  const hash = getImageHash(img);   // → img.toPNG() 全量编码
```

只要剪贴板里**存在**一张图片（哪怕是十分钟前复制的），每秒都要执行一次完整 PNG 编码。一张 4K 截图编码约 100-300ms / 数十 MB 内存分配 → **持续 10%-30% CPU 占用 + 每秒几十 MB 内存抖动**，风扇起飞、笔记本掉电。

**修复（三步）**

1. 先做廉价判重再编码：比较 `img.getSize()` 宽高 + `img.toBitmap()` 采样若干像素，不同才 `toPNG()`。
2. 编码/写盘从主线程移出：`worker_threads` 或至少改用 `fs.promises.writeFile`。
3. 根本方案：用 `AddClipboardFormatListener`（Windows 原生剪贴板通知）替代轮询，通过 `ffi-napi`/`node-addon-api` 或 Electron 的 `clipboard` + 原生模块实现，事件驱动、零空转。

退一步的最低改动：`POLL_INTERVAL` 提到 1500-2000ms，并在窗口隐藏/系统空闲时降频。

---

### P0-10 `saveHistory()` 每次新增全量同步写盘

`main.js:114-120 / 241`

用户每复制一次 → `JSON.stringify(全量 history)` + `writeFileSync` 阻塞主线程。历史无上限时，文件到 10MB 量级，单次序列化+写入就是 **50-200ms 的主线程卡顿**，表现为「复制后界面卡一下」。写盘期间轮询、IPC 全部停滞。

**修复**

- debounce 500ms 合并写入 + 空闲写。
- 改为增量存储：SQLite 或分片 JSON（`history-<date>.json`）。
- 至少改 `fs.promises.writeFile` 不阻塞事件循环。

---

### P0-11 `sendHistoryToRenderer()` 每次全量推送历史

`main.js:261-269`，每次新增都 `send("clipboard-history", history)` 全量序列化跨进程。历史 5000 条时单次传输可达数 MB，IPC 序列化开销显著。

**修复**：推送增量 `{ op: "add", item }` / `{ op: "remove", id }`，渲染层本地维护列表；首屏走分页（一次 100 条 + 滚动加载）。

---

### P1-12 渲染层 `render()` 全量重建 DOM + 每条绑定闭包

`renderer/app.js:39-89`

```js
listEl.innerHTML = "";                        // 全量销毁
for (const item of di) {
  const div = document.createElement("div");
  div.addEventListener("click", async () => {...});   // 每个条目一个闭包
}
```

- 每次历史更新都销毁重建全部节点，上千条时 **reflow 风暴**。
- 每个条目一个闭包监听器，1000 条 = 1000 个监听器常驻内存。
- 图片条目直接 `img.src = 原始 PNG 路径`，**无缩略图、无懒加载**，100 张 4K 截图同屏 = 数 GB 显存/内存。

**修复**

- 事件委托：`listEl` 上挂一个 `click`，用 `e.target.closest(".item").dataset.id` 定位。
- 增量 diff：只在新增/删除时操作对应节点。
- 图片生成 thumbnail（保存时同时写一份 200px 宽的 `thumb_*.webp`），列表只加载缩略图 + `loading="lazy"`。
- 虚拟滚动（列表只渲染可视区 + 缓冲）。

---

### P1-13 `startTimer()` 每秒全量过滤 + 遍历 DOM 更新时间

`renderer/app.js:92-100`

```js
timer = setInterval(() => {
  const items = filterItems(searchQuery);   // 每秒全量 filter，生成新数组
  els.forEach((el, i) => el.textContent = formatTime(items[i].timestamp));
}, 1000);
```

每秒一次 O(n) 过滤 + O(n) DOM 写入，且 `formatTime` 在「X秒钟前」粒度下 **60 秒才变一次**，前 59 秒全是无效 DOM 写入，白白触发 60 次 layout。

**修复**：间隔提到 30s（或按最小粒度的变化周期动态调整）；只在可见项上更新；`textContent` 相同则跳过赋值。

---

### P1-14 拖拽窗口 mousemove 每次移动都 IPC + `setBounds`

`renderer/app.js:199-208` + `main.js:393-398`

```js
window.clipboardAPI.dragWindow(dx, dy);     // 每个 mousemove 事件一次 IPC
mainWindow.setBounds(...);                   // 每次同步重排窗口
```

一个 mousemove 序列每秒可触发 60-120 次，每次 `getBounds()` + `setBounds()` 两次跨进程往返 + DWM 重排 → **拖拽明显掉帧、窗口跟手性差**。

**修复**：渲染层用 `requestAnimationFrame` 合并位移，每帧最多发一次；主进程用 `mainWindow.setPosition()` 替代全量 `setBounds()`；或用 CSS `-webkit-app-region: drag` 交给系统原生拖拽（最流畅）。

---

### P1-15 `get-image-cache-info` 每次全量 `readdirSync` + `statSync`

`main.js:353-368`，且 `renderer/app.js:112` 在**每次历史更新时**调用 `refreshCacheStats()` → 用户每复制一次，就对缓存目录做一次全量遍历 stat。缓存 2000 个文件时单次 200ms+ 主线程阻塞。

**修复**：内存维护 `{count, size}` 计数器，保存/删除文件时增量更新，只在启动和切换目录时全量重算。

---

### P1-16 保存图片用 `writeFileSync` 同步写大 PNG

`main.js:74`。几十 MB 的 PNG 同步写入直接冻结 UI 主线程。改 `fs.promises.writeFile`。

---

### P1-17 启动时 `loadHistory()` 同步全量读取 + 循环删文件

`main.js:122-140`，冷启动同步 `readFileSync` + `JSON.parse` 大文件 + 逐个 `unlinkSync`，阻塞首屏。改为异步 + 后台清理。

---

### P1-18 高频 `console.log` 刷屏

`main.js:185` 每秒打印剪贴板文本预览；`main.js:586-588` 把渲染层所有日志转发到主进程 stdout。打包后 stdout 重定向到文件时是**持续的磁盘写入**，且日志里带剪贴板内容（隐私风险叠加）。

**修复**：统一封装 `log()`，仅 `isDev` 时输出；生产版关闭或写滚动日志。

---

### P1-19 `loadConfig()` 每次 IPC 都同步读盘 + `JSON.parse`

`main.js:498-506`，`close-window`、`get-config`、`updateCacheDisplay` 频繁调用。加内存缓存 + 写时失效即可。

---

### P2-20 窗口 blur 无条件重设 acrylic

`main.js:629-635`，每次失焦 `setTimeout` 重设 `setBackgroundMaterial("acrylic")`。频繁切换窗口会造成 DWM 反复重建材质、闪烁与额外开销。加节流（如 500ms 内只执行一次）。

---

## 三、逻辑缺陷（顺带发现）

| #  | 位置                         | 问题                                                                                                                                                                                                  |
| -- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1 | `main.js:157-167`          | `getImageHash` **只哈希 PNG 前 1024 字节**。两张图只要 IHDR/调色板头相同（同尺寸同来源截图极常见）就会算出相同哈希 → **新复制的图被判定为"未变化"而丢失**。`hash = hash & hash` 是无意义的自与操作。建议改用 `crypto.createHash("sha1").update(buf).digest("hex")` 全量哈希。 |
| B2 | `main.js:421-424` vs `261` | `get-history` 返回**过滤 3 天后**的数据，`sendHistoryToRenderer` 推的是**未过滤全量**。结果：启动显示 3 天内，新增一条后 UI 突然变成全部历史 → 前后不一致。两处应共用同一个过滤函数。                                                                            |
| B3 | `main.js:244`              | `addHistoryItem` 末尾又调了一次 `clipboard.readText()`，纯多余的一次剪贴板读取。                                                                                                                                        |
| B4 | `main.js:226`              | `id: Date.now() + Math.random()` — 同一毫秒内多次新增时浮点相加可能丢精度；改用自增计数器或 `randomUUID()`。                                                                                                                     |
| B5 | `renderer/app.js:36`       | 图片搜索逻辑 `lq.includes("图片")` 语义奇怪：搜任意包含"图片"的词会返回全部图片。应按时间戳/标签搜索或明确不做图片内容检索。                                                                                                                           |
| B6 | `main.js:274-291`          | `copy-to-clipboard` 未校验 `item.content` 类型，非字符串时 `startsWith` 抛异常 → IPC 未捕获异常。                                                                                                                       |
| B7 | `renderer/app.js:143`      | `blur` 回调中引用 `dialogOverlay`，该 const 定义在下方 229 行（运行时已初始化，不报错，但可读性差，建议上移）。                                                                                                                           |
| B8 | `main.js:22`               | 注释「不限制历史记录条数」与 UI 文案「仅保留近三天内容」矛盾，实际只在启动时做一次 3 天过滤，运行期间不清理。                                                                                                                                          |

---

## 四、修复优先级建议

**第一批（安全底线，建议本次迭代全部完成）**

1. P0-1 历史条数/大小上限 + 隐私排除
2. P0-2 `set-config` 字段白名单 + `openPath` 目录校验
3. P0-3 `asar: true` + 缓存目录迁至 userData
4. P0-4 `devTools: !app.isPackaged` + 导航拦截
5. P0-5 加 CSP
6. P1-6 路径 resolve 校验

**第二批（性能体感）**  
7\. P0-9 图片判重前置（最高收益：直接解决 CPU 常驻占用）  
8\. P0-10 / P0-11 历史存储 debounce + 增量推送  
9\. P1-12 事件委托 + 缩略图 + 懒加载  
10\. P1-13 / P1-14 定时器与拖拽节流

**第三批（架构演进）**  
11\. 历史存储迁移 SQLite  
12\. 轮询改 Windows 剪贴板监听（事件驱动）  
13\. 补 `app.setLoginItemSettings()` 替换注册表操作  
14\. 单元测试（`test/app-test.js` 目前 25 行，覆盖几乎为零）

---

## 五、附：本次操作的安全处理

---

## 六、修复记录（本次已完成）

### 安全

| 项                    | 处理                                                                                                                                      |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| P0-2 `set-config` 劫持 | 新增 `CONFIG_KEYS` 白名单（`trayOnClose`/`cleanImagesOnClose`/`imageCacheDir`），`imageCacheDir` 强制 `path.resolve` + `isDirectory()` 校验，非目录直接拒绝 |


| P0-2 `shell.openPath` 执行风险 | `open-cache-dir` 先 `statSync().isDirectory()`，非目录不调用 |  
| P0-3 `asar: false` | 改为 `asar: true` + `asarUnpack: ["assets/**"]`（图标走解包，规避 asar 下 nativeImage 加载风险） |  
| P0-3 缓存在安装目录 | `getDefaultCacheDir()` 固定为 `app.getPath("userData")` |  
| P0-4 DevTools | `devTools: isDev`；新增 `setWindowOpenHandler(deny)` + `will-navigate` 拦截非 `file://` |  
| P0-5 CSP | `index.html` 加 meta CSP：`default-src 'none'`，仅放开 `self`/`file:` 的脚本样式与 `file: data:` 图片 |  
| P1-6 目录穿越 | 新增 `isPathInside()`（`path.resolve` + `path.sep` 前缀校验），`deleteImageFile` 与 `copy-to-clipboard` 加载图片均校验 |  
| P1-7 `execSync` | 改 `spawnSync("reg", [...args])`，参数数组传参、无 shell 中转、`windowsHide: true` |  
| P1-8 file:// 解析 | 统一为 `fileUrlToPath()` / `pathToFileUrl()`，含 `decodeURIComponent` |  
| — | 渲染层另加：`contextmenu` 默认屏蔽、F12/Ctrl+Shift+I/J/C 拦截、`sandbox: true` |

### 性能

| 项                | 处理                                                                                            |
| ---------------- | --------------------------------------------------------------------------------------------- |
| P0-9 每秒 toPNG    | 新增 `getImageFingerprint()`：尺寸 + `toBitmap()` 采样做廉价指纹，**指纹未变则完全跳过 PNG 编码**；仅指纹变化时才 `toPNG()`   |
| P0-9 二次编码        | 删除 `getImageHash()` 内的 `toPNG()`，新图只编码一次                                                      |
| P0-10 全量同步写盘     | `saveHistory()` 改为 800ms debounce + `fs.promises` 异步写；`before-quit` 用 `saveHistoryNow()` 强制落盘 |
| P0-11 全量推送       | 改为增量 op：`init` / `add` / `remove` / `clear`，渲染层本地维护列表                                         |
| P1-12 每条目闭包      | 改为 `listEl` 上的事件委托                                                                            |
| P1-12 大图解码       | 保存时同步生成 240px 缩略图（`thumb_*`），列表渲染优先用缩略图 + `loading="lazy"` + `decoding="async"`               |
| P1-12 无限列表       | 新增 `RENDER_LIMIT_STEP = 150` 分批渲染，滚动到底自动加载下一批                                                 |
| P1-13 每秒刷 DOM    | 定时器 1s → 30s，且 `textContent` 相同时跳过赋值；用 `idIndex` Map 替代 `find`                                |
| P1-14 拖拽 IPC     | 渲染层 `requestAnimationFrame` 合并位移，每帧最多一次 IPC；主进程改用 `setPosition`                               |
| P1-15 全量 stat    | `cacheStats` 增量维护（保存时 count++/size+=，删除/切换目录标脏）                                               |
| P1-16 同步写图       | `saveImageToFile` 改 `await fs.promises.writeFile`                                             |
| P1-18 日志刷屏       | 新增 `log()` 仅在 dev 输出；`console-message` 转发也仅 dev                                               |
| P1-19 配置重复读盘     | `configCache` 内存缓存，`saveConfig` 时更新                                                           |
| P2-20 acrylic 闪烁 | `refreshAcrylic()` 500ms 节流                                                                   |

### 逻辑

| 项                               | 处理                                                   |
| ------------------------------- | ---------------------------------------------------- |
| B1 哈希只取前 1024 字节                | 删除旧 `getImageHash`；精确判重改用 `crypto` sha1 全量哈希         |
| B2 过滤口径不一致                      | 统一 `getVisibleHistory()`，`get-history` 与推送同源         |
| B3 多余 `readText()`              | 删除                                                   |
| B4 `Date.now() + Math.random()` | 改 `crypto.randomUUID()`；`delete-item` 兼容旧 numeric id |
| B5 图片搜索语义                       | 改为「图片」/「image」/「tupian」精确匹配                          |
| B6 IPC 参数未校验                    | `copy-to-clipboard` 校验 `item.content` 类型与来源          |
| B8 保留策略矛盾                       | 抽出 `RETENTION_DAYS = 3` 常量，**改成 0 即为真正的无上限**         |

### 未处理（按你的要求保留）

- **P0-1** 历史明文 / 无上限 / 无排除机制 —— 自用场景，保留无上限。  
  注意：代码里 `RETENTION_DAYS` 默认仍是 3，与「无上限」冲突，**想要无上限请把它改成 0**。

---

- 克隆时使用了含 PAT 的 URL，**PAT 已明文写入 `.git/config`**。已执行清理：
  ```
  git remote set-url origin https://github.com/kunge9106/self_clipboard.git
  git config --local credential.helper manager
  ```
  当前 `.git/config` 已无 token。
- **强烈建议到 GitHub Settings → Developer settings → Personal access tokens 吊销这两个 Token 并重新签发**（它们在对话中明文出现过，应视为已泄露）。新 Token 请配置到 Windows 凭据管理器，不要写进 URL 或配置文件。
