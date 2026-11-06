#!/usr/bin/env node
/**
 * 手动回收「回收站里属于本仓库构建产物」的空间。
 *
 * ## 什么会进回收站（实测结论，别再猜）
 *
 * 在本机 D 盘做了对照实验，四种删除方式各测一遍、统计 `$I` 条目增量：
 *
 *   · 覆盖写入已存在文件（`> file`）        → 0
 *   · 写临时文件后 rename 覆盖（编辑器保存） → 0
 *   · `rm -f`（shell 永久删除）             → 0
 *   · `node fs.rmSync`                      → 0
 *
 * 也就是说 cargo / vite / 编辑器的正常写盘**不会**往回收站塞东西。
 * 曾经写在这里的「工具链覆盖 .o 时留副本」是错的：那是把「回收站里原路径在
 * target 下」当成了「target 的写入造成的」。真正会产生条目的是走 Windows 外壳
 * 的删除 —— 资源管理器右键删除、删除确认对话框、某些清理工具的「删除到回收站」。
 * 17539 条 `target` 记录就是这么一次性整目录删出来的。
 *
 * ## 所以这个脚本的定位
 *
 * 不是构建流程的一环，是**事后手动兜底**：万一用资源管理器删了 `target` / `dist`，
 * 空间被回收站吃掉，用它精确捞回来。刻意**不挂 postbuild** ——
 * 构建不该碰回收站，而清理两万条目要按单文件 ~0.85s 串行走，会白拖慢每次打包。
 *
 * ## 安全边界
 *
 * 只处理同时满足两条的条目：
 *   1. 原路径在**本仓库目录**下；
 *   2. 落在 `src-tauri/target/`、`dist/`、`node_modules/.vite/` 三者之一。
 *
 * 桌面上被删的 PPT、视频，仓库里 src 的历史版本，一律不碰 —— 那些可能是
 * 用户想恢复的东西，脚本无权替他决定。用户已明确「回收站我自己手动清」，
 * 所以默认只预览，`--delete` 必须由人显式敲。
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

// 并发删除：一条一条 `rmSync` 实测只有 ~2 项/秒（大头是 $R 目录里成百上千个
// .o，逐个 unlink 受磁盘与杀软拖累），18000 项要跑两个多小时。libuv 线程池
// 默认 4 个线程，这里开 32 路并发把等待叠起来。
//
// 幂等：先删 $R 再删 $I。中途被打断时残留的「$I 在、$R 不在」条目，下次运行
// 会被 `force: true` 直接忽略数据、补删元数据 —— 不会留下打不开的僵尸条目。
const fsp = fs.promises;
const CONCURRENCY = 32;
let cursor = 0;
let freed = 0;
let failed = 0;

async function worker() {
  while (cursor < targets.length) {
    const t = targets[cursor++];
    try {
      await fsp.rm(t.dataPath, { recursive: true, force: true });
      await fsp.rm(t.infoPath, { force: true });
      freed++;
    } catch {
      failed++;
    }
  }
}

await Promise.all(Array.from({ length: CONCURRENCY }, worker));
console.log(`\n已回收 ${freed} 项（${gb(totalSize)}）${failed ? `，${failed} 项失败` : ""}`);
void mb;
