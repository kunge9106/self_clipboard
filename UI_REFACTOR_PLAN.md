# UI 统一改造方案

> 状态：**待评审，未开工**。
> 范围：实现层（令牌 / 组件 / 状态 / 材质消费方式）。**不动**信息架构、布局、文案。
> 溯源：`DESIGN.md` §12.6 记的「浅色主题元素可见性」当时明确「功能收尾统一处理，不逐个临时打补丁」。
> 本文把那笔账收口，并把同一批遗留（状态、组件重复、材质档位）一次做完 —— 分开做等于改两遍。

---

## 1. 结论先说

问题不是「颜色不好看」，是**没有语义层**：

- 令牌按**层级**命名（`--glass` / `--glass-strong` / `--card-hover`），不是按**角色**（surface / border / state）；
- 浅色主题直接复用深色的「白色叠加」，白窗上等于隐形（§12.6 已定性）；
- 组件靠复制粘贴，同一件事有 2–5 份实现，且**已经漂移**（三套按钮的高/字号/内距已经不一致）。

所以每加一个控件、每换一次主题都要重新判断，漏一处就是隐形。改造目标只有一句：
**同一个视觉决定只留一处实现，其余全部引用它。**

---

## 2. 现状盘点（2026-11-05 实测，可复核）

| 项 | 数字 |
|---|---|
| 样式规模 | `styles.css` 3225 行 · `panel.css` 181 · `tray.css` 80 · `animations.css` 122 |
| 令牌 | 85 个（`:root` 80 / `body.dark` 74；几何与字体类只在 `:root`） |
| 硬编码色值 | 200 处字面量，其中**规则体内 40 处 `rgba()`**（不在令牌定义里） |
| 状态覆盖 | `:hover` 24 · `:focus` 6 · `:focus-visible` 2 · **`:active` 0** |
| 状态类名 | 17 种并存：`on`(22 次) / `is-on` / `is-active` / `is-selected` / `is-current` / `is-ok` / `is-err` … |
| 弹窗骨架 | 12 个 `modal-mask` 手写块（9 个业务弹窗 + 引导/提示） |
| 按钮变体 | 5 套且**无基类**：`btn-ghost`24 / `btn-danger`8 / `btn-primary`4 / `wc-btn`3 / `statusbar-clear`2 |
| 浮层令牌 | 18 个 `--hp-*`（自有明暗两套） |
| 窗口 | 3 个页面：`index` / `panel` / `tray`（后两者已 `@import styles.css`，这点做对了，沿用） |
| 材质档位 | 4 档，来自 `effects.rs::Tier`：`system-backdrop` / `mica` / `acrylic` / `solid` |
| 其他缺口 | `::selection` 0 处 · `forced-colors` 0 处 · 滚动条 4 处硬编码 rgba |

---

## 3. 五个结构性病因（都有代码证据）

**A. 浅色主题复用了「白色叠加」**
`--stroke: rgba(255,255,255,.35)`、`--glass: .16`、`--glass-hover: .24`、`--glass-strong: .28`。
白色叠加在深色窗口上天然可见，在浅色窗口（`--material-rgb: 255 255 255`）上等于没有。
后来加的 `--line: rgba(0,0,0,.09)` 是正解，但只有卡片在用（14 次），`--stroke` 仍被 22 处引用 —— 同一件事两套表达。

**B. 令牌按层级不按角色**
「悬浮底色」有四种写法：`--glass-hover`、`--card-hover`、`--glass`、以及 6 处硬编码 `rgba(255,255,255,.05)`。
用户要「统一悬浮底色」，靠逐个改是改不干净的 —— 因为没有一个「角色」可以指。

**C. 状态没有统一入口**
三套按钮各自复制 `height / padding / border-radius / font-family / font-size / cursor`，已经漂移：
`btn-ghost` 30px/12px/12px、`btn-danger` 32px/13px/14px、`btn-primary` 30px/12px/12px。
`input:focus { outline: none }` 之后只给 `button:focus-visible` 补了环，**输入框键盘聚焦完全看不见**，且环色 `rgba(47,107,255,.4)` 写死（= 浅色 `--accent`，暗色主题下不对）。`:active` 全项目 0 处。

**D. 组件靠复制**
12 个 `modal-mask` 各自手写 `modal > title + text + actions`；右键菜单、悬浮浮层、toast 三种浮层各写一套底面/阴影；快捷面板用 `.p-row` 不复用卡片。

**E. 材质 × 主题没有集中消费**
4 档材质只有零散 `body.tier-solid` 之类的 body 类，浅色/深色各写一遍值；没有「某档材质下的 surface/border 该是什么」的统一答案。

---

## 4. 目标架构：三层，禁止跨层

```
L0  ui/tokens.css       角色化语义 + 主题覆盖 + 材质覆盖
L1  ui/*.css, ui/*.js   唯一实现：button / field / modal / toast / popover / tag / card / empty
L2  styles.css          只组合，不写颜色字面量、不写状态
```

**角色命名**（替换现有层级命名）：

```
--surface-base / -raised / -sunken / -overlay / -solid
--border-subtle / -strong / -focus
--state-hover / -active / -selected / -disabled
--text-primary / -secondary / -muted / -on-accent
--accent / -soft / -ring
--track-bg / --scrollbar-thumb / --selection-bg
```

**迁移手法（关键，保证能分批回滚）**：旧名先变别名 —— `--stroke: var(--border-subtle)`，
逐批替换调用点，最后删别名。每批都不中断运行，视觉变化范围可预期。

**材质消费**：`body[data-tier="solid|acrylic|mica|system-backdrop"]` 只覆盖 `--surface-*` / `--border-*`
的 alpha 值。**组件层不出现任何 tier 判断** —— 现在零散写成 `body.tier-solid .xxx` 的规则全部删除。
同理，前端只认 tier，不认「Win10 / Win11」：系统差异已经在 Rust 侧折算成档位了，
前端再加一层版本判断就是第二个真源（`effects.rs` 注释里写过这条教训）。

---

## 5. 逐项确认清单

「用户点名的」+「我扫出来的」，共 28 项。`归属`列即后续唯一实现的位置。

| # | 表面 | 现状 / 问题 | 改法 | 归属 |
|---|---|---|---|---|
| 1 | 弹窗 modal | 12 个骨架手写；标题/正文/按钮间距各写一遍 | `openModal({title, text, actions, tone})` 生成同一骨架，尺寸/阴影/入场动画一处定义 | `ui/modal` |
| 2 | 危险确认 | 靠 `btn-danger` 颜色表达严重性，文案与按钮无联动 | modal 增 `tone: default \| danger`，由它决定确认键变体 | `ui/modal` |
| 3 | Toast | `main.js` 内单例，只有 `.show` 一个状态 | 提为模块，info/warn/error 三态 + 停留时长 + 队列（不叠三条） | `ui/toast` |
| 4 | 气泡（悬浮预览） | 18 个 `--hp-*`，自有渐变/描边/rim/阴影 | 与右键菜单共用 popover 底面；**渐变保留在内容层**（那是它的识别特征） | `ui/popover` |
| 5 | 右键菜单 | JS 拼 class，样式在 `styles.css` 尾部；项只有 hover | 并入 popover；项补齐 hover/active/disabled/危险色 | `ui/popover` |
| 6 | 材质 4 档 | 零散 body 类，浅深各写一遍 | `body[data-tier]` 只覆盖 surface/border alpha | `tokens` |
| 7 | Win10 / Win11 | 差异在 Rust 侧，前端无感知（**正确**） | 保持：前端只认 tier；Win10 无模糊时靠 solid 的不透明度补偿 | `tokens` |
| 8 | 开关 | 原生 `checkbox` 16px + `accent-color`，Win10/11 渲染不一致，无 disabled/focus | 自绘 switch（36×20，轨道/滑块/焦点环/禁用） | `ui/field` |
| 9 | 复选 / 单选 | 与开关混用同一原生控件 | 自绘 checkbox（14px 勾）与 radio（14px 点） | `ui/field` |
| 10 | 滑块 range | 只覆盖了轨道与滑块颜色 | 统一 thumb/track/focus/disabled | `ui/field` |
| 11 | 选中底色 | 卡片 `.is-selected` 用 `--select` 淡金 + inset 描边 | 改 `--state-selected`，保留 inset（外扩会被滚动容器裁掉，这条已有注释，别动） | `ui/card` |
| 12 | 悬浮底色 | `--card-hover` / `--glass-hover` / 硬编码三种并存 | 统一 `--state-hover` | `tokens` |
| 13 | 聚焦底色 | 只覆盖 button；输入框无；环色写死 | `--border-focus` + `:focus-visible` 全局兜底（button/a/input/select/textarea/[tabindex]） | `tokens` + primitives |
| 14 | 按下态 | 全项目 0 处 | 每个 primitive 一条 `--state-active` | primitives |
| 15 | 禁用态 | 6 处零散 override | `--state-disabled` + 统一 opacity / pointer-events | primitives |
| 16 | 标签徽章 tag | 14 种语义色（fg/bg 成对），生成函数在 `main.js` 里 | 保留语义色，补 size/shape；生成函数收进 `ui/tag` | `ui/tag` |
| 17 | 快捷面板 | 独立窗口，自有 `.p-row`，不复用卡片 | 复用 `ui/card` 的 compact 变体 | `ui/card` |
| 18 | 托盘菜单 | 已 `@import` 主样式（做对了） | 只补状态与焦点 | `tokens` |
| 19 | 更新横幅 / 引导 tour | tour 12 个步骤块，深浅两套规则 | 复用 popover/modal 基底，只保留步骤排版 | `ui/popover` |
| 20 | 空态 / 骨架行 | 空态文案散在各视图；骨架 `.vl-skeleton-bar` 单独实现 | 统一空态组件；骨架高度与卡片类高一致（已修） | `ui/empty` |
| 21 | 滚动条 | 4 处硬编码 rgba；只有 `body.dark` 覆盖 thumb | `--scrollbar-thumb[-hover]` | `tokens` |
| 22 | 标题栏 / 窗口按钮 / 拖拽把手 | `wc-btn` 3 个、`.rz` 9 个，无按下态 | 共用 button primitive，补 hover/active/close 危险色 | `ui/button` |
| 23 | 搜索框 | `::placeholder` 只有 1 条；清除按钮自带样式 | 输入框基类（icon / 清除 / focus 环） | `ui/field` |
| 24 | 下载 / 更新进度 | `.dl-fill` 单独实现 | 与 range 轨道共用 `--track-bg` | `ui/field` |
| 25 | 文本选区 | `::selection` 0 处，走浏览器默认蓝 | `--selection-bg/-fg` | `tokens` |
| 26 | 键盘可达 | Tab 顺序无定义；Esc 关弹窗散落各处；无焦点陷阱 | modal 内置焦点陷阱 + Esc；Esc 收敛到一处 | `ui/modal` |
| 27 | 高对比度 | `forced-colors` 0 处 | 补 `@media (forced-colors: active)` 兜底边框 | `tokens` |
| 28 | DPI 125% / 150% | `--radius` 固定 px，缩放后观感偏小 | 半径/间距走 4px 栅格 | `tokens` |

---

## 6. 材质 × 主题 × 系统矩阵

| 系统 | 可用档位 | 前端表现 |
|---|---|---|
| Win11 | `system-backdrop` / `mica` / `acrylic` / `solid` | 真模糊，surface alpha 可低 |
| Win10 | `acrylic`（tint 压 8%）/ `solid` | 无真模糊，surface alpha 需抬高补偿 |

**每个组合下必须成立的三条不变量**：

1. 正文对比度 ≥ 4.5:1、次要文字 ≥ 3:1（无论背景是桌面、文档还是纯白）；
2. 卡片/弹窗边界必须可见 —— 靠 `--border-subtle` 中性线，不靠阴影；
3. 悬浮/选中/聚焦三态在同一元素上叠加时，优先级 聚焦 > 选中 > 悬浮，且互不吞没。

---

## 7. 分批落地（每批一支、可单测、可整体 revert）

| 批 | 分支 | 内容 | 验收 |
|---|---|---|---|
| 0 | `chore/ui-audit` | 审计脚本：裸色值、裸 transition、状态类名白名单、重复 modal 骨架；`CONTRIBUTING` 增一条检查项 | 脚本可跑，输出基线数字 |
| 1 | `refactor/ui-tokens` | L0 角色令牌 + 浅色中性化 + 旧名别名 | 明暗对比截图，**只有颜色变化，无布局变化** |
| 2 | `refactor/ui-fields` | button / field（开关、复选、单选、滑块、输入框）/ tag | 设置页全控件 + 弹窗按钮 + 批量条 |
| 3 | `refactor/ui-overlay` | modal / toast / popover 三件套；12 个骨架收敛为 `openModal` | 全部弹窗 + 右键菜单 + 浮层 + toast |
| 4 | `refactor/ui-pages` | 卡片 / 快捷面板 / 托盘 / 空态 / 滚动条 / 标题栏收口 | 列表、面板、托盘三窗口 |
| 5 | `fix/ui-matrix` | 4 材质 × 2 主题 × Win10/11 × 125%/150% 校准 | §8 验收清单全过 |

批 1 是纯值替换（不动选择器），风险最低但观感变化最明显；批 2/3 是新增目录、删旧规则，回滚就是删目录 + `git revert`。

---

## 8. 验收清单

- [ ] 浅色下逐项可见：卡片描边、列表分隔线、状态栏分隔、hover、输入框边框、开关轨道、滚动条、右键菜单、空态
- [ ] 键盘 Tab 走一圈：每个可交互元素都出现焦点环，明暗两套颜色都对
- [ ] 4 档材质 × 明暗：正文 ≥4.5:1、次要 ≥3:1（DevTools 取色）
- [ ] `grep` 复核：色值字面量只出现在 `tokens.css`；裸 `transition` 0 处；`:active` 覆盖所有可点元素
- [ ] Win11（办公室）与 Win10（家里）各跑一遍：solid 档文字不吃背景
- [ ] 125% / 150% 缩放：卡片不截断、弹窗不超屏、焦点环不被裁
- [ ] 系统「减少动态效果」「高对比度」两个开关下不破版

---

## 9. 不做 / 已知风险

**不做**：

- 不改信息架构与布局，不重排侧栏/工具栏/状态栏；
- 不引第三方 UI 库（打包体积 + 与自绘窗口材质冲突 + 又一套设计语言）；
- 不改 Rust 侧材质算法（`Tier` 已稳定），只改前端消费方式；
- 不做「整窗换皮」式重写 —— 现有 HTML 结构与 id 全部保留，改造只发生在样式与生成函数。

**风险**：

- 批 1 会让浅色「突然有边界」，看起来像改了设计 —— 那是修好了，不是坏；
- 自绘开关/复选会改变点击热区（16px → 36×20），需要手测误触；
- 浮层统一底面可能让悬浮预览失去辨识度 → 渐变保留在内容层（已在清单里注明）；
- 一次改 28 项必然有遗漏，所以批 0 的审计脚本必须先进主干，后续每批都跑。

---

## 10. 「不重复造轮子」的机制（比方案本身更重要）

方案做完会再次腐化，除非把约束固化下来：

1. **一个视觉决定只允许一处实现。** 新增控件先查清单（§5 的「归属」列即组件表），没有才新建；
   新建必须同时补进这张表，否则下一批又会复制一遍。
2. **页面层禁止写颜色字面量与状态。** 审计脚本拦截，数字不得比基线上升。
3. **状态类名白名单**：`is-*` 只允许登记过的语义（selected / pinned / missing / disabled / active…），
   不再新增 `on` 这类无前缀短名。
4. **改行为/改视觉必须同一次提交改注释**（`CONTRIBUTING` §11 已定），这条对样式同样适用。
