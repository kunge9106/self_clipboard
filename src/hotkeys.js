// 快捷键的键名与组合串：设置页录制、面板匹配关闭键共用一份。
//
// 为什么单独成模块：后端 `hotkey::parse_combo` 认的就是这套键名（`ctrl+alt+v`、
// `esc`、`f5`），前端若在主界面与面板里各写一套映射，迟早出现「设置页能录、
// 面板却不认」这种静默失效。`formats.js` 是同一个理由的先例。
//
// 两套键名白名单是有意的，不是重复实现：
//   - 全局热键（`baseKeyName`）只认字母、数字、`-`、空格、F1–F12 —— 与后端 `vk_of`
//     的白名单一致；其它键录进去后端会拒绝注册。
//   - 面板关闭键（`extendedKeyName`）多认 Esc / Tab / Home 等非字符键 ——
//     这些键在面板里不会和输入冲突，但注册成全局热键就会抢掉系统行为。

const MOD_LABELS = {
  ctrl: "Ctrl",
  control: "Ctrl",
  alt: "Alt",
  shift: "Shift",
  win: "Win",
  super: "Win",
};

/** 修饰键在组合串里的规范顺序。**顺序必须与后端一致**（`parse_combo` 顺序无关，
 *  但字符串比较要用同一种写法，否则「看起来一样」的两个组合串不相等）。 */
export const MOD_ORDER = ["ctrl", "alt", "shift", "win"];

/** 全局热键可用的主键：字母、数字、反引号、减号、空格、F1–F12。 */
export function baseKeyName(key) {
  if (key === " ") return "space";
  if (key.length === 1) {
    const c = key.toLowerCase();
    if (/[a-z0-9`\-]/.test(c)) return c;
    return null;
  }
  if (/^f([1-9]|1[0-2])$/i.test(key)) return key.toLowerCase();
  return null;
}

/** 面板里额外可用的非字符键。这些键不产生文字，绑成关闭键不会干扰搜索输入。 */
const NON_CHAR_KEYS = {
  Escape: "esc",
  Tab: "tab",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
  Insert: "insert",
  Delete: "delete",
  Backspace: "backspace",
};

/** 面板关闭键可用的主键：全局热键那套 + 非字符键。 */
export function extendedKeyName(key) {
  const extra = NON_CHAR_KEYS[key];
  if (extra) return extra;
  return baseKeyName(key);
}

/** 「不带修饰键也能绑」的主键白名单。字母与数字不在其中：绑成裸键之后，
 *  在搜索框里打字就会关掉面板。 */
export const BARE_CLOSE_KEYS = new Set([
  "esc",
  "tab",
  "home",
  "end",
  "pageup",
  "pagedown",
  "insert",
  "delete",
  "backspace",
  "f1",
  "f2",
  "f3",
  "f4",
  "f5",
  "f6",
  "f7",
  "f8",
  "f9",
  "f10",
  "f11",
  "f12",
]);

/** 面板自己占用的键：上下选择、回车粘贴。绑成关闭键会让面板没法用。 */
export const RESERVED_CLOSE_KEYS = new Set(["up", "down", "enter"]);

/** `ctrl+alt+v` → `{ mods: ["ctrl","alt"], main: "v" }`。空串或缺主键返回 null。 */
export function parseCombo(combo) {
  const parts = String(combo || "")
    .split("+")
    .map((p) => p.trim().toLowerCase())
    .filter(Boolean);
  if (parts.length === 0) return null;
  const mods = [];
  let main = null;
  for (const p of parts) {
    if (p in MOD_LABELS) {
      const norm = p === "control" ? "ctrl" : p === "super" ? "win" : p;
      if (!mods.includes(norm)) mods.push(norm);
    } else {
      if (main !== null) return null; // 两个主键，非法
      main = p;
    }
  }
  if (!main) return null;
  mods.sort((a, b) => MOD_ORDER.indexOf(a) - MOD_ORDER.indexOf(b));
  return { mods, main };
}

/** 组合串 → 显示文本：`ctrl+alt+v` → `Ctrl + Alt + V`。 */
export function comboLabel(combo) {
  const parsed = parseCombo(combo);
  if (!parsed) return "";
  const keys = parsed.mods.map((m) => MOD_LABELS[m]);
  if (parsed.main === "space") keys.push("空格");
  else if (parsed.main === "esc") keys.push("Esc");
  else if (parsed.main === "tab") keys.push("Tab");
  else keys.push(parsed.main.toUpperCase());
  return keys.join(" + ");
}

/** 一次按键事件 → `{ combo, mods, main }`；不认识的键返回 null。
 *
 *  `extended` 决定用哪份主键白名单：全局热键录制传 false（与后端 `vk_of` 对齐），
 *  面板关闭键传 true。 */
export function comboFromEvent(e, { extended = false } = {}) {
  const main = extended ? extendedKeyName(e.key) : baseKeyName(e.key);
  if (!main) return null;
  const mods = [];
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");
  if (e.shiftKey) mods.push("shift");
  if (e.metaKey) mods.push("win");
  return { combo: [...mods, main].join("+"), mods, main };
}

/** 这次按键是不是 `combo` 所指的那一组。面板用它判断关闭键。 */
export function matchesCombo(e, combo) {
  const want = parseCombo(combo);
  if (!want) return false;
  const got = comboFromEvent(e, { extended: true });
  if (!got) return false;
  if (got.main !== want.main) return false;
  if (got.mods.length !== want.mods.length) return false;
  return want.mods.every((m) => got.mods.includes(m));
}

/** 关闭键的合法性检查。返回 `null` 表示可用，否则是给用户看的原因。
 *
 *  规则：非字符键可以裸绑（Esc、F5、Home…）；字母、数字、空格等会与搜索输入
 *  冲突的键必须带修饰键；面板自己占用的上下键与回车一律不可绑。 */
export function checkPanelCloseKey(combo, { callUpHotkey = "" } = {}) {
  const parsed = parseCombo(combo);
  if (!parsed) return "缺少主键";
  if (RESERVED_CLOSE_KEYS.has(parsed.main)) {
    return `${comboLabel(combo)} 是面板的选择 / 粘贴键，不能用作关闭键`;
  }
  if (parsed.mods.length === 0 && !BARE_CLOSE_KEYS.has(parsed.main)) {
    return "这个键要带修饰键（Ctrl / Alt / Shift / Win），否则打字时会关掉面板";
  }
  const callUp = parseCombo(callUpHotkey);
  if (callUp && callUp.main === parsed.main && callUp.mods.join("+") === parsed.mods.join("+")) {
    return "与弹出面板的快捷键相同，按下会同时触发";
  }
  return null;
}
