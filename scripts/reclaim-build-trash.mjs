#!/usr/bin/env node
/**
 * 回收「构建过程塞进回收站」的空间。
 *
 * ## 为什么需要这个脚本
 *
 * 这台机器上，**覆盖或删除已存在的文件**都会在回收站留一份副本 —— 包括：
 *   · cargo / rustc 覆盖 `target/**\/*.o`、清理陈旧增量缓存；
 *   · vite 重建时删掉旧的 `dist/`、`node_modules/.vite/`；
 *   · 编辑器保存 `src/*.rs`（写临时文件再 rename 覆盖）。
 * 一次 debug 构建就能塞进去几个 GB，连跑几轮就是几十 GB。磁盘看着没东西，
 * 空间却回不来 —— 用户实测「回收站又攒了几十 G」就是这么来的。
 *
 * 这不是我们 `rm` 造成的（shell `rm` 是永久删除），而是工具链的删除走了回收站，
 * 所以只能在**事后回收**，并且必须精确到本项目。
 *
 * ## 安全边界
 *
 * 只处理同时满足两条的条目：
 *   1. 原路径在**本仓库目录**下；
 *   2. 落在 `src-tauri/target/`、`dist/`、`node_modules/.vite/` 三者之一。
 *
 * 桌面上被删的 PPT、视频，仓库里 src 的历史版本，一律不碰 —— 那些可能是
 * 用户想恢复的东西，脚本无权替他决定。
 *
 * ## 用法
 *
 *   node scripts/reclaim-build-trash.mjs            # 只报告（默认）
 *   node scripts/reclaim-build-trash.mjs --delete   # 确认后真的删
 *
 * 回收站条目的 `$I<id>` 是元数据（原始路径 / 大小 / 删除时间），`$R<id>` 是数据。
 * 两个文件按同一个 `<id>` 配对，删的时候必须成对删，否则资源管理器里会留下
 * 打不开的僵尸条目。
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const drive = root.slice(0, 2);
const binRoot = path.join(`${drive}\\`, "$Recycle.Bin");
const doDelete = process.argv.includes("--delete");

/** 只认这三处：都是能由源码/工具重新生成的产物 */
const BUILD_DIRS = [
  path.join(root, "src-tauri", "target"),
  path.join(root, "dist"),
  path.join(root, "node_modules", ".vite"),
].map((p) => p.toLowerCase());

function parseInfoFile(file) {
  const b = fs.readFileSync(file);
  if (b.length < 28) return null;
  const size = b.readBigUInt64LE(8);
  const filetime = b.readBigUInt64LE(16);
  const nameLen = b.readUInt32LE(24);
  // 名字长度是字符数，正常路径不会超过 4096 字符；超出说明不是 $I 格式
  if (nameLen > 4096) return null;
  const name = b.toString("utf16le", 28, 28 + nameLen * 2).replace(/\0+$/, "");
  if (!name) return null;
  return {
    size,
    name,
    ms: Number(filetime / 10000n) - 11644473600000,
  };
}

function inBuildDirs(name) {
  const lower = name.toLowerCase();
  return BUILD_DIRS.some((d) => lower === d || lower.startsWith(d + "\\"));
}

if (!fs.existsSync(binRoot)) {
  console.log(`没有回收站目录：${binRoot}`);
  process.exit(0);
}

// 一台机器上可能有多个 SID 目录（不同账户），逐个看，没权限的跳过
const sids = fs.readdirSync(binRoot).filter((n) => {
  try {
    return fs.statSync(path.join(binRoot, n)).isDirectory();
  } catch {
    return false;
  }
});

let totalSize = 0n;
let matched = 0;
const groups = new Map();
const targets = [];

for (const sid of sids) {
  const dir = path.join(binRoot, sid);
  let entries;
  try {
    entries = fs.readdirSync(dir);
  } catch {
    continue; // 无权限（别的账户的回收站）
  }
  for (const f of entries) {
    if (!f.startsWith("$I")) continue;
    let info;
    try {
      info = parseInfoFile(path.join(dir, f));
    } catch {
      continue;
    }
    if (!info || !inBuildDirs(info.name)) continue;
    const id = f.slice(2);
    const dataPath = path.join(dir, `$R${id}`);
    matched++;
    totalSize += info.size;
    targets.push({ infoPath: path.join(dir, f), dataPath });

    const rel = info.name.slice(root.length + 1).split("\\").slice(0, 2).join("\\");
    const g = groups.get(rel) || { n: 0, size: 0n };
    g.n++;
    g.size += info.size;
    groups.set(rel, g);
  }
}

const gb = (b) => (Number(b) / 1024 ** 3).toFixed(2) + " GB";
const mb = (b) => (Number(b) / 1024 ** 2).toFixed(1) + " MB";

console.log(`扫描盘符 ${drive} 的回收站，命中本项目构建产物 ${matched} 项，合计 ${gb(totalSize)}`);
for (const [k, v] of [...groups.entries()].sort((a, b) => Number(b[1].size - a[1].size))) {
  console.log(`  ${gb(v.size).padStart(8)}  ${String(v.n).padStart(6)} 个  ${k}`);
}
console.log(`（个人文件、src 历史版本等不在本脚本处理范围内，未计入）`);

if (!doDelete) {
  console.log(`\n这是预览。确认后执行：node scripts/reclaim-build-trash.mjs --delete`);
  process.exit(0);
}

let freed = 0;
let failed = 0;
for (const t of targets) {
  try {
    fs.rmSync(t.dataPath, { recursive: true, force: true });
    fs.rmSync(t.infoPath, { force: true });
    freed++;
  } catch {
    failed++;
  }
}
console.log(`\n已回收 ${freed} 项（${gb(totalSize)}）${failed ? `，${failed} 项失败` : ""}`);
void mb;
