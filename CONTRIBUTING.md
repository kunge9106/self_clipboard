# 贡献指南

本仓库使用 GitHub Flow，不配置 CI。

合并规则：改动从独立分支出发，在办公室 Win11 与家里 Win10 两台机器实测通过，并由仓库所有者发出合并指令后，经 PR 合入 `main`。分支纪律此前只存在于开发者本机的对话记录中，换一台机器即丢失；规则写入仓库后，任意机器 clone 下来照做即可。

## 一、分支模型

| 分支 | 用途 |
|---|---|
| `main` | 稳定可发布线。只经 PR 合入，不直接提交，不直接推送 |
| `develop` | 集成线。日常开发汇总处，`main` 合入后快进到同一提交 |
| `feature/*` `fix/*` `docs/*` `chore/*` | 单个改动单独开一支，不在 `main` 与 `develop` 上直接堆改动 |
| `legacy/electron-v1` | v1 Electron 代码归档，不再演进。重新打 tag 前的唯一副本，不得删除 |

分支命名：

| 前缀 | 用途 | 示例 |
|---|---|---|
| `feature/` | 新功能 | `feature/about-page` |
| `fix/` | 缺陷修复 | `fix/singleton-window-shrink` |
| `docs/` | 文档 | `docs/v2.1.8-sync` |
| `chore/` | 构建与杂项 | `chore/bump-version` |

## 二、开发与合并流程

1. 从 `develop` 开分支：

   ```bash
   git checkout develop && git pull && git checkout -b feature/xxx
   ```

2. 开发并提交，提交信息规范见第四节。

3. 本地构建自测。只出 exe 用 `npm run tauri build -- --no-bundle`，出安装包用 `npm run tauri build`。

   不使用 `cargo build --release` 产出交付物。Tauri 的 `custom-protocol` feature 只由 Tauri CLI 注入，直接用 cargo 构建得到的是 dev 版，运行时会连接 `localhost:5173`。

4. 推送分支：`git push -u origin feature/xxx`。

5. 开 PR：`gh pr create --base main --head feature/xxx`，描述写明改动内容与原因。

6. 实测。由仓库所有者在 Win10 与 Win11 上验证。

7. 合并。仅在仓库所有者发出合并指令后执行：

   ```bash
   gh pr merge <n> --merge        # 等价 --no-ff，保留合并节点
   git checkout main && git pull --ff-only origin main
   git checkout develop && git merge --ff-only main && git push origin develop
   ```

### 禁止事项

- 直接向 `main` 提交或推送。仓库为私有且使用免费账号，GitHub 分支保护不可用，API 返回 403，规则靠纪律保证。
- squash 合并，会丢弃历史，不利于恢复。
- 在收到合并指令前自行合并。
- 删除任何分支。本地与远端、已合并与陈旧、临时分支一律适用，与内容是否已并入 `main` 无关。清理分支属破坏性操作：先逐条列出候选分支及其状态，含是否已并入 `main`、最后提交时间；取得明确同意后才可删除。默认动作是只报告、不动手。

## 三、可回退性

每个功能保留一段 `--no-ff` 合并提交，供将来整段撤销：

```bash
git revert -m 1 <合并提交 sha>   # 撤销整个功能
git checkout <tag>                # 回到某个发布版本
```

因此发版必打 annotated tag：`git tag -a vX.Y.Z -m "轻贴 vX.Y.Z"`。

## 四、提交信息

遵循 Conventional Commits，类型取 `feat` / `fix` / `perf` / `refactor` / `docs` / `test` / `build` / `chore`。

```
feat(about): 关于页新增检查更新与更新日志
fix(effects): 圆角模式关闭后残留直角边框
docs: 补齐 v2.1.8 功能说明
```

## 五、双机开发

开发机为办公室 Win11 与家里 Win10。换机器前先把改动 commit 并 push 到分支，不在本地留下未提交改动。

```bash
# 离开一台机器前
git add -A && git commit -m "wip: ..." && git push -u origin feature/xxx

# 到另一台机器
git fetch --all --prune && git checkout feature/xxx && git pull
```

两台机器都要装好仓库、依赖与工具链，含 Node、Rust、NSIS 工具链。

## 六、发版流程

1. `feature/*` 合入 `main`，`develop` 快进，见第二节。

2. 补 `CHANGELOG.md`。在 `CHANGELOG.md` 顶部即最新一节的位置，按 `## <版本号>` 格式写明本版改动，每节只用 `新增` / `修复` / `优化` / `其他` 四类小标题，无内容的类别不写，不使用自定义名目。

   `CHANGELOG.md` 在编译期内嵌进 exe。关于页在没有新版本时显示这一节，漏写会使 beta 或未发布版本的用户看到上一个正式版的说明。改写后执行 `touch src-tauri/src/about.rs` 再编译，确保内嵌内容被重新读取。

3. 改版本号：`npm run version:set X.Y.Z`。

   版本源是 `package.json` 的 `version`。该命令一次写入 `package-lock.json`（两处）、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`，不必手工逐处修改。以下两处不参与：

   | 位置 | 为什么不用改 |
   |---|---|
   | `src-tauri/tauri.conf.json` | `version` 为 `"../package.json"`，Tauri 支持的引用写法，构建与运行时都从 `package.json` 取值 |
   | `src/main.js` | 状态栏版本号取自 `getVersion()`，无硬编码 |

   `package.json` 与 `src-tauri/Cargo.toml` 无法合并为一处：npm 与 cargo 两个工具链各自必须能独立读到版本，相互不可引用。脚本会逐项校验是否匹配到自身版本号，未匹配即报错，不静默跳过。

4. 打 tag 并推送：`git tag -a vX.Y.Z -m "轻贴 vX.Y.Z"`，`git push origin vX.Y.Z`。

5. 构建安装包：`npm run tauri build`，产物在 `src-tauri/target/release/bundle/nsis/`。构建前关闭正在运行的 `self-clipboard.exe`，该进程会锁住编译产物，导致 `LNK1104 / 拒绝访问`。

6. 上传前把安装包复制为 ASCII 文件名，例如 `light_clip_vX.Y.Z_setup.exe`。`gh` 在 Windows 下会丢弃中文资产名。

7. 创建 Release：`gh release create vX.Y.Z <资产> --title "..." --notes-file <notes.md>`。

   beta 版加 `--prerelease`。正式用户查询的 `/releases/latest` 按定义跳过 pre-release，beta 不会触达正式用户；beta 版自身查询 `/releases` 列表，可看到全部版本，含 pre-release。Release 没有设为私密的功能，可见性只到仓库级，draft 需要 token 才能读取；让正式用户看不到 beta 的官方机制就是 `--prerelease`。

8. Release notes 写明新增、修复、已知问题。

部分机器上 Tauri 自动下载 NSIS 工具链会被代理拦截，需手动放入 `%LOCALAPPDATA%\tauri\NSIS\`，解压时剥掉 `nsis-3.11/` 这一层目录。

## 七、文档

| 文件 | 读者 | 要求 |
|---|---|---|
| `README.md` | 用户 | 功能、安装、FAQ。GitHub 只在默认分支 `main` 上渲染，文档改动最终要落到 `main` |
| `DESIGN.md` | 开发 | 正文 1–11 章只写已实现内容，标注对应源码文件与行号；未实现的规划统一收进「路线图」章节，不删 |

每次发版同步更新上述两份文档。

## 八、构建产物与临时文件

项目自己生成、且确定无用的文件（构建产物、增量编译缓存、测试中间文件、一次性脚本），**直接删除，不要丢回收站**。

`src-tauri/target` 会被反复重建，一次全量就是几 GB。删进回收站等于把空间从 `target` 挪到回收站，磁盘一点没腾出来 —— 实测回收站里积攒过 90G 的旧构建产物，把整块 D 盘占满。

```bash
# 直接删（推荐）：不影响任何源码与已构建好的产物
rm -rf src-tauri/target/debug/incremental
# 或者整体清掉
cargo clean
```

判断标准只有一条：**这个文件能不能从源码或工具重新生成？**

- 能 → 直接删，不必问。
- 不能（仓库里已跟踪的文件、`D:/Desktop` / `下载` 等处的个人资料）→ 仍然先扫描、出报告、等确认，绝不自己动手。
