// 悬浮预览的「内容」部分：判断有没有更多、取内容、按类型填 DOM。
//
// 两处在用：主界面的卡片列表（1060×690）与快捷面板（620×460 的单行列表）。
// 内容口径必须一份 —— 否则同一个条目会出现「主界面弹 4 行、面板弹 3 行」这种两套规格；
// 定位则各自实现：主界面的卡片常贴在列表底部，要四个方向挨个试；面板空间小，
// 只在上/下二选一然后夹进窗口。

/** 文件清单最多列几个；多出来的折成「另有 N 个」。 */
export const PREVIEW_FILES_MAX = 3;

/** 载荷护栏（不是显示行数）。显示行数由 CSS 的 `-webkit-line-clamp` 决定：
 *  只有浏览器知道一行折成了几行，JS 数 `\n` 会漏判超长单行。这里只防止把几万字
 *  塞进 DOM 让排版变慢。 */
const PREVIEW_TEXT_GUARD_LINES = 40;
const PREVIEW_TEXT_GUARD_CHARS = 1200;

/**
 * 卡片 / 行上已经显示全的内容不给浮层 —— 再弹一次同样的字只是噪音。
 *
 * 「是不是还有更多」一律问后端，前端不照抄一份判断规则：
 *   文本 → `preview_truncated`，即后端生成摘要时得出的「有没有丢弃内容」；
 *          照抄「2 行 / 140 字」到前端，日后改一边必漂移。
 *   文件 → 只有 1 个时行上已经写着文件名，浮层给不出新信息。
 *   图片 → 缩略图必然是缩过的，大图永远有增量。
 */
export function hasMoreToPreview(item) {
  if (item.kind === "text") return !!item.preview_truncated;
  if (item.kind === "files") return (item.files || []).length > 1;
  return true;
}

/** 文本载荷护栏，见上面的常量注释。 */
export function guardPreviewText(text) {
  let s = text;
  const lines = s.split("\n");
  if (lines.length > PREVIEW_TEXT_GUARD_LINES) {
    s = lines.slice(0, PREVIEW_TEXT_GUARD_LINES).join("\n");
  }
  if (s.length > PREVIEW_TEXT_GUARD_CHARS) {
    s = s.slice(0, PREVIEW_TEXT_GUARD_CHARS);
  }
  return s;
}

/** 取文件名：浮层只给名字，完整路径留给右键「查看」 */
export function baseName(path) {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i >= 0 ? path.slice(i + 1) : path;
}

/** 填文件清单槽：前 3 个文件名 + 折叠行。 */
export function fillFiles(ul, item) {
  const all = Array.isArray(item.files) ? item.files : [];
  const lis = all.slice(0, PREVIEW_FILES_MAX).map((p) => {
    const li = document.createElement("li");
    li.textContent = baseName(p);
    return li;
  });
  if (all.length > PREVIEW_FILES_MAX) {
    const li = document.createElement("li");
    li.className = "hp-more";
    li.textContent = `…另有 ${all.length - PREVIEW_FILES_MAX} 个`;
    lis.push(li);
  }
  // 极老的条目可能没有 files 字段 → 退回行上那条摘要，至少不是个空浮层
  if (lis.length === 0) {
    const li = document.createElement("li");
    li.textContent = item.preview || "";
    lis.push(li);
  }
  ul.replaceChildren(...lis);
}

/**
 * 建浮层骨架：三个内容槽常驻，按类型显示其中一个。
 * 样式来自 styles.css 的 `.hover-preview` / `.hp-*`，两个页面共用同一套外观。
 */
export function createPreviewBox() {
  const box = document.createElement("div");
  box.className = "hover-preview";
  box.hidden = true;
  const img = document.createElement("img");
  img.alt = "预览";
  const text = document.createElement("div");
  text.className = "hp-text";
  const files = document.createElement("ul");
  files.className = "hp-files";
  box.append(img, text, files);
  document.body.appendChild(box);
  return box;
}

/**
 * 只留 kind 对应的内容槽，其余清空。
 *
 * 清空而不是只隐藏：图片槽留着上一次的 src 会一直占着解码后的位图内存，
 * 文本 / 清单留着也无意义。浮层是纯展示层，每次重建内容的成本可以忽略。
 */
export function pickSlot(box, kind) {
  const img = box.querySelector("img");
  const text = box.querySelector(".hp-text");
  const files = box.querySelector(".hp-files");
  img.hidden = kind !== "image";
  text.hidden = kind !== "text";
  files.hidden = kind !== "files";
  if (kind !== "image") img.removeAttribute("src");
  if (kind !== "text") text.textContent = "";
  if (kind !== "files") files.replaceChildren();
  return { img, text, files };
}

/** 收起浮层并松开图片引用（大图 base64 常驻会一直占内存）。 */
export function resetPreviewBox(box) {
  if (!box) return;
  box.hidden = true;
  box.querySelector("img").removeAttribute("src");
}
