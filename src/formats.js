// 格式渲染：卡片「轻量着色」与详情「完整渲染」共用一套实现。
//
// 为什么要一个独立模块：卡片预看着色、详情 Markdown 排版、代码高亮、富文本白名单
// 都属于「把用户复制来的文本按格式呈现」，放两处必然漂移（一处改了另一处忘）。
//
// 安全底线（这个文件里的每个函数都必须遵守）：
//   1. **所有来自用户内容的文本，一律先转义再加标签**；标签只由本文件生成。
//      绝不把用户文本原样拼进 innerHTML。
//   2. 不产出 `<script>` / 事件属性（onerror 等）—— 即使 CSP 已经禁了内联脚本，
//      也不依赖它兜底。
//   3. 富文本走白名单：标签、属性、样式属性都按允许列表过滤，其余丢弃。

/// HTML 转义。main.js 的关于页也在用同一份语义，统一从这里走。
export function esc(s) {
  return String(s ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
  );
}

// ── 代码高亮 ──────────────────────────────────────────────────────────────
// 刻意只做**四类** token：注释、字符串、数字、关键字。
// 更细的着色（函数名/类型/变量）需要真正的词法分析，收益远不抵复杂度；
// 而 2 行预览里能一眼分辨出「这是代码」靠的就是这四类。
const KEYWORDS = [
  // 通用
  "fn", "let", "mut", "const", "var", "function", "def", "class", "struct", "impl", "trait",
  "enum", "pub", "use", "mod", "import", "from", "export", "default", "return", "yield",
  "if", "else", "elif", "for", "while", "loop", "match", "switch", "case", "break",
  "continue", "new", "try", "catch", "except", "finally", "throw", "raise", "async", "await",
  "public", "private", "protected", "static", "final", "abstract", "interface", "extends",
  "void", "int", "float", "double", "char", "bool", "boolean", "string", "str", "bytes",
  "true", "false", "null", "nil", "None", "True", "False", "undefined", "this", "self",
  "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE", "CREATE", "TABLE", "JOIN", "GROUP",
];

const CODE_RE = new RegExp(
  [
    // 1) 注释：// 行注释、/* 块 */、# 行注释、-- 行注释（SQL/Lua）
    "(\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/|#[^\\n]*|--[^\\n]*)",
    // 2) 字符串：单/双/反引号（不跨行）
    "(\"(?:[^\"\\\\\\n]|\\\\.)*\"|'(?:[^'\\\\\\n]|\\\\.)*'|`(?:[^`\\\\]|\\\\.)*`)",
    // 3) 数字
    "\\b(0[xX][0-9a-fA-F]+|\\d+(?:\\.\\d+)?)\\b",
    // 4) 关键字（整词边界）
    "\\b(" + KEYWORDS.join("|") + ")\\b",
  ].join("|"),
  "g"
);

/// 代码着色：返回已转义的 HTML 片段。
export function highlightCode(text) {
  const src = String(text ?? "");
  let out = "";
  let last = 0;
  for (const m of src.matchAll(CODE_RE)) {
    out += esc(src.slice(last, m.index));
    const cls = m[1] ? "tok-cmt" : m[2] ? "tok-str" : m[3] ? "tok-num" : "tok-kw";
    out += `<span class="${cls}">${esc(m[0])}</span>`;
    last = m.index + m[0].length;
  }
  return out + esc(src.slice(last));
}

// ── Markdown 着色（卡片用，只染色不排版）────────────────────────────────
// 卡片只有两行高，排版成标题/列表反而看不清结构；这里只把标记符号染成次要色，
// 让人一眼认出「这是 Markdown 源码」。
export function markdownTint(text) {
  return String(text ?? "")
    .split("\n")
    .map((line) => {
      const m = line.match(/^(\s*)(#{1,6}\s|[-*+]\s|\d+\.\s|>\s|\|\s*)/);
      if (!m) return esc(line);
      const head = m[0];
      return `<span class="tok-md">${esc(head)}</span>${esc(line.slice(head.length))}`;
    })
    .join("\n");
}

// ── 卡片预览（入口）──────────────────────────────────────────────────────
/// 按子类型返回卡片正文的 HTML。调用方直接赋给 innerHTML 即可（内容已全部转义）。
export function cardPreviewHtml(text, subtype) {
  if (subtype === "code") return highlightCode(text);
  if (subtype === "markdown") return markdownTint(text);
  return esc(text);
}

// ── 链接 ──────────────────────────────────────────────────────────────────
/// 从文本里取出可展示的域名（`github.com/k/xxx` 或 `https://x.com/y` 都行）。
/// 失败返回 null —— 调用方回退到普通文本，不因为解析失败就什么都不显示。
export function hostOf(text) {
  const t = String(text ?? "").trim();
  if (!t || /\s/.test(t)) return null; // 含空白 = 句子里夹着东西，不是纯链接
  try {
    const u = new URL(/^[a-z]+:\/\//i.test(t) ? t : "https://" + t);
    const host = u.hostname.replace(/^www\./i, "");
    // 必须像个域名。URL 解析很宽容 —— `https://这是一段说明` 也能解析成功
    // （IDN 规则），不校验的话中文正文会被当成域名。
    if (!host.includes(".") || !/^[a-z0-9.-]+$/i.test(host)) return null;
    return host;
  } catch {
    return null;
  }
}
