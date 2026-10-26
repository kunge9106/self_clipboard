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

// ── 卡片用的 Markdown「扁平化」渲染 ─────────────────────────────────────
/// 卡片只有两行高，不能出现块级元素（标题放大、列表缩进会把内容挤没）。
/// 所以这里**只做「去标记 + 行内样式」**：
///   标题 → 去掉 `#`、整行加粗
///   列表 → 换成圆点、去掉 `- `
///   引用 → 换成竖线、去掉 `> `
///   表格 → 竖线换成间隔点
///   围栏 → 整行去掉（代码本身仍在下一行显示）
/// 出来的仍是纯文字流，`-webkit-line-clamp: 2` 依然生效。
export function cardMarkdown(text) {
  return String(text ?? "")
    .split("\n")
    .map((line) => {
      if (/^\s*```/.test(line)) return ""; // 围栏行不要
      const h = line.match(/^\s*#{1,6}\s+(.*)$/);
      if (h) return `<strong>${inlineFormat(h[1])}</strong>`;
      const ul = line.match(/^(\s*)[-*+]\s+(.*)$/);
      if (ul) {
        // 任务列表：卡片里没有空间画方块，用 ☑ / ☐ 两个字符表示勾选状态。
        // 不显示成 `• [x]` —— 那是没解析的原始标记，正是用户之前抱怨过的观感。
        const tm = ul[2].match(/^\[([ xX])\]\s+(.*)$/);
        if (tm) {
          const mark = tm[1].toLowerCase() === "x" ? "☑" : "☐";
          return `${ul[1]}<span class="md-bullet">${mark}</span> ${inlineFormat(tm[2])}`;
        }
        return `${ul[1]}<span class="md-bullet">•</span> ${inlineFormat(ul[2])}`;
      }
      const ol = line.match(/^(\s*)(\d+\.)\s+(.*)$/);
      if (ol) return `${ol[1]}<span class="md-bullet">${ol[2]}</span> ${inlineFormat(ol[3])}`;
      const bq = line.match(/^\s*>\s?(.*)$/);
      if (bq) return `<span class="md-quote">│</span> ${inlineFormat(bq[1])}`;
      if (/^\s*\|/.test(line)) {
        const cells = line.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
        if (cells.every((c) => /^:?-{2,}:?$/.test(c))) return ""; // 分隔行不要
        return inlineFormat(cells.join("  ·  "));
      }
      return inlineFormat(line);
    })
    .filter((l) => l.trim() !== "")
    .join("\n");
}

// ── 卡片预览（入口）──────────────────────────────────────────────────────
/// 按子类型返回卡片正文的 HTML。调用方直接赋给 innerHTML 即可（内容已全部转义）。
export function cardPreviewHtml(text, subtype) {
  if (subtype === "code") return highlightCode(text);
  if (subtype === "markdown") return cardMarkdown(text);
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
/// 导出给卡片用：卡片与详情的行内规则必须一致，否则同一段在列表里和弹窗里长得不一样。
export function inlineFormat(text) {
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
  // 列表栈：每层记 { tag, indent, liOpen }。
  // 单层 `list` 变量做不了嵌套 —— `- a` 里再缩进一层必须把 `<ul>` 放进上一个
  // `<li>` 内部，而 `<li>` 得**延迟闭合**。所以栈里还要记这一层是否开着 li。
  const listStack = [];
  const closeList = () => {
    while (listStack.length) {
      const top = listStack.pop();
      if (top.liOpen) out.push("</li>");
      out.push(`</${top.tag}>`);
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
      out.push(`<h${lv}>${inlineFormat(h[2])}</h${lv}>`);
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
      out.push(`<blockquote>${inlineFormat(buf.join("\n")).replace(/\n/g, "<br>")}</blockquote>`);
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
          .map((c) => `<th>${inlineFormat(c)}</th>`)
          .join("")}</tr></thead><tbody>${rows
          .map((r) => `<tr>${r.map((c) => `<td>${inlineFormat(c)}</td>`).join("")}</tr>`)
          .join("")}</tbody></table>`
      );
      continue;
    }

    // 列表（支持嵌套与任务列表）
    // 缩进宽度决定层级：制表符按 4 空格算，覆盖各种编辑器的输出。
    const lm = line.match(/^(\s*)([-*+]|\d+\.)\s+(.*)$/);
    if (lm) {
      const indent = lm[1].replace(/\t/g, "    ").length;
      const tag = /\d/.test(lm[2]) ? "ol" : "ul";
      let body = lm[3];

      // 任务列表：`- [ ] xxx` / `- [x] xxx`
      let task = null; // null = 普通列表项
      const tm = body.match(/^\[([ xX])\]\s+(.*)$/);
      if (tm) {
        task = tm[1].toLowerCase() === "x";
        body = tm[2];
      }

      // 回到合适层级：缩进比栈顶浅就把那几层收掉（每层先关掉自己开着的 li）
      while (listStack.length && indent < listStack[listStack.length - 1].indent) {
        const top = listStack.pop();
        if (top.liOpen) out.push("</li>");
        out.push(`</${top.tag}>`);
      }

      const top = listStack[listStack.length - 1];
      if (!top || indent > top.indent) {
        // 更深一层：**不关上一层的 li**，直接嵌套进去
        listStack.push({ tag, indent, liOpen: false });
        out.push(`<${tag}>`);
      } else {
        // 同级：先关掉上一条 li
        if (top.liOpen) {
          out.push("</li>");
          top.liOpen = false;
        }
        if (top.tag !== tag) {
          // `- ` 接 `1. ` 这种类型切换：换标签，层级不变
          out.push(`</${top.tag}>`);
          listStack[listStack.length - 1] = { tag, indent, liOpen: false };
          out.push(`<${tag}>`);
        }
      }

      const box =
        task === null ? "" : `<span class="md-box${task ? " is-on" : ""}"></span>`;
      out.push(
        `<li${task === null ? "" : ' class="md-task"'}>${box}${inlineFormat(body)}`
      );
      listStack[listStack.length - 1].liOpen = true;
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
    out.push(`<p>${inlineFormat(buf.join("\n")).replace(/\n/g, "<br>")}</p>`);
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
  "INPUT", "BUTTON", "TEXTAREA", "SELECT", "SVG", "VIDEO", "AUDIO", "CANVAS",
]);

/// 单张内联图片的大小上限（base64 **字符数**，解码后约为其 3/4）。
///
/// 只放行 `data:` 内联图，**外链一律丢弃**：`<img src="https://…">` 会在渲染时
/// 自动发请求，等于把「我在读这段内容」告诉对方服务器。内联图是本地的，没有这个问题。
const RICH_IMG_MAX = 1_200_000;
/// 一条内容里所有内联图的合计上限。防止一条长文档把详情页拖死。
const RICH_IMG_TOTAL = 6_000_000;
/// 只认这几种位图。**不含 svg** —— SVG 可以内嵌脚本，即使走 data: 也不安全。
const RICH_IMG_MIME = /^data:image\/(png|jpe?g|gif|webp|bmp);base64,/i;
const RICH_STYLES = new Set([
  "color", "background-color", "font-weight", "font-style", "text-decoration",
  "text-align", "font-size", "font-family", "line-height",
]);

// ── CF_HTML 头部剥离（详情渲染前必做）──────────────────────────────────
/// 从 Windows 剪贴板的 `HTML Format` 里取出**正文片段**，丢掉封装头。
///
/// `HTML Format` 不是裸 HTML，而是一份带头的 CF_HTML：
///
///   Version:1.0
///   StartHTML:0000000192            ← 整份文档起点
///   EndHTML:0000042891              ← 整份文档终点
///   StartFragment:0000040287        ← 正文片段起点（**字节**偏移）
///   EndFragment:0000042851          ← 正文片段终点
///   SourceURL:file:///D:/…/报告.docx ← 来源路径
///
/// 这些偏移是**回贴时原样写回**所必需的（自己重算容易错，尤其带中文时），
/// 所以存储必须完整保留。但对用户毫无意义，而且 SourceURL 会暴露本地路径 ——
/// 绝不能出现在「查看」里。
///
/// 不剥会怎样：DOMParser 把 `<html>` 之前那些纯文本当成正文文本节点，
/// 于是详情第一段就变成 `Version:1.0 StartHTML:0000000192 …`（用户实测截图）。
export function cfHtmlFragment(raw) {
  const s = String(raw ?? "");
  if (!/^\s*Version:/i.test(s)) return s; // 不是 CF_HTML，原样交出

  const sf = s.match(/StartFragment:(\d+)/i);
  const ef = s.match(/EndFragment:(\d+)/i);
  if (sf && ef) {
    // 偏移按**字节**算，不是字符 —— 必须走 TextEncoder/TextDecoder。
    // 中文内容下按字符切会偏，切出来的片段会从半个字中间开始。
    const bytes = new TextEncoder().encode(s);
    const from = Number(sf[1]);
    const to = Number(ef[1]);
    if (from > 0 && to > from && to <= bytes.length) {
      return new TextDecoder("utf-8").decode(bytes.slice(from, to));
    }
  }
  // 偏移缺失或越界（有的应用会写全 0）：退化成「截掉头部」。
  // 头部一定在第一个 '<' 之前，从那里开始就是正文。
  const i = s.indexOf("<");
  return i >= 0 ? s.slice(i) : "";
}

/// 内联图片的准入判断（纯函数，便于单测）。
///
/// 返回 `"ok"` / `"external"`（外链或 svg）/ `"too-large"` / `"budget"`。
export function richImgVerdict(src, usedChars = 0) {
  const s = String(src ?? "").trim();
  if (!RICH_IMG_MIME.test(s)) return "external";
  if (s.length > RICH_IMG_MAX) return "too-large";
  if (usedChars + s.length > RICH_IMG_TOTAL) return "budget";
  return "ok";
}

/// 把一段 HTML 过滤成可安全插入 DOM 的节点树。
/// `onDrop(kind)` 用来把「丢了什么」报给日志（复用现有前端日志通道，不另造一套）。
export function sanitizeRichHtml(html, onDrop) {
  let doc;
  try {
    // 先剥掉 CF_HTML 封装头：否则 `Version:1.0 StartHTML:…` 会被 DOMParser
    // 当成正文文本节点显示在开头（用户实测截图）。
    const body = cfHtmlFragment(html);
    if (!body.trim()) {
      onDrop?.("empty-fragment");
      return document.createDocumentFragment();
    }
    // DOMParser 解析不会执行脚本，也不加载资源 —— 比 innerHTML 安全得多
    doc = new DOMParser().parseFromString(body, "text/html");
  } catch (e) {
    onDrop?.(`parse-fail:${e}`);
    return document.createDocumentFragment();
  }

  const root = document.createElement("div");
  /// 已放进来的内联图大小合计（字符数），用于 `RICH_IMG_TOTAL` 预算
  let imgChars = 0;

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

      // 图片单独处理：只放行内联 data: 位图，且受单张与合计两道预算约束
      if (tag === "IMG") {
        const src = (node.getAttribute?.("src") || "").trim();
        const verdict = richImgVerdict(src, imgChars);
        if (verdict !== "ok") {
          onDrop?.(`drop-img:${verdict}`);
          continue;
        }
        imgChars += src.length;
        const img = document.createElement("img");
        // 只赋 src 与 alt：width/height/style 一律不带（否则原文档的固定尺寸
        // 会让图片溢出详情区），显示尺寸交给 CSS 控制。
        img.src = src;
        const alt = node.getAttribute?.("alt");
        if (alt) img.alt = String(alt).slice(0, 200);
        img.loading = "lazy";
        dst.appendChild(img);
        continue;
      }

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
