# 贡献指南

本仓库采用 **GitHub Flow（弹性版，无 CI）**。核心只有一句话：**任何改动都从独立分支出发，经本人在 Win10 / Win11 两台机器实测通过、并明确说「合并」之后，才合入 `main`。**

> 为什么写这份文件：分支纪律此前只存在于开发者本机的对话记忆里，换一台电脑就丢了。把规矩放进仓库，任何一台机器 clone 下来照做即可。

---

## 一、分支模型

| 分支 | 用途 |
|---|---|
| `main` | 稳定可发布，**只经 PR 合入**，任何情况下不直接提交 / 不直接推送 |
| `develop` | 集成分支，日常开发汇总；`main` 合入后快进到同一提交 |
| `feature/*` / `fix/*` / `docs/*` / `chore/*` | 每个改动单独开一支，**绝不在 `main` / `develop` 上直接堆改动** |
| `legacy/electron-v1` | v1（Electron）归档，不再演进，不要删除（重新打 tag 前的唯一副本） |

分支命名：

```
feature/<短描述>    新功能        feature/about-page
fix/<短描述>        缺陷修复      fix/singleton-window-shrink
docs/<短描述>       文档          docs/v2.1.8-sync
chore/<短描述>      构建/杂项     chore/bump-version
```

## 二、开发 → 合并的完整流程

1. **从 `develop` 开分支**：`git checkout develop && git pull && git checkout -b feature/xxx`
2. **开发**：改动 + 提交（提交信息见下）。
3. **本地构建自测**：只出 exe 用 `npm run tauri build -- --no-bundle`；出安装包用 `npm run tauri build`。
   - ⚠️ **绝不要用 `cargo build --release` 出交付物**——Tauri 的 `custom-protocol` feature 只有 CLI 会加，直接 cargo build 得到的是 **dev 版**（运行时去连 `localhost:5173`）。
4. **推送分支**：`git push -u origin feature/xxx`。
5. **开 PR**：`gh pr create --base main --head feature/xxx`，描述写清「改了啥 / 为什么」。
6. **等实测**：由本人（项目 owner）在 Win10 / Win11 上实测。
7. **合并**：仅在本人明确发出**「合并」**指令后执行：

   ```bash
   gh pr merge <n> --merge        # 等价 --no-ff，保留合并节点
   git checkout main && git pull --ff-only origin main
   git checkout develop && git merge --ff-only main && git push origin develop
   ```

**禁止**

- 禁止直接向 `main` 提交 / 推送（本仓库为私有 + 免费账号，GitHub 分支保护不可用，API 返回 403，靠纪律保证）。
- 禁止 squash 合并（丢历史、不利恢复）。
- 禁止在未获「合并」指令前自行合并。

## 三、可回退性（长期）

每个功能都保留一段 `--no-ff` 合并提交，将来可整段撤销：

```bash
git revert -m 1 <合并提交 sha>   # 撤销整个功能
git checkout <tag>                # 回到某个发布版本
```

因此：**发版必打 annotated tag** —— `git tag -a vX.Y.Z -m "轻贴 vX.Y.Z"`。

## 四、提交信息

遵循 [Conventional Commits](https://www.conventionalcommits.org/)：

```
feat / fix / perf / refactor / docs / test / build / chore

feat(about): 关于页新增检查更新与更新日志
fix(effects): 圆角模式关闭后残留直角边框
docs: 补齐 v2.1.8 功能说明
```

## 五、双机开发（办公室 Win11 + 家里 Win10）

**换机器前必须先把改动 commit + push 到分支**，别留未提交改动在本地：

```bash
# 离开一台机器前
git add -A && git commit -m "wip: ..." && git push -u origin feature/xxx

# 到另一台机器
git fetch --all --prune && git checkout feature/xxx && git pull
```

仓库、依赖、工具链在两台机器上都要装好（Node、Rust、NSIS 工具链等）。

## 六、发版流程

1. `feature/*` 合入 `main`、`develop` 快进（见第二节）。
2. **补 `CHANGELOG.md`**：在本文件顶部（最新一节的位置）按 `## <版本号>` 的格式写这一版改了什么。
   它会被**编译期内嵌**进 exe —— 关于页在没有新版本时就显示这一节，所以漏写的话，
   beta / 未发布版本的用户会看到上一个正式版的说明。
   > 改完最好 `touch src-tauri/src/about.rs` 再编译，确保内嵌内容被重新读取。
3. **统一改版本号**——少改一处就会不一致，检查清单：

   ```
   package.json          "version"
   package-lock.json     顶层 "version" + packages[""].version（两处！）
   src-tauri/tauri.conf.json   "version"
   src-tauri/Cargo.toml   version
   src-tauri/Cargo.lock   [[package]] name = "self-clipboard" 的 version
   src/main.js            版本号兜底字符串（grep "Version："）
   ```

4. `git tag -a vX.Y.Z -m "..."` 并 `git push origin vX.Y.Z`。
5. `npm run tauri build` 产出 NSIS 安装包（在 `src-tauri/target/release/bundle/nsis/`）。
   - 构建前先关掉正在运行的 `self-clipboard.exe`——它会锁住编译产物（`LNK1104 / 拒绝访问`）。
6. 复制为 **ASCII 文件名**（如 `light_clip_vX.Y.Z_setup.exe`）再上传——`gh` 在 Windows 下会吞掉中文资产名。
7. `gh release create vX.Y.Z <资产> --title "..." --notes-file <notes.md>`。
   - **beta 版勾 `--prerelease`**：正式用户查的 `/releases/latest` 会按定义跳过 pre-release，
     所以 beta 不会打扰正式用户；而 beta 版自己查的是 `/releases` 列表，能看到全部（含 pre-release）。
   - 单个 Release **没有**「设为私密」这种功能（可见性只到仓库级；draft 需要 token 才读得到），
     想让正式用户看不到 beta，`--prerelease` 就是官方机制。
8. Release notes 写明 **新增 / 修复 / 已知问题**。

> 某些机器上 Tauri 自动下载 NSIS 工具链会被代理拦，需手动补到 `%LOCALAPPDATA%\tauri\NSIS\`（解压时**剥掉 `nsis-3.11/` 这一层**）。

## 七、文档

- `README.md`：面向用户——功能、安装、FAQ。GitHub 只在**默认分支 `main`** 上渲染它，因此文档改动最终要落到 `main`。
- `DESIGN.md`：面向开发——**正文（1–11 章）只写已实现**并标注对应源码文件与行号；未实现的规划统一收进「路线图」章节，**不删**。
- 每次发版都要同步更新上述两份文档。
