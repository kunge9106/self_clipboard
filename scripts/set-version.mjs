#!/usr/bin/env node
/**
 * 一次性同步版本号。
 *
 * 版本源是 `package.json` 的 `version` 字段。npm 与 cargo 两个工具链各自必须能从
 * 自己的清单里读到版本，互相读不了，所以 `src-tauri/Cargo.toml` 必须单独持有一份，
 * 由本脚本同步 —— 这是双清单生态的硬约束，不是重复。
 *
 * 各文件的角色：
 *   - `package.json`           版本源。手工或本脚本修改。
 *   - `package-lock.json`      npm 生成，含自身版本两处；本脚本同步。
 *   - `src-tauri/Cargo.toml`   cargo 清单；本脚本同步。
 *   - `src-tauri/Cargo.lock`   cargo 生成；本脚本同步，避免构建时产生未提交改动。
 *   - `src-tauri/tauri.conf.json`
 *                              其 `version` 为 `"../package.json"`（Tauri 支持的引用写法），
 *                              **不需要改**，构建与运行时都从 package.json 取值。
 *   - `src/main.js`            状态栏版本号走 `getVersion()`，不硬编码。
 *
 * 用法：npm run version:set 2.1.11
 *       npm run version:set 2.2.0-beta   （预发布后缀可带）
 */
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const next = (process.argv[2] || "").trim();

if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(next)) {
  console.error(
    "用法：npm run version:set <x.y.z>\n" +
      "      x.y.z 为 semver，可带预发布后缀，例如 2.1.11 / 2.2.0-beta"
  );
  process.exit(1);
}

/** 每项：文件、定位自身版本号的正则（捕获前导部分）、替换构造。
 *  换行一律写成 `\r?\n`：仓库里的文件既有 LF 也有 CRLF（锁文件由工具在 Windows 下生成）。 */
const targets = [
  {
    file: "package.json",
    find: /"version":\s*"[^"]*"/,
    build: () => `"version": "${next}"`,
    global: false,
  },
  {
    // 顶层与 packages[""] 各一处，都以 name 紧跟 version 的形式出现
    file: "package-lock.json",
    find: /("name":\s*"self-clipboard",\s*\r?\n\s*)"version":\s*"[^"]*"/g,
    build: (m, p1) => `${p1}"version": "${next}"`,
    global: true,
  },
  {
    file: "src-tauri/Cargo.toml",
    find: /(\[package\][^[]*?\r?\nversion\s*=\s*)"[^"]*"/,
    build: (m, p1) => `${p1}"${next}"`,
    global: false,
  },
  {
    file: "src-tauri/Cargo.lock",
    find: /(\[\[package\]\]\r?\nname = "self-clipboard"\r?\nversion = ")[^"]*"/,
    build: (m, p1) => `${p1}${next}"`,
    global: false,
  },
];

let hadError = false;

for (const { file, find, build, global } of targets) {
  const path = join(root, file);
  const before = readFileSync(path, "utf8");

  // test 探测用不带 g 的副本，避免 lastIndex 干扰
  const probe = new RegExp(find.source);
  if (!probe.test(before)) {
    console.error(`× ${file}：未匹配到自身版本号，文件结构可能已变，需人工检查`);
    hadError = true;
    continue;
  }

  const after = before.replace(global ? find : probe, build);
  if (after === before) {
    console.log(`- ${file}：已是 ${next}`);
    continue;
  }
  writeFileSync(path, after);
  console.log(`✓ ${file}`);
}

if (hadError) {
  console.error("\n有文件未能同步，版本号可能处于不一致状态，请人工核对。");
  process.exit(1);
}

console.log(
  `\n版本号已同步为 ${next}。\n` +
    "下一步：提交，然后 npm run tauri build 出包（产物名会带该版本号）。"
);
