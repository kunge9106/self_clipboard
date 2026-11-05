#!/usr/bin/env node
// UI 审计脚本（期 0）：把「改之前长什么样」变成可复核的数字。
//
// 为什么要有它：整批改造的验收标准是「人眼看得见了」，但人眼没法证明「没有顺带改坏别的」。
// 这份脚本负责后者 —— 六类指标，每类都有一个基线数字，后续每期跑一次对比，
// 数字只能降不能升（唯一例外是期 1.5，它按设计会改变尺度相关的计数）。
//
// 用法：
//   node scripts/ui-audit.mjs            # 打印到 stdout
//   node scripts/ui-audit.mjs --write    # 写入 docs/ui-audit-<日期>.md
//
// 没有依赖：不引 postcss / puppeteer。样式表是本项目自己的，格式稳定，
// 一层大括号配对足够；引解析器反而多一个需要维护的依赖（原则见方案 §3）。

import fs from "node:fs";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "..");
const read = (p) => fs.readFileSync(path.join(ROOT, p), "utf8");

const css = read("src/styles.css");
const html = read("index.html");
const js = read("src/main.js");

// ── CSS 结构：先切出四个令牌块，后面所有统计都要排除它们 ─────────────────
const TOKEN_BLOCKS = [":root {", "body.dark {", "body.tier-solid {", "body.dark.tier-solid {"];

const lines = css.split("\n");
// 去注释后的代码行：注释里的 `rgba(...)` 不算硬编码，行内注释也不该混进选择器。
// 多行注释用状态位跨行吞掉（本项目大量使用块注释说明取舍）。
const codeLines = (() => {
  let inBlock = false;
  return lines.map((l) => {
    let out = "";
    let i = 0;
    while (i < l.length) {
      if (inBlock) {
        const end = l.indexOf("*/", i);
        if (end < 0) return out;
        inBlock = false;
        i = end + 2;
      } else {
        const start = l.indexOf("/*", i);
        if (start < 0) { out += l.slice(i); break; }
        out += l.slice(i, start);
        inBlock = true;
        i = start + 2;
      }
    }
    return out;
  });
})();

const blockRanges = []; // [{ start, end }]（1-based 行号，闭区间）
for (let i = 0; i < lines.length; i++) {
  if (!TOKEN_BLOCKS.includes(lines[i].trim())) continue;
  let depth = 0;
  for (let j = i; j < lines.length; j++) {
    depth += (lines[j].match(/\{/g) || []).length - (lines[j].match(/\}/g) || []).length;
    if (depth === 0 && j > i) {
      blockRanges.push({ start: i + 1, end: j + 1 });
      break;
    }
  }
}
const inTokenBlock = (ln) => blockRanges.some((r) => ln >= r.start && ln <= r.end);

/** 取某个令牌块里的 `--name: value` 映射。 */
function tokens(selector) {
  const out = new Map();
  const start = lines.findIndex((l) => l.trim() === selector);
  if (start < 0) return out;
  for (let i = start; i < lines.length; i++) {
    const m = lines[i].match(/^\s*(--[a-z0-9-]+)\s*:\s*(.+?);\s*(?:\/\*.*)?$/);
    if (m) out.set(m[1], m[2].trim());
    if (i > start && lines[i].trim() === "}") break;
  }
  return out;
}

const light = tokens(":root {");
const dark = tokens("body.dark {");

// ── 材质档 ────────────────────────────────────────────────────────────
// Windows 上有四档：system-backdrop / mica / acrylic（三者在前端共用同一套令牌）
// 与 solid（兜底：关闭透明效果、节能模式、低端机、Win10 无 acrylic）。
// **solid 档单独覆盖了一批令牌**（面变实、材质色变实），只审 :root 等于漏掉一整档 ——
// 用户在「关闭透明效果」的机器上看到的就是这一档，而它从来没被测过。
const solidLight = tokens("body.tier-solid {");
const solidDark = tokens("body.dark.tier-solid {");
const merge = (base, over) => {
  const m = new Map(base);
  for (const [k, v] of over) m.set(k, v);
  return m;
};
/** [标签, 变量表] —— 四个组合都要过同一套门槛 */
const themeSets = [
  ["浅色·材质", light],
  ["深色·材质", dark],
  ["浅色·实底", merge(light, solidLight)],
  ["深色·实底", merge(dark, solidDark)],
];

// ── 颜色解析与合成（对比度要算「叠完之后」的颜色，不能只看字面色）──────
function parseColor(v, vars, depth = 0) {
  if (!v || depth > 4) return null;
  v = v.trim();
  const varMatch = v.match(/^var\(\s*(--[a-z0-9-]+)\s*(?:,\s*(.+))?\)$/);
  if (varMatch) {
    const got = vars.get(varMatch[1]);
    if (got) return parseColor(got, vars, depth + 1);
    return varMatch[2] ? parseColor(varMatch[2], vars, depth + 1) : null;
  }
  let m = v.match(/^#([0-9a-f]{3,8})$/i);
  if (m) {
    let h = m[1];
    if (h.length === 3) h = h.split("").map((c) => c + c).join("");
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    const a = h.length === 8 ? parseInt(h.slice(6, 8), 16) / 255 : 1;
    return [r, g, b, a];
  }
  m = v.match(/^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)(?:[\s,/]+([\d.]+))?\s*\)$/i);
  if (m) return [+m[1], +m[2], +m[3], m[4] === undefined ? 1 : +m[4]];
  return null;
}

/** 把 fg（可带 alpha）叠在 bg（不透明）上，返回不透明色。 */
function over(fg, bg) {
  const a = fg[3];
  return [0, 1, 2].map((i) => Math.round(fg[i] * a + bg[i] * (1 - a))).concat(1);
}

const toHex = (c) =>
  "#" + c.slice(0, 3).map((n) => Math.max(0, Math.min(255, n)).toString(16).padStart(2, "0")).join("");

function luminance(c) {
  const s = c.slice(0, 3).map((v) => {
    const x = v / 255;
    return x <= 0.03928 ? x / 12.92 : Math.pow((x + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * s[0] + 0.7152 * s[1] + 0.0722 * s[2];
}
function contrast(a, b) {
  const l1 = luminance(a);
  const l2 = luminance(b);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
}

/**
 * 逐层合成出四个面：
 *   base  窗口底   = 材质色按 glass-alpha 叠在壁纸上
 *   content 内容区 = surface-1 叠在 base 上（列表/设置页的底色）
 *   card  卡片     = surface-2 叠在 content 上
 *   overlay 浮层   = surface-3 / solid-bg（用于文字对比度）
 *
 * 之所以要分四层：**「底色与背景太相同」这个反馈，本质是缺了 content 这一层。**
 * 只有 base 和 card 时，浅色下两者都是近白，差值靠 alpha 挤不出来。
 */
function surfaces(vars, wallpaper) {
  const rgb = (vars.get("--material-rgb") || "255 255 255").split(/\s+/).map(Number);
  const alpha = parseFloat(vars.get("--glass-alpha") || "1");
  const win = over([rgb[0], rgb[1], rgb[2], alpha], wallpaper);
  const layer = (key, base, fallback) => over(parseColor(vars.get(key), vars) || fallback, base);
  const content = layer("--surface-1", win, [0, 0, 0, 0]);
  const card = layer("--surface-2", content, [255, 255, 255, 1]);
  const overlay = layer("--surface-3", win, [255, 255, 255, 1]);
  return { win, content, card, overlay };
}

// ── 1. 规则体内色值（基线 66）────────────────────────────────────────────
const COLOR_RE = /#[0-9a-fA-F]{3,8}\b|rgba?\([^)]*\)/g;

// 「行号 → 所属选择器」：扫描大括号配对，遇到以 { 结尾的行就把累积的片段当选择器。
// 不能靠「向上找最近含 { 的行」—— 多行选择器（逗号分组）会取到半截。
const selAt = new Map();
{
  let buf = [];
  let cur = "";
  for (let i = 0; i < lines.length; i++) {
    const t = codeLines[i].trim();
    if (!t) { buf = []; continue; }
    if (t.endsWith("{")) {
      buf.push(t.slice(0, -1).trim());
      cur = buf.filter(Boolean).join(" ").replace(/\s+/g, " ").trim();
      buf = [];
      selAt.set(i + 1, cur);
    } else if (t === "}") {
      cur = "";
      buf = [];
    } else if (t.endsWith(";")) {
      if (cur) selAt.set(i + 1, cur);
    } else if (!t.startsWith("--")) {
      buf.push(t);
    }
  }
  // 未闭合块（@media / 嵌套）内的声明沿用外层选择器
  let last = "";
  for (let i = 0; i < lines.length; i++) {
    const ln = i + 1;
    if (selAt.has(ln)) last = selAt.get(ln);
    else if (lines[i].trim() === "}") last = "";
    else if (!selAt.has(ln)) selAt.set(ln, last);
  }
}

const ruleColors = [];
let colorOccurrences = 0;
codeLines.forEach((l, i) => {
  const ln = i + 1;
  if (inTokenBlock(ln)) return;
  // `rgb(var(--material-rgb) / calc(...))` 是令牌组合，不是硬编码 —— 先剔掉再匹配
  const cleaned = l.replace(/rgba?\(\s*var\([^)]*\)[^;]*?\)/g, "");
  const hits = cleaned.match(COLOR_RE);
  if (!hits) return;
  colorOccurrences += hits.length;
  ruleColors.push({ ln, value: hits.join(" / "), sel: selAt.get(ln) || "", n: hits.length });
});

// ── 2. 状态覆盖 ──────────────────────────────────────────────────────────
const stateOf = (p) => (css.match(new RegExp(p, "g")) || []).length;
const states = {
  hover: stateOf(/html:not\(\.hovers-off\)[^{]*:hover|:hover/g),
  focus: stateOf(/:focus(?!-visible)/g),
  "focus-visible": stateOf(/:focus-visible/g),
  active: stateOf(/:active/g),
  disabled: stateOf(/disabled/g),
};

// ── 3. 对比度矩阵（最坏情况壁纸：纯白 / 纯黑）──────────────────────────
const WHITE = [255, 255, 255, 1];
const BLACK = [0, 0, 0, 1];
function contrastRows(themeName, vars) {
  const rows = [];
  for (const [wallName, wall] of [["白壁纸", WHITE], ["黑壁纸", BLACK]]) {
    const { card } = surfaces(vars, wall);
    // 门槛按角色分档，不能一刀 3:1：
    //   · 文字用 WCAG AA（正文 4.5 / 次要 3.0）；
    //   · **描边与分隔线不是「控件」**，WCAG 的 3:1 只约束「识别控件所必需」的边界。
    //     给卡片画 3:1 的边等于画一圈深灰框，观感直接崩。这里用自定的「可辨底线 1.5」，
    //     含义是「肉眼能看出这里是边界」，配合阴影使用。
    //     —— 这条是期 0 跑完第一版报告后修的：第一版对描边也用 3:1，结论会变成
    //     「必须把边加深到黑 .42」，那是错的。
    for (const [label, key, min] of [
      ["正文", "--text", 4.5],
      ["次要文字", "--text-dim", 3.0],
      // 控件边框是**承担信息**的线（「这里能输入」），按 WCAG 非文本 3:1 的一半取 1.5：
      // 它不需要像图标那样断言「这是个控件」，只需明显强于装饰线。
      ["控件边框", "--border-control", 1.5],
      // 层级线已经不是层级表达手段了（层级交给 surface + elev），只要求「能感知」。
      ["层级线", "--stroke", 1.15],
    ]) {
      const fg = parseColor(vars.get(key), vars);
      if (!fg) continue;
      const composed = over(fg, card);
      const ratio = contrast(composed, card);
      rows.push({
        theme: themeName,
        wall: wallName,
        label,
        token: key,
        color: toHex(composed),
        ratio: ratio.toFixed(2),
        pass: ratio >= min,
        min,
      });
    }
  }
  return rows;
}
const contrastRowsAll = themeSets.flatMap(([name, vars]) => contrastRows(name, vars));

// ── 3b. 面层级差（这才是「看不清」的第一性指标）──────────────────────
// 对比度回答「文字/线条能不能读」；层级差回答「两块面能不能区分」。
// 用户反馈「底色与背景太相同」属于后者 —— 只盯对比度会得出「全部达标」的错误结论。
const surfaceRowsAll = [];
for (const [themeName, vars] of themeSets) {
  for (const [wallName, wall] of [["白壁纸", WHITE], ["黑壁纸", BLACK]]) {
    const { win, content, card } = surfaces(vars, wall);
    for (const [label, a, b] of [
      ["窗口底 → 内容区", win, content],
      ["内容区 → 卡片", content, card],
    ]) {
      const ratio = contrast(a, b);
      // 深色只有三阶（窗口底与内容区同层）：再分一层会变成「陷进去」的错觉，
      // 而深色要区分的本来就是「卡片 vs 背景」。这条不是放过，是设计决策，写进报告可见。
      // 内容区与窗口底同层（浅色深色都是）：毛玻璃上再压一层只会发灰，
      // 真正的层级只有「底 → 卡片」两阶。这是设计决策，不是放过。
      const na = label.startsWith("窗口底");
      surfaceRowsAll.push({
        theme: themeName,
        wall: wallName,
        label,
        from: toHex(a),
        to: toHex(b),
        ratio: ratio.toFixed(2),
        na,
        pass: na || ratio >= 1.06,
      });
    }
  }
}

// ── 4. body.dark 组件级覆盖（基线 9）────────────────────────────────────
const darkOverrides = lines
  .map((l, i) => ({ ln: i + 1, t: l.trim() }))
  .filter(({ ln, t }) => t.startsWith("body.dark ") && !inTokenBlock(ln))
  .map(({ ln, t }) => ({ ln, sel: t.replace(/\s*\{$/, "") }));

// ── 5. 双表达：同一选择器同时写 border 与 box-shadow ────────────────────
const dual = [];
{
  let sel = null;
  let hasBorder = false;
  let hasShadow = false;
  let startLn = 0;
  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    if (!sel && t.endsWith("{")) {
      sel = t.replace(/\{$/, "").trim();
      hasBorder = hasShadow = false;
      startLn = i + 1;
    } else if (sel && t === "}") {
      if (hasBorder && hasShadow) dual.push({ ln: startLn, sel });
      sel = null;
    } else if (sel) {
      // transparent 边框是「占位」：状态色（置顶/选中）要复用它，
      // 直接加 border 会让卡片尺寸跳 2px。它不构成视觉表达，不算双表达。
      // 只认真正的边框声明：`border-radius` 不是边框线，
      // 原正则 `border(-[a-z]+)?` 会把 border-radius 也算进来（虚报了若干处）。
      if (/^border(-(top|right|bottom|left|width|style|color))?\s*:/.test(t) && !/transparent/.test(t))
        hasBorder = true;
      if (/^box-shadow\s*:/.test(t)) hasShadow = true;
    }
  }
}

// ── 6. 文案基线（只出数字，不做门禁，见方案 §12.3 P6）──────────────────
const texts = html
  .split(/<[^>]+>/)
  .map((t) => t.trim())
  .filter((t) => /[\u4e00-\u9fa5]/.test(t));
const attrTexts = (html.match(/(?:title|placeholder|aria-label)="([^"]*[\u4e00-\u9fa5][^"]*)"/g) || []).length;
const toastCalls = (js.match(/toast\(/g) || []).length;
const terms = {};
for (const w of ["条目", "记录", "内容", "历史", "删除", "移除", "清除", "清理", "清空"]) {
  terms[w] = (html.match(new RegExp(w, "g")) || []).length;
}

// ── 输出 ────────────────────────────────────────────────────────────────
const today = new Date().toISOString().slice(0, 10);
const md = `# UI 审计基线（${today}）

> 由 \`scripts/ui-audit.mjs\` 生成，请勿手改。基线提交：\`${process.env.GIT_HEAD || "main"}\`。
> 用法：\`node scripts/ui-audit.mjs --write\`。后续每期跑一次，**数字只能降不能升**
> （期 1.5 例外：它按设计会改变尺度相关计数，跑完记得更新基线并说明原因）。

## 1. 规则体内色值 —— 基线 ${colorOccurrences} 处（分布在 ${ruleColors.length} 行）

口径：排除 4 个令牌块（\`:root\` / \`body.dark\` / \`body.tier-solid\` / \`body.dark.tier-solid\`）。
出现在规则体内的色值都属于「改主题会漏」的隐患。

| # | 行 | 选择器 | 值 |
|---|---|---|---|
${ruleColors.map((r, i) => `| ${i + 1} | ${r.ln} | \`${r.sel}\` | \`${r.value}\` |`).join("\n")}

## 2. 状态覆盖

| 状态 | 出现次数 |
|---|---|
${Object.entries(states).map(([k, v]) => `| :${k} | ${v} |`).join("\n")}

## 3. 值级对比度（WCAG）

按「材质 → 卡片 → 文字/描边」逐层合成后计算，不是比较字面颜色。
壁纸取最坏情况（纯白 / 纯黑）—— 浅色主题在深色壁纸上会不会糊，就看这张表。

门槛说明：文字按 WCAG AA；**描边与分隔线用自定的「可辨底线 1.5」**，不套 WCAG 的 3:1 —— 那条针对的是「识别控件所必需的边界」，把它套到卡片描边上会得出「边要加深到黑 .42」这种毁观感的结论。

| 主题 | 壁纸 | 项 | 令牌 | 合成后 | 对比度 | 要求 | 结果 |
|---|---|---|---|---|---|---|---|
${contrastRowsAll.map((r) => `| ${r.theme} | ${r.wall} | ${r.label} | \`${r.token}\` | \`${r.color}\` | ${r.ratio}:1 | ≥${r.min} | ${r.pass ? "✅" : "❌"} |`).join("\n")}

## 3b. 面层级差（「两块面分不分得出来」）

对比度回答「文字能不能读」，**层级差回答「哪块是哪块」**。
用户反馈的「底色与背景太相同」属于后者 —— 只盯对比度会得出「全部达标」的错误结论。
门槛 1.06 ≈ 亮度差 3%，是 Linear / Notion / Fluent 共用的「whisper-quiet」档位：
再低两块面就真的并成一片，再高就开始显得厚重。

| 主题 | 壁纸 | 相邻两层 | 下 → 上 | 比值 | 结果 |
|---|---|---|---|---|---|
${surfaceRowsAll.map((r) => `| ${r.theme} | ${r.wall} | ${r.label} | \`${r.from}\` → \`${r.to}\` | ${r.ratio}:1 | ${r.na ? "— 同层" : r.pass ? "✅" : "❌"} |`).join("\n")}

## 4. body.dark 组件级覆盖 —— 基线 ${darkOverrides.length}

约定是「颜色一律走变量」，以下属例外，改造后应收敛到装饰性白名单。

${darkOverrides.map((r) => `- \`${r.ln}\` \`${r.sel}\``).join("\n")}

## 5. 双表达（同选择器同时用 border 与 box-shadow）—— ${dual.length} 处

方案 §5.1 第 1 条要求「同一元素不得同时用描边与阴影表达层级」，这里是回归网。

${dual.map((r) => `- \`${r.ln}\` \`${r.sel}\``).join("\n")}

## 6. 文案基线（只记录，不做门禁）

| 项 | 数量 |
|---|---|
| HTML 中文文本节点 | ${texts.length} |
| 属性内文案（title/placeholder/aria-label） | ${attrTexts} |
| \`toast(...)\` 调用点 | ${toastCalls} |
| 超过 24 字的文本节点 | ${texts.filter((t) => t.length > 24).length} |

术语计数（同一概念多个词 = 改造要统一的量）：

| 词 | 出现次数 |
|---|---|
${Object.entries(terms).map(([k, v]) => `| ${k} | ${v} |`).join("\n")}
`;

if (process.argv.includes("--write")) {
  // 支持 --label：每期各存一份，否则新报告会把基线覆盖掉（踩过一次）。
  const li = process.argv.indexOf("--label");
  const label = li > 0 && process.argv[li + 1] ? `-${process.argv[li + 1]}` : "";
  const out = path.join(ROOT, "docs", `ui-audit${label}-${today}.md`);
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.writeFileSync(out, md);
  console.log(`已写入 ${path.relative(ROOT, out)}`);
  console.log(
    `规则体内色值 ${colorOccurrences} 处 · dark 覆盖 ${darkOverrides.length} · 双表达 ${dual.length} · 对比度不达标 ${contrastRowsAll.filter((r) => !r.pass).length}`
  );
} else {
  process.stdout.write(md);
}
