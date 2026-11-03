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

**「首次编译慢 1–3 分钟」说的是开发侧**（`cargo build` 编译 SQLite/OpenSSL 源码）。
用户侧零成本：拿到编译好的 exe，不编译、不安装。用户唯一能感知的差别是
**体积 +2~4MB**，以及首次启动创建 `.db` 文件（几十毫秒）。

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

### 全文搜索必须单独建 FTS5（不能靠 `LIKE`）

```sql
-- 中文用 trigram：按 3 字窗口切分，搜「剪贴板」能命中含该词的条目，
-- 且支持子串匹配。默认的 unicode61 对中文按**单字**切，召回高、精度低。
CREATE VIRTUAL TABLE item_fts USING fts5(
  preview,
  content = '',          -- 外部内容表，正文不存在这里，只存索引
  tokenize  = 'trigram'
);

-- 用触发器与 item 保持同步（插入/更新/删除各一个）
CREATE TRIGGER item_ai AFTER INSERT ON item BEGIN
  INSERT INTO item_fts(rowid, preview) VALUES (new.rowid, new.preview);
END;
```

**为什么这条是必做项**：`LIKE '%关键词%'` 无法使用 B-tree 索引，10 万行全表扫约
80ms（用户能感觉到卡）；FTS5 的 trigram 索引是亚毫秒级。现方案的搜索是内存扫描，
数据量一大同样会慢，只是目前没到那个规模。

## 六、分阶段落地（若决定做）

**阶段 0：影子写入（不改读路径）**
新库与旧格式并行写，旧路径照常读。跑一周，比对两边条数与内容，确认无损。

**阶段 1：切换读路径**
列表/搜索/分页改走 SQL；旧的快照+WAL 保留为备份。此阶段删掉
指纹队列、预热、墓碑、`query::scan`。

**阶段 2：迁移旧数据 + 清理**
一次性导入存量（现有 666 条），校验通过后停止写旧格式。

每阶段都有独立提交与回滚开关（配置里 `storage_backend = legacy | sqlite`）。

### 验收标准（达不到就不切）

在 10 万条真实数据上量：

| 项 | 门槛 |
|---|---|
| 首屏取 60 条 | < 20ms |
| 模糊搜索（FTS5） | < 50ms |
| 启动到可交互（10 万条） | < 150ms（现方案 100–300ms） |
| 常驻内存（不含界面/WebView） | < 100MB（现方案 10 万条约 60MB 仅索引） |
| 连续复制 100 条 | 无卡顿、无漏记 |
| 删除一条后立刻复制同样内容 | 正常新增（墓碑回归） |
| 重启后复制历史里的内容 | 合并并置顶（跨重启回归） |

最后三条是这一支修过的 bug，迁移后必须重测一遍——换存储最容易把它们重新引回来。

## 七、性能基准（决定要不要做之前先看这节）

标尺：剪贴板是**人工操作**，一分钟几条，不比日志系统。这里所有数字都是「单次操作」。

### 单条操作

| 操作 | SQLite（WAL + synchronous=NORMAL，本地 SSD） | 现有自研方案 |
|---|---|---|
| 写入一条 | **0.12ms**（120 微秒） | 约 2ms（10 万条量级实测） |
| 主键点查 | 0.08ms | — |
| 范围查询 100 行 | 0.15ms | — |

SQLCipher 的加密开销是**每页 AES 解密**，相对增加 5–15%，**绝对值是几十微秒** ——
不是「ms 级下降」。真正吃 ms 的是 `fsync`（磁盘同步），两种方案都躲不掉。

### 数据量放大后

| 场景 | 现有方案（内存索引 + 窗口化渲染） | SQLite |
|---|---|---|
| 打开列表前 60 条 | 666 条≈微秒；10 万条≈1–3ms（内存线性扫描） | ≈ **0.1–0.5ms**（走 `idx_item_ts`） |
| **模糊搜索** | 内存扫描，10 万条 ≈ 5–20ms（要扫 60MB） | `LIKE '%x%'` **全表扫 ≈ 80ms** ✗ |
| 启动加载 | 解密整份 JSON + 反序列化，10 万条 ≈ **100–300ms** | 只读需要的那几页 ≈ 几十 ms |
| 常驻内存 | 612 字节/条 → 10 万条 ≈ **60MB** | 页缓存，通常几 MB |
| 清零 / 按天批量删 | 逐条 WAL 追加 | 一个事务，快一个量级 |

**两个必须记住的结论**：

1. **搜索必须上 FTS5**。SQLite 的 `LIKE '%关键词%'` 用不上普通索引，10 万行要 80ms
   （用户能感觉到卡）。FTS5 是亚毫秒级 —— 这是迁移的**必做项，不是可选项**。
2. **数据量越大，SQLite 优势越明显**（启动、内存、写入三项都是现有方案的痛点）。
   当前的 666 条两种方案都无感，瓶颈还没到。

### 需要注意的 SQLite 细节

- `synchronous=NORMAL` + WAL：崩溃后库仍一致，只可能丢最后几个未提交事务。
  对剪贴板可接受（丢一秒内的复制，不破坏历史）。
- 备份必须**三个文件一起复制**（`.db` + `-wal` + `-shm`），不能只拷主文件。
- WAL 会随写入增长，需要定期 checkpoint（`wal_autocheckpoint` 默认 1000 页，够用）。
- 单写者模型：本应用只有一个采集线程写入，不涉及并发写竞争。

## 八、风险与成本

| 项 | 量级 / 说明 |
|---|---|
| 依赖体积 | exe +2~4MB |
| 开发侧编译 | 首次 `cargo build` 慢 1–3 分钟（用户无感） |
| 改动面 | **只换存储层**，采集 / 判重策略 / UI 不动 |
| 迁移 | 现有 666 条加密 JSON → 一次性导入脚本 |
| 主要风险 | 迁移期间的数据安全；必须有备份与回滚开关 |
| 判断标准 | 若近期要加「标签 / 回收站 / 全文搜索 / 多端同步」，现在上 SQLite 最划算；若没有这些计划，仅为消除已修好的 bug 而重构，性价比不高 |

## 九、触发条件（什么时候真的开工）

满足任意两条再启动：

1. 需要「标签 / 回收站 / 全文搜索 / 多端同步」中的任意一项
2. 历史条数常态超过 5 万，内存索引（当前约 612 字节/条）开始吃内存
3. 存储层再出现一次「数据一致性问题」（顺序 / 墓碑 / WAL 类）
4. 准备发大版本，有完整的测试窗口
