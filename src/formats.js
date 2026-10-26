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

// ── Markdown 排版（详情用）──────────────────────────────────────────────
// 自写子集，覆盖常用语法：标题、无序/有序列表、引用、代码块、表格、水平线；
// 行内支持 **粗体**、*斜体*、`代码`、[文字](链接)。
//
// 实现取向：**逐行解析块级 → 行内替换**，每一步的输出都基于 esc() 的结果，
// 所以用户内容里的 `<` 永远是实体，不会被当成标签。这一点比「支持多少语法」重要。
const MD_BLOCK_START = /^(#{1,6}\s|\s*[-*+]\s|\s*\d+\.\s|\s*>|\s*\||\s*```)/;

/// 行内格式。入参是**原始文本**，内部先转义再替换标记。
function inline(text) {
  let h = esc(text);
  // 行内代码优先：里面的 `*` `[` 不该再被解析
  h = h.replace(/`([^`\n]+)`/g, (_, c) => `<code class="md-inline">${c}</code>`);
  h = h.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  h = h.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  h = h.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (whole, label, href) => {
    // 只放行 http(s)：`javascript:` / `data:` 一律退化成纯文字
    if (!/^https?:\/\//i.test(href)) return label;
    return `<a class="md-link" href="#" data-ext="${href}">${label}</a>`;
  });
  return h;
}

export function renderMarkdown(text) {
  const lines = String(text ?? "").replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  let i = 0;
  let list = null; // "ul" | "ol"
  const closeList = () => {
    if (list) {
      out.push(`</${list}>`);
      list = null;
    }
  };

  while (i < lines.length) {
    const line = lines[i];

    // 代码块：围栏内**原样**保留（只做高亮，不解析其它语法）
    const fence = line.match(/^\s*```\s*([\w+-]*)\s*$/);
    if (fence) {
      closeList();
      const buf = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) {
        buf.push(lines[i]);
        i++;
      }
      i++; // 吃掉结束围栏
      out.push(`<pre class="md-code"><code>${highlightCode(buf.join("\n"))}</code></pre>`);
      continue;
    }

    // 标题
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      closeList();
      const lv = Math.min(h[1].length, 6);
      out.push(`<h${lv}>${inline(h[2])}</h${lv}>`);
      i++;
      continue;
    }

    // 水平线
    if (/^\s*([-*_])\1{2,}\s*$/.test(line)) {
      closeList();
      out.push("<hr>");
      i++;
      continue;
    }

    // 引用
    if (/^\s*>\s?/.test(line)) {
      closeList();
      const buf = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^\s*>\s?/, ""));
        i++;
      }
      out.push(`<blockquote>${inline(buf.join("\n")).replace(/\n/g, "<br>")}</blockquote>`);
      continue;
    }

    // 表格：首行 + 分隔行（|---|:--:|）
    if (/^\s*\|/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      closeList();
      const split = (row) =>
        row.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = split(line);
      i += 2;
      const rows = [];
      while (i < lines.length && /^\s*\|/.test(lines[i])) {
        rows.push(split(lines[i]));
        i++;
      }
      out.push(
        `<table class="md-table"><thead><tr>${head
          .map((c) => `<th>${inline(c)}</th>`)
          .join("")}</tr></thead><tbody>${rows
          .map((r) => `<tr>${r.map((c) => `<td>${inline(c)}</td>`).join("")}</tr>`)
          .join("")}</tbody></table>`
      );
      continue;
    }

    // 列表
    const ul = line.match(/^\s*[-*+]\s+(.*)$/);
    const ol = line.match(/^\s*\d+\.\s+(.*)$/);
    if (ul || ol) {
      const want = ul ? "ul" : "ol";
      if (list !== want) {
        closeList();
        out.push(`<${want}>`);
        list = want;
      }
      out.push(`<li>${inline((ul || ol)[1])}</li>`);
      i++;
      continue;
    }

    // 空行：结束当前列表/段落
    if (!line.trim()) {
      closeList();
      i++;
      continue;
    }

    // 段落：连续非空、且不是其它块的起始行
    closeList();
    const buf = [line];
    i++;
    while (i < lines.length && lines[i].trim() && !MD_BLOCK_START.test(lines[i])) {
      buf.push(lines[i]);
      i++;
    }
    out.push(`<p>${inline(buf.join("\n")).replace(/\n/g, "<br>")}</p>`);
  }

  closeList();
  return out.join("\n");
}

// ── 富文本白名单（详情用）────────────────────────────────────────────────
// 只用**白名单**重建 DOM，不做 blacklist 过滤：
//   - 白名单外的标签：保留子内容、丢掉标签本身（不整段丢弃，否则正文会凭空消失）
//   - 危险标签（script/style/iframe/...）连同内容一起丢弃
//   - 属性只留白名单里的 style 子项，且拒绝 url() / expression() / javascript:
//   - 图片整类丢弃：外链图会发请求（泄露阅读行为），data URI 还可能撑爆内存
const RICH_TAGS = new Set([
  "P", "BR", "DIV", "SPAN", "B", "STRONG", "I", "EM", "U", "S", "STRIKE", "SUB", "SUP",
  "H1", "H2", "H3", "H4", "H5", "H6", "UL", "OL", "LI", "DL", "DT", "DD",
  "TABLE", "THEAD", "TBODY", "TFOOT", "TR", "TD", "TH", "BLOCKQUOTE", "PRE", "CODE", "A", "FONT",
]);
const RICH_DROP = new Set([
  "SCRIPT", "STYLE", "IFRAME", "OBJECT", "EMBED", "LINK", "META", "BASE", "FORM",
  "INPUT", "BUTTON", "TEXTAREA", "SELECT", "SVG", "IMG", "VIDEO", "AUDIO", "CANVAS", "LINK",
]);
const RICH_STYLES = new Set([
  "color", "background-color", "font-weight", "font-style", "text-decoration",
  "text-align", "font-size", "font-family", "line-height",
]);

/// 把一段 HTML 过滤成可安全插入 DOM 的节点树。
/// `onDrop(kind)` 用来把「丢了什么」报给日志（复用现有前端日志通道，不另造一套）。
export function sanitizeRichHtml(html, onDrop) {
  let doc;
  try {
    // DOMParser 解析不会执行脚本，也不加载资源 —— 比 innerHTML 安全得多
    doc = new DOMParser().parseFromString(String(html ?? ""), "text/html");
  } catch (e) {
    onDrop?.(`parse-fail:${e}`);
    return document.createDocumentFragment();
  }

  const root = document.createElement("div");

  const filterStyle = (style) =>
    style
      .split(";")
      .map((s) => s.trim())
      .filter((s) => {
        if (!s) return false;
        const idx = s.indexOf(":");
        if (idx < 0) return false;
        const k = s.slice(0, idx).trim().toLowerCase();
        const v = s.slice(idx + 1).trim().toLowerCase();
        if (!RICH_STYLES.has(k)) return false;
        // 这几种值可以间接执行代码或发起请求
        if (/url\(|expression\(|javascript:|@import/i.test(v)) return false;
        return true;
      })
      .join("; ");

  const walk = (src, dst) => {
    for (const node of src.childNodes) {
      if (node.nodeType === Node.TEXT_NODE) {
        dst.appendChild(document.createTextNode(node.nodeValue));
        continue;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) continue;

      const tag = node.tagName.toUpperCase();
      if (RICH_DROP.has(tag)) {
        onDrop?.(`drop-tag:${tag}`);
        continue; // 连同内容一起丢
      }
      if (!RICH_TAGS.has(tag)) {
        onDrop?.(`unwrap-tag:${tag}`);
        walk(node, dst); // 保留子内容，丢掉外层
        continue;
      }

      // FONT 是富文本时代的老标签，统一换成 span 再保留 color
      const el = document.createElement(tag === "FONT" ? "span" : tag.toLowerCase());

      const style = node.getAttribute?.("style");
      if (style) {
        const keep = filterStyle(style);
        if (keep) el.setAttribute("style", keep);
      }
      if (tag === "FONT") {
        const color = node.getAttribute("color");
        if (color && /^[#\w(),.% -]+$/.test(color)) el.style.color = color;
      }
      if (tag === "A") {
        const href = (node.getAttribute("href") || "").trim();
        if (/^https?:\/\//i.test(href)) {
          // 不用真实 href：点击由前端拦下来交给系统浏览器，避免 WebView 内导航白屏
          el.setAttribute("href", "#");
          el.dataset.ext = href;
          el.className = "md-link";
        }
      }

      walk(node, el);
      dst.appendChild(el);
    }
  };

  walk(doc.body, root);
  return root;
}
