# 计划：存储层迁移到 SQLite（暂不实施）

> 状态：**已列入计划，未开工**。本文只记录结论与路径，动手前需重新评估。
> 记录时间：2026-11-03

## 一、为什么会有这个计划

`fix/window-frame-and-dedup-window` 这一支修掉的存储类问题，几乎全部源于
**自研存储层的结构性限制**：

| 现象 | 根因 |
|---|---|
| 「复制不到内容」 | `replay_wal` 让 `WAL.file` 留空，`ensure_wal_dir` 只比对目录就早返回 → 启动后所有写入失败 |
| 复制同样内容崩两次 | 去重置顶时按「置顶优先」重排常驻列表，破坏「恒 ts 降序」不变式 |
| 「启动即崩、连崩三次」 | `query::scan` 用 `debug_assert` 检查**持久化数据**的顺序，而工程设了 `panic = "abort"` |
| 「删除后复制不进」 | 删除条目不清指纹队列 → 墓碑 → 再复制命中死 id，既不新增也不置顶 |
| 跨重启去重失效 | 指纹是内存队列，重启清空 → 只能靠「启动预热」补 |
| 预热 400 多条全不命中 | 正文是 AES-256-GCM 加密存储，预热直接 `fs::read` 拿到密文 → 哈希对不上 |

共同点：**整份 JSON 加密 → 无法按内容查询 → 只能把指纹搬进内存 → 才需要预热与墓碑**。

SQLCipher（SQLite 的加密分支）可以让「加密」与「可查询」共存，从根上消掉这个矛盾。

## 二、结论：不需要用户安装任何东西

- `rusqlite` + `bundled`：SQLite 源码静态编译进 exe（约 +1MB）
- 加 SQLCipher：再 +1.5–3MB（含 OpenSSL）
- 发布形态仍是**单文件绿色版**，双击即用、无需运行库、无需管理员权限

参考实现：ClipVault（Windows 剪贴板管理器，Rust + Tauri）——SQLCipher 加密库 +
SHA-256 内容哈希去重 + FTS5 全文索引，图片走 AES-256-GCM 文件存储。

## 三、能根治什么

- **顺序不变式消失**：列表由 `ORDER BY ts DESC` 给出，不存在「必须维护有序 Vec」
- **去重不再需要内存队列**：`content_hash` 列 + 索引，`WHERE hash = ?`；
  删除即消失，**墓碑问题不存在**
- **跨重启天然成立**：不需要启动预热
- **自研 WAL / compact / 索引重建全部删除**
- **加密与查询不再冲突**：SQLCipher 整库加密，SQL 照常可用
- **分页与搜索**：`LIMIT/OFFSET` + 索引，搜索可上 FTS5

预计可删除约：指纹队列 + 预热 + 墓碑 + 自研 WAL + 索引快照/compact + `query::scan` 的分组逻辑（约 1500–2000 行）。

## 四、不解决什么（重要，别指望过高）

数据库只管存储。以下问题与它无关，迁移后**一模一样**：

- 窗口无边框 / 材质切换变灰 / 任务栏最小化（Win32 + DWM）
- 快捷面板拖动重影（自定义拖动 + `SetCapture` 线程归属）
- 微信重复写剪贴板、同源文本合并（采集时序）
- 列表新增时闪烁（前端虚拟列表渲染）
- 剪贴板监听本身（`AddClipboardFormatListener`）

## 五、表结构草案

```sql
-- 条目元数据
CREATE TABLE item (
  id            TEXT PRIMARY KEY,   -- 现有 id 格式不变（base36 时间 + 序号）
  ts            INTEGER NOT NULL,   -- 毫秒
  kind          TEXT NOT NULL,      -- text / image / files
  size          INTEGER NOT NULL,
  preview       TEXT NOT NULL,
  preview_cut   INTEGER NOT NULL DEFAULT 0,
  subtype       TEXT,
  favorite      INTEGER NOT NULL DEFAULT 0,
  edited        INTEGER NOT NULL DEFAULT 0,
  shot          INTEGER NOT NULL DEFAULT 0,
  content_hash  INTEGER NOT NULL,   -- 去重主键；见下方「哈希一致性」
  img_path      TEXT,
  files_path    TEXT,
  files_full    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_item_ts ON item(ts DESC);
CREATE UNIQUE INDEX idx_item_hash ON item(content_hash, kind);  -- 去重靠它

-- 置顶按视图独立（现有 pinned_views 语义）
CREATE TABLE pin (
  item_id TEXT NOT NULL REFERENCES item(id) ON DELETE CASCADE,
  view    TEXT NOT NULL,
  PRIMARY KEY (item_id, view)
);

-- 正文/文件清单：仍存文件，库里只放相对路径（大对象不进 BLOB）
-- 图片同样只存路径；几 MB 的图进出 SQLite 不值得
```

**哈希一致性是唯一必须守住的东西**：入库时算的哈希与去重时查的哈希必须来自同一份
字节（当前踩过「对密文算哈希」的坑）。SQLCipher 整库加密后，读出即明文，这个问题
自然消失。

## 六、分阶段落地（若决定做）

**阶段 0：影子写入（不改读路径）**
新库与旧格式并行写，旧路径照常读。跑一周，比对两边条数与内容，确认无损。

**阶段 1：切换读路径**
列表/搜索/分页改走 SQL；旧的快照+WAL 保留为备份。此阶段删掉
指纹队列、预热、墓碑、`query::scan`。

**阶段 2：迁移旧数据 + 清理**
一次性导入存量（现有 666 条），校验通过后停止写旧格式。

每阶段都有独立提交与回滚开关（配置里 `storage_backend = legacy | sqlite`）。

## 七、风险与成本

| 项 | 量级 / 说明 |
|---|---|
| 依赖体积 | +2~4MB |
| 首次编译 | 慢 1–3 分钟（编译 SQLite/OpenSSL） |
| I/O 性能 | SQLCipher 下降 5–15%；剪贴板频率下无感 |
| 改动面 | 只换存储层，采集/判重策略/UI 不动 |
| 主要风险 | 迁移期间的数据安全；必须有回滚与备份 |
| 判断标准 | 若近期要加「标签 / 回收站 / 全文搜索 / 多端同步」，现在上 SQLite 最划算；若没有这些计划，仅为消除已修好的 bug 而重构，性价比不高 |

## 八、触发条件（什么时候真的开工）

满足任意两条再启动：

1. 需要「标签 / 回收站 / 全文搜索 / 多端同步」中的任意一项
2. 历史条数常态超过 5 万，内存索引（当前约 612 字节/条）开始吃内存
3. 存储层再出现一次「数据一致性问题」（顺序 / 墓碑 / WAL 类）
4. 准备发大版本，有完整的测试窗口
