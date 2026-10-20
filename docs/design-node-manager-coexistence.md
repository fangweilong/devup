# DevUp × Node 版本管理器共存设计

> 状态：设计待评审（未开始编码）
> 范围：`nodejs` 组件的检测 / 安装 / 卸载，以及依赖 node 的 Agent 全局安装路径
> 关联代码：`src/detector/env_check.rs`、`src/installer/mod.rs`、`src/installer/macos.rs`、`src/installer/npm.rs`、`src/platform.rs`

## 0. 结论摘要

**问题**：用户本地装了 nvm / fnm / volta / mise / n / asdf 这类 Node 版本管理器时，DevUp 的「更新 Node.js」会破坏该管理器自己的版本管理语义（Windows 上写穿 nvm 的 symlink，macOS 上凭空多出并行 runtime 并把 Agent 装进错误前缀）。

**根因**：现有实现隐含假设「一台机器的 node 只有一个，且归系统安装器所有」。manager 用户的现实是「node 有多个，且归 manager 所有」。检测和写入两侧都按前者实现，必然互踩。

**方案**：把假设换成**所有权驱动（ownership-driven）**——

> **谁拥有当前生效的 node，更新就走谁的路径；拿不到所有权就不写；写完必须回验真实生效版本。**

**与用户诉求的对齐**：DevUp 仍然能「一键直接安装 / 更新 Node」，只是**执行者随所有权切换**——有 manager 时由 DevUp 代跑 manager 的 `install`/`set default`（结果仍然是自动装好，不需要用户去终端敲命令），无 manager 时保持现有 MSI / PKG / Homebrew 链路。唯一被取消的是「往别人托管的目录里写」这一种行为。

**推荐模式**：分流（`node.mode = "auto"`），而非全局只读、也非全局改装私有目录。理由见 §5。

---

## 1. 现状：三条具体的破坏路径

### 1.1 macOS — `child_path()` 把 Homebrew 前置到用户 PATH 之前

`src/platform.rs:143-166`：Unix 分支把 `extra_search_dirs()`（`/opt/homebrew/bin`、`/usr/local/bin`）拼在**原 PATH 之前**，并且 `refresh_process_path()`（`platform.rs:136-139`）会把它 `set_var` 回当前进程。

后果链：

| 现象 | 机制 |
|---|---|
| 检测结果不是用户终端里那个 node | `Detector::check_command` → `platform::which()`（`platform.rs:169-175`）在已被改写的 PATH 上查找，永远先命中 brew/pkg 的 node，即使用户 shell 里 nvm 排在最前 |
| Agent 装到了用户终端找不到的位置 | `NpmInstaller::wrap_command("npm")`（`npm.rs:17-25`）同样命中 brew 的 npm → `npm -g` 写进 brew 前缀，nvm/volta 的 `~/.volta/bin` 里不会出现 `claude` |
| 「装了但没装」的假阴性 | 只有 nvm、没有 brew node 的机器 → 探测为未安装 → 点击安装执行 `brew_install_or_upgrade("node")`（`installer/mod.rs:549-551`）→ 凭空增加并行 runtime，此后谁生效由 PATH 顺序决定 |

注：Windows 侧的 PATH 构造（`platform.rs:76-134`）顺序是 注册表系统 PATH → 用户 PATH → 高频目录 → 当前进程 PATH，补充目录本就在用户目录之后。Unix 侧不能简单照抄这个「追加」语义——Finder 启动时用户 PATH 恰好只有系统目录，纯追加会让 `/usr/bin/git` 遮蔽 brew 的 git。最终实现见 `docs/plan-m0-node-guardrails.md` §2.1。

### 1.2 Windows — MSI 直接写穿 nvm-windows 的 symlink

nvm-windows 用 `C:\Program Files\nodejs` 作为**指向 `C:\Users\<u>\AppData\Roaming\nvm\v<ver>` 的 symlink/junction**，`nvm use` 的本质就是改这个链接。

`src/installer/mod.rs:343-358` 无条件执行 `msiexec /i node-<ver>-x64.msi /quiet`，而该 MSI 的默认目标目录正是 `C:\Program Files\nodejs`。三种可能结果**全都是破坏**：

1. 透过链接写进 nvm 托管的版本目录 → 污染该版本内容；
2. 把 junction 替换成实体目录 → `nvm use` 从此失效，nvm 的 current 语义被抹掉；
3. 安装失败并残留注册表项 / 半成品目录。

而检测侧（`env_check.rs:112-189` 的 `where.exe node.exe`，加上 `extra_search_dirs()` 里的 `C:\Program Files\nodejs`，`platform.rs:51`）拿到的版本号本来就是 nvm 当前版本——即 DevUp 在明知目录被 nvm 托管的情况下仍向其写入。

### 1.3 macOS — 官方 PKG 覆盖 `n` 的地盘，卸载链路反向误删

- 官方 PKG 落点是 `/usr/local/bin/node` + `/usr/local/lib/node_modules`，恰好是 `n` 的默认 `N_PREFIX`。装 PKG = 把 `n` 的当前版本二进制原地换掉，`n ls` 与现实不再对应。
- 反向：`MacInstaller::remove_pkg_node_files`（`macos.rs:161-164`）无条件 `rm -rf /usr/local/bin/node /usr/local/lib/node_modules`，不校验这些文件是否真的由 PKG 装入。若那里其实是 `n` 或源码编译产物，DevUp 会把它删掉。
- Windows 卸载（`installer/mod.rs:459-468`）用 `Get-Package -Name 'Node.js*' | Uninstall-Package -Force` 名称通配，同样不看 `InstallLocation` 归属。

### 1.4 本机实测证据

```
NVM_DIR=/Users/rainbow/.nvm      ~/.nvm 与 ~/.volta 同时存在
which -a node:
  /Users/rainbow/.nvm/versions/node/v26.12.0/bin/node   ← 当前生效
  /Users/rainbow/.volta/bin/node
  /Users/rainbow/.nvm/versions/node/v24.22.0/bin/node   ← PATH 里残留的旧版本目录
  /Users/rainbow/.nvm/versions/node/v16.20.2/bin/node
node -v → v26.12.0
```

多 manager 共存 + 单 manager 多版本目录同时挂在 PATH 上，是最真实的场景；任何「找第一个 node」的实现到这里都会给出用户看不懂的结论。

---

## 2. 所有权模型与术语

```
NodeOwner  ── 当前「生效的 node」归谁
  ├ None            机器上没有可用 node
  ├ SystemPkg       macOS: /usr/local/bin/node 且有 org.nodejs.node.pkg receipt
  │                 Windows: C:\Program Files\nodejs 是实体目录
  ├ Brew            node 来自 Homebrew Cellar
  ├ Manager(kind)   nvm / nvm-windows / fnm / volta / mise / asdf / proto / n / scoop / nvs
  └ Unknown         找到了 node 但归属判定不了（保守对待，等同 Manager 处理）

NodeEnv    ── 一次探测的完整结论（供决策、UI、回验共用）
  ├ owner            NodeOwner
  ├ active_node      生效 node 的 canonical 路径
  ├ version          node -v 的实际值
  ├ managers[]       机器上所有已安装的 manager（可能多个，各自 root 路径）
  ├ npm_prefix       npm config get prefix 的结果
  ├ prefix_on_path   该 prefix 的 bin 目录是否出现在（用户 shell 的）PATH 上
  └ conflicts[]      冲突清单：多 runtime / prefix 不在 PATH / 链接被替换 / manager 互抢

ActionKind ── 一次更新动作最终选择谁作为执行者
  ├ ViaManager(kind)   代跑 manager 自己的命令
  ├ ViaSystem()        MSI / PKG / brew（仅在 owner 为 SystemPkg/Brew/None 时允许）
  ├ ViaDevUpPrivate()  装到 DevUp 自有目录（兜底，永不共享系统路径）
  └ ReportOnly         只给命令与说明，不执行
```

设计约束：**`owner` 描述「谁拥有当前生效的 node」，`managers` 描述「这台机器上装过哪些 manager」**。两者不等价——装了 nvm 但当前生效的是 brew node 的机器很常见，这正是必须让用户选一次而不能猜的原因。

---

## 3. 检测规则（`src/detector/node_source.rs`，新文件）

GUI 进程（Finder / LaunchServices 启动）拿不到用户 shell 的环境变量，因此必须四路交叉。任何单一路径都不足以下结论。

### 3.1 四路信号

1. **环境变量**（终端启动 DevUp 时最可靠）
   `NVM_DIR`、`NVM_SYMLINK`、`FNM_DIR`、`FNM_MULTISHELL_PATH`、`VOLTA_HOME`、`MISE_DATA_DIR`、`MISE_CONFIG_DIR`、`ASDF_DATA_DIR`、`PROTO_HOME`、`N_PREFIX`、`SCOOPT_HOME`。

2. **解析 shell 配置文件**（Finder 启动时唯一能还原真实 PATH 顺序的手段）
   扫 `~/.zprofile`、`~/.zshrc`、`~/.bashrc`、`~/.profile`，正则提取：
   - `export NVM_DIR=...` / `nvm.sh`
   - `export VOLTA_HOME=...` / `~/.volta/bin`
   - `eval "$(fnm shell)"` / `fnm env` / `FNM_DIR`
   - `mise activate` / `~/.local/share/mise`
   - `N_PREFIX=`
   与 VS Code 的 shell 环境解析同思路。**只做只读文本解析，绝不 source 用户 rc**（避免任意代码执行与副作用）。

3. **已知目录探测**
   `~/.nvm`、`~/AppData/Roaming/nvm`、`~/.volta`、`~/.fnm`、`~/.local/share/mise`、`~/.asdf`、`~/.proto`、`~/scoop/apps/nodejs`、`~/.nvm/versions/node/*`。

4. **路径归属判定（决定性一步）**
   - 对解析到的 `node` 做 `std::fs::canonicalize`（穿透 symlink/junction），与上述 manager 根目录做前缀比对 → 识破 `C:\Program Files\nodejs → ...\nvm\v24.22.0`。
   - Windows reparse point 检查（优先零新依赖）：`std::fs::canonicalize(r"C:\Program Files\nodejs")` 与自身路径归一化后比对，不相等即为 reparse point（junction 与 symlink 都会被解析到目标）；或用 `std::fs::symlink_metadata().file_type().is_symlink()`。若将来需精确区分 symlink / junction / 其他 reparse tag，才引入 `GetFileAttributesW` + `FSCTL_GET_REPARSE_POINT`（需新增 `windows` crate 依赖，当前工程只有 `winreg`）。
   - macOS PKG 归属：`pkgutil --pkgs | grep org.nodejs.node.pkg` 才认定 `/usr/local` 那份是 PKG 装的。
   - 问 manager 本人（只读命令）：`nvm version` / `nvm list`、`fnm list`、`volta list node`、`mise ls node`、`asdf where nodejs`。macOS nvm 是 shell function，需 `zsh -c 'export NVM_DIR=~/.nvm; . "$NVM_DIR/nvm.sh" --no-use; nvm list'`（`--no-use` 关键：不改变当前版本）。

### 3.2 manager 识别与驱动特征表

| manager | 平台 | 生效位置 | 是否可被 GUI 直接驱动 | 提权 | 说明 |
|---|---|---|---|---|---|
| nvm-windows | Win | `%APPDATA%\nvm` + `C:\Program Files\nodejs` 链接 | ✅ `nvm.exe install/use` | `use` 可能需管理员（写 Program Files 下的链接） | 最易出事，护栏优先 |
| nvm | mac/linux | `~/.nvm/versions/node/<v>/bin` | ⚠️ shell function，需 `zsh -c 'source nvm.sh --no-use; nvm ...'` | 否 | 可驱动，但必须验证 rc 干扰，见 §10 R2 |
| fnm | 双平台 | `~/Library/Application Support/fnm` / `FNM_DIR`，`FNM_MULTISHELL_PATH` | ✅ 真二进制 | 否 | 语义上「按 shell 激活」，无全局 current，需说明 |
| volta | 双平台 | `~/.volta`（shim + hooks） | ✅ `volta install node@<v>` | 否 | 有 per-project pin，别手改 `package.json` |
| mise / rtx | 双平台 | `~/.local/share/mise` + `~/.local/bin` shim | ✅ `mise install/set -g` | 否 | 与 asdf 数据可迁移 |
| asdf | mac/linux | `~/.asdf` + shims | ✅ `asdf install/global` | 否 | 需 `.tool-versions` 语境 |
| proto | mac/linux | `~/.proto` | ✅ `proto install/set node` | 否 | |
| n | mac/linux | `N_PREFIX`，默认 `/usr/local` | ✅ 真二进制 | 默认前缀需 sudo | **与官方 PKG 同地盘**，§1.3 |
| scoop / nvs | Win | `~/scoop/apps/nodejs` / `%APPDATA%\nvs` | ✅ | 否 | |

### 3.3 冲突判定（`conflicts`）

- PATH 上出现 **≥2 个** node 目录（本机等即此例）；
- `npm_prefix` 的 bin 目录不在用户 shell PATH 上 → 装完 Agent 命令找不到；
- `npm_prefix` 落在 manager 的**版本目录内**（nvm 的典型形态）→ 切版本会丢全局包；
- `C:\Program Files\nodejs` 是 reparse point 但注册表里存在 Node.js MSI 安装项 → 状态已被上一次 MSI 搞坏，需要修复指引（§9.3）。

---

## 4. 护栏：写入禁区（硬约束，落在 `InstallerEngine` 入口）

必须在 `install_target_windows` / `install_target_macos` 的 `"nodejs"` 分支**开头**做 preflight，命中即 `bail!` 成人话，**不依赖 UI 置灰**（未来加 CLI 入口或批量更新时 UI 拦不住）。

| 编号 | 禁区 | 判定 |
|---|---|---|
| G1 | 不向 `NVM_SYMLINK` / reparse point 的 `C:\Program Files\nodejs` 执行 MSI | `FILE_ATTRIBUTE_REPARSE_POINT`；或 `NVM_SYMLINK` 环境变量与该路径相等 |
| G2 | 不向任何 manager 数据目录写 runtime（`~/.nvm/**`、`~/.volta/**`、`~/.local/share/mise/**`、`~/.asdf/**`、`~/.proto/**`、`%APPDATA%\nvm/**`、`~/scoop/apps/nodejs/**`） | canonical 路径前缀比对 |
| G3 | 检测到可驱动 manager 时不执行 `brew install node` / PKG | `managers` 非空且 `node.mode != "system"` |
| G4 | 检测到 `n`（或 `N_PREFIX=/usr/local`）时不执行官方 PKG 安装 | rc 解析 + 目录探测 |
| G5 | 卸载 PKG 前必须确认 receipt 存在且路径归属正确；否则只提示、不 `rm` | `pkgutil --pkgs` |
| G6 | Windows 卸载按 `InstallLocation` 过滤，名称通配不得命中 manager 目录 | `Get-Package` 的 `InstallLocation` |
| G7 | 任何模式都不得静默修改用户 shell rc / 注册表 PATH / `~/.npmrc`；需要修改的一律显式确认并记录可撤销 | 见 §6.3、§8.3 |

违反 G1 是当前 Windows 事故的直接来源；G3/G4 是 macOS 并行 runtime 的来源。**G1–G6 是纯安全收益，可以先于任何新功能上线。**

---

## 5. 决策矩阵：`node.mode`（新增配置，默认 `auto`）

| mode | 行为 | 何时用 |
|---|---|---|
| `auto`（默认） | 按 owner 分流，见下表 | 面向绝大多数用户，保持「一键就能装/更新」 |
| `manager` | 强制代跑 manager；驱动不了则报错 | 用户明确只想走自己的 nvm |
| `system` | 保留现状（MSI/PKG/brew） | 无 manager 的机器；有 manager 时 UI 需二次确认并标警告 |
| `private` | 强制装 DevUp 私有 runtime（§6） | 不想让 DevUp 碰任何已有环境 |
| `readonly` | 只出命令与指引，不执行 | 极保守 / 排障 |
| `off` | 卡片不显示 nodejs | 不需要 |

### `auto` 的分流规则（按顺序命中）

| # | 条件 | 动作 | 用户感知 |
|---|---|---|---|
| 1 | `owner = Manager(m)` 且 m 可驱动 | `ViaManager(m)`：`m install <ver>`（+ 按用户选择决定是否设默认） | 一键自动装好，仍是他的 manager 语义 |
| 2 | `owner = Manager(m)`，m 不可驱动（保守判定，见 R2） | `ReportOnly` + 一键复制；或经确认后 `ViaDevUpPrivate()` | 不猜、不写 |
| 3 | `owner = Brew` 且无 manager | `brew upgrade node`（现状） | 同现状 |
| 4 | `owner = SystemPkg` 且无 manager | MSI / PKG 升级（现状） | 同现状 |
| 5 | `owner = None` 且无 manager | 按平台首选渠道安装（现状：mac 优先 brew，Win 用 MSI） | 同现状 |
| 6 | `owner = None` 但有 manager 痕迹（rc 里 source 了 nvm，但没装任何版本） | `ViaManager(m) install <ver>` | 补齐他 manager 的第一个版本，最符合预期 |
| 7 | `owner = Unknown`，或 manager 与系统安装**同时存在且互相遮蔽** | 弹窗展示两份 runtime（路径/版本/prefix），默认选中生效那份，让用户选一次并持久化 | 不替用户猜 |

规则 7 是必须存在的：本机 nvm+volta 共存就属于这类。默认选项是「当前生效那份」，并明写「另一份 DevUp 不会动」。

### 「装新版本」与「改当前生效版本」必须分离

`install` 只新增，`alias default` / `nvm use` / `fnm default` / `mise set -g` / `volta install`（这个会改）才影响用户日常手感。UI 拆成两个动作：

- **安装 LTS vX**（默认按钮，安全）
- **设为默认版本**（次级按钮，二次确认，文案写明「你终端里 `node -v` 会变成 vX」）

Windows `nvm use` 还会改 reparse point（可能需提权），单独标注。

---

## 6. `ViaDevUpPrivate`：私有 runtime 兜底（新目录，互不干扰）

适用：owner 为 `Unknown`、manager 不可驱动、或用户选 `private`。价值在于**永远不会写进别人的地盘**。

### 6.1 目录布局

```
macOS   ~/.devup/node/v24.22.0/bin/node
Windows %LOCALAPPDATA%\devup\node\v24.22.0\node.exe
        同时提供 ~/.devup/node/current 联结（Win 用 junction，mac 用 symlink）便于 PATH 只写一条
```

下载源沿用现有 `NODEJS_MIRROR_BASE`（`src/network/mirrors.rs:72`），已实测可用：

```
https://npmmirror.com/mirrors/node/v24.22.0/node-v24.22.0-darwin-arm64.tar.xz   200
https://npmmirror.com/mirrors/node/v24.22.0/node-v24.22.0-win-x64.zip            200
https://npmmirror.com/mirrors/node/v24.22.0/SHASUMS256.txt                       200
```

**必须校验 SHASUMS256**（对目标文件名那行），校验失败即中止删除临时文件——镜像内容不可信是既定前提。解压到临时目录 → 校验 → 原子 rename 进版本目录。

### 6.2 Agent 安装配合

`npm install -g` 时强制 `--prefix ~/.devup/node/<ver>` 或直接用该目录的 npm（其默认 prefix 就在自己目录内），保证 Agent 落在同一棵树下，不会漂到系统 `/usr/local`。

### 6.3 PATH 接入

- **不自动写 PATH / 不改 rc**（G7）。
- 安装后提供明确的下一步：展示要加的 `export PATH="$HOME/.devup/node/current/bin:$PATH"` 片段（一键复制），或经确认后写入 **DevUp 自有文件** `~/.devup/env` 并由用户 source 一行。
- Windows 写用户 PATH（`HKCU\Environment`）属于可逆改动，但仍需显式勾选 + 提供「撤销」按钮；写入前备份原值到 `~/.devup/path-backup.json`。

### 6.4 卸载

只删 `~/.devup/node/**`，并撤销自己写过的 PATH。绝不碰别处。

---

## 7. 事后回验：把「计划」与「生效」分开报告

当前最伤信任的不是装失败，而是**报成功而用户的 `node -v` 没变**。契约：

安装/升级结束后重新执行一次 `NodeEnv` 探测，向 UI 推 `Verify` 事件：

| 字段 | 含义 |
|---|---|
| planned | DevUp 计划安装的版本与目标路径 |
| effective | 重新解析到的 `node -v` 与 canonical 路径 |
| match | 三者是否一致（版本一致 + 路径落在预期 owner 内） |
| note | 不一致时的人话解释 |

不一致时的展示规则（示例文案）：

- 「v24.22.0 已通过 nvm 安装到 `~/.nvm/versions/node/v24.22.0`，但你的终端当前仍是 v22.7.0 —— nvm 默认版本未切换。点「设为默认」可切换。」→ 卡片 badge 显示 **需切换**，**不显示「已是最新」**。
- 「已安装，但当前生效的仍是 Homebrew 的 /opt/homebrew/bin/node（PATH 顺序）」→ 显示冲突横幅 + 两份路径。

这条同时修正 `hasNewerVersion`（`ui/app.js:902`、`ui/app.js:984`）的判断口径：比较对象必须是 `effective.version`，而不是安装动作的目标版本。

---

## 8. Agent 安装链路与 node 的耦合（同一 bug 的另一半）

现状：`install_agent`（`installer/mod.rs:150-187`）→ `NpmInstaller::install_global`（`npm.rs:65-117`）用 PATH 上第一个 `npm`，且不校验 prefix。

改造：

1. **用检测到的那份 runtime 的 npm 绝对路径**（`NodeEnv.active_node` 同目录下的 `npm`），保证 node/npm/prefix 同源；找不到则报「Agent 依赖的 npm 与生效的 node 不匹配」而不是随便抓一个。
2. **装前校验 `prefix_on_path`**：为 false 时不装，先告知「装完终端里不会有 `claude`，因为 `<prefix>/bin` 不在你的 PATH」。
3. **prefix 落在版本目录内时警告**（nvm 形态）：「切换 node 版本后需 `nvm reinstall-packages` 重装全局包」；并提供两个可选修法：`nvm reinstall-packages <旧版本>`、或设固定 prefix `~/.npm-global`（属用户配置改动，显式同意、可撤销）。
4. **volta 用户走 `volta install <pkg>`** 而不是 `npm -g`（这样才有全局 shim，也符合 volta 语义）。
5. **`uninstall_agent`（`npm.rs:161-184`）与 `install_global` 必须使用同一份 npm**，否则删错地方，留下幽灵安装。
6. `cleanup_native_artifacts`（`installer/mod.rs:217-269`）删 `~/.local/bin/<bin>` 前确认它不是 manager/其他工具的软链。

---

## 9. UI 与数据契约变更

### 9.1 `SoftwareStatus` 增字段（`detector/env_check.rs:18-30`）——**按 D4，`managed_by` 提前进 M0**

```rust
pub managed_by: Option<String>,     // "nvm" | "volta" | "brew" | "msi" | "pkg" | "devup"
pub node_env: Option<NodeEnvView>,  // 仅 nodejs / agent 卡片使用（active_path, npm_prefix, prefix_on_path）
pub warnings: Vec<String>,          // 冲突说明，前端渲染黄色/红色横幅
```

`install_method`（现有字段）的取值口径要一并收紧：`env_check.rs:231-259` 目前用字符串包含 `nvm` 判定为 `"npm"`，这会把「nvm 装的 node」和「npm 全局包」混为一谈。改为直接取 `managed_by`。

### 9.2 IPC / 配置

- `IpcCommand::SetNodeMode { mode: String }`（`state.rs:8`）；或并入现有 `SetNetworkConfig` 式的通用 `SaveConfig`。
- `AppConfig` 新增：
  ```rust
  pub struct NodeConfig { pub mode: String /* auto|manager|system|private|readonly|off */,
                          pub preferred_manager: Option<String>, pub install_as_default: bool }
  ```
  `#[serde(default)]` 保证老 `config.json` 平滑升级（与 `config.rs:54-59` 现有测试风格一致，补一条缺字段回退单测）。
- 新事件 `AppEvent::NodeEnv(NodeEnvView)`、`AppEvent::Verify { planned, effective, match_, note }`。

### 9.3 确认弹窗（`ui/app.js` 的 nodejs 分支在 606-624 行）

现有弹窗已经把「将要执行的命令」列给用户，这个形态保留并强化：manager 场景下 commands 换成 `nvm install 24.22.0` 等真实 manager 命令，并固定追加一行红字说明**不会写入哪里**（例：`不会向 C:\Program Files\nodejs（nvm 托管）写入任何文件`）。

另外新增**修复指引**：判定为「reparse point 已被实体目录替换」时，给出恢复步骤（卸载 MSI → 重装 nvm-windows → `nvm use <v>`），只读展示、不代做。

---

## 10. 里程碑与验收

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| **M0 止血**（不含新功能） | ① ✅ `platform.rs` Unix PATH 合并顺序（已完成，实现细节与一处原判断修正见 plan §2.1）；② 新增最小 manager 探测（env + 已知目录 + canonicalize + Win reparse 判定）；③ 护栏 G1/G2/G3/G4；④ 卸载 G5/G6 | Windows：有 nvm-windows 时点更新 → **拒绝执行 MSI 并给出原因**，`C:\Program Files\nodejs` 的链接属性不变；macOS：装 volta/nvm 的机器点更新 → 不再 `brew install node`；`cargo test` 通过；`dump_detect_all -- --ignored` 输出 `managed_by` |
| **M1 回验** | §7 的 `Verify` 事件 + 卡片 badge 口径改为 effective | 只 install 不改默认的场景，UI 明确显示「已装 vX / 当前仍 vY」，不再出现「已是最新」 |
| **M2 manager 路由** | `auto` 分流 + 装/设为默认分离：先 fnm / volta / mise / asdf / proto / nvm-windows（都是真二进制单条命令），后补 macOS nvm 的 shell 包装 | 本机（nvm+volta 共存）命中规则 7 的弹窗；用 volta 的机器能一键 `volta install node@24` 且 `node -v` 变化符合预期 |
| **M3 Agent 前缀统一** | §8 全部 6 条 | 三种 owner（nvm / volta / brew）下装 `claude`，装完在**用户终端**里直接 `claude -v` 可用 |
| **M4 私有 runtime + 冲突 UI** | §6 + §3.3 conflicts 横幅 + `node.mode` 设置面板 | 无任何 manager 的干净 VM 上选 `private`，装完不产生 `/usr/local` 或 `Program Files` 写入，且能一键撤销 |

M0/M1 是纯安全与诚实性收益，改动小、可独立发布；M2 之后才引入新行为。

---

## 11. 测试矩阵（每个里程碑都要过）

| # | 机器形态 | 关键断言 |
|---|---|---|
| T1 | macOS，只有 nvm（多版本） | 检测出 `managed_by=nvm`；不触发 brew/PKG；能代跑 `nvm install` |
| T2 | macOS，**nvm + volta 共存（本机）** | 命中规则 7 弹窗，默认选中 PATH 上生效那份；另一份被明写「不会动」 |
| T3 | macOS，只有 brew node | 保持现状 `brew upgrade node`，成功且回验 match |
| T4 | macOS，装了 `n`（前缀 /usr/local） | G4 拦住 PKG 安装 |
| T5 | macOS，全新、无 node 无 manager | 规则 3/5 走通（brew 有则 brew，否则 PKG） |
| T6 | Windows，nvm-windows 且 nodejs 为 junction | G1 拦截；junction 未被替换；无残留注册表项 |
| T7 | Windows，裸 MSI 装的 node | 保持现状升级链路 |
| T8 | Windows，fnm / scoop / nvs | 走 `ViaManager`，不写 Program Files |
| T9 | 任一平台，prefix 不在 PATH | Agent 装前拦住并说明 |
| T10 | 离线 | `default_latest_version`（`env_check.rs:279`）兜底时仍不违反护栏 |

单测覆盖：`NodeOwner` 判定（给定合成路径集合）、rc 文件解析（给定 fixture，断言只读不执行）、SHASUMS 校验（含校验失败）、`config.json` 缺字段回退。

---

## 12. 风险与未决

| 编号 | 风险 | 处置 |
|---|---|---|
| R1 | 改 Unix PATH 合并顺序，影响面是**所有**子进程调用（brew / npm / claude / omp） | 需回归 M0 全链路。**M0 步 1 实现时发现本文原判断有误**：纯追加会让 Finder 启动场景下 `/usr/bin/git`（CLT）遮蔽 brew 的 git，把 Git 卡片从「2.56 · brew」误报成「2.39 · system」。最终规则改为「用户顺序原样保留，补充目录插在系统兜底目录之前」，两个坑一起治；详见 `docs/plan-m0-node-guardrails.md` §2.1 |
| R2 | macOS nvm 是 shell function，`zsh -c 'source nvm.sh'` 可能受用户 rc 干扰（rc 里有 `nvm use`、`cd`、交互式提示时行为漂移） | 用 `--no-use` + `--Command`（非交互）+ 显式 `NVM_DIR`；T1 实测不过则 macOS nvm 降级为 `ReportOnly`，不影响其余 manager 交付 |
| R3 | 解析用户 shell rc 属于读取隐私文件 | 只读、不 source、不外传；解析结果仅取路径；文档与 UI 明示 |
| R4 | `volta install` 会改当前项目的 pinned 语义 | 只在无 `package.json` 上下文（GUI 环境）执行 per-project 之外的安装，UI 说明差异 |
| R5 | Windows `nvm use` 需要管理员权限，会打断「一键」体验 | 单独按钮 + 提权说明，失败回滚到「仅 install 不 use」 |
| R6 | DevUp 私有 runtime 引入「第三个 node」，反而加剧用户 PATH 混乱 | 默认不写 PATH（§6.3），只有用户显式勾选才接入，并提供撤销；`private` 不是默认模式 |
| R7 | 多 manager 并存时用户每次都弹窗很烦 | 选择结果持久化到 `preferred_manager`，并提供「改为始终用 X」入口 |
| R8 | 尚未在 Windows 真机验证 MSI 遇 junction 的三种具体表现 | M0 的护栏不依赖是哪一种（一律拒写），但 T6 的断言细节需在 Windows 环境补测 |

**需要产品判断的两点**（本文档给出默认值，评审时可改）：

1. 装完是否**自动设为默认版本**？默认取 `install_as_default = false`（只装不改手感），Windows 与 nvm 尤其保守。
2. `auto` 在「manager 与系统安装共存」时是否允许 DevUp 自动修 PATH 顺序？默认 **不允许**（G7），只提示。

---

## 13. 评审结论（2026-10-20 已定）

| # | 决策 | 理由 | 已接受的代价 |
|---|---|---|---|
| **D1** | `node.mode = "system"` 逃生口只进 `config.json`，M0 不做 UI | 护栏误判的代价是「少一个功能」，UI 一键绕过的代价是「用户把 nvm 目录写坏然后怪 DevUp」；愿意手改配置文件的人才会去读那行警告 | 真误判时需要文档教人手改配置 |
| **D2** | `owner = Unknown` 只拦卸载、不拦安装 | 判据是「错的代价是否可逆」，不是「哪个更常见」。误拦安装用户看得见原因也绕得开；误删不可逆且用户不知道自己少了什么 | Unknown 用户装完可能仍撞 PATH 遮蔽，由 M1 回验如实报出 |
| **D3** | 护栏拒绝走**新增的非错误事件** `AppEvent::Guidance`，不走 `InstallError` | 这条提示每个 nvm/volta 用户每次点更新都会看到；渲染成红色报错会把「安全设计」教成「DevUp 会坏」 | 要动 `state.rs` + 事件分发 + 前端渲染，M0 约 +1 天 |
| **D4** | 「显示但不代管」取代「显示未安装」：卡片直接显示 `由 nvm 托管 · ~/.nvm/versions/node/v26.12.0`（版本从目录名读），但不视为可操作 runtime | 比「你装了但我说你没装」诚实得多，护栏的自辩文案压力随之消失；且用户能亲眼确认 DevUp 读到了它的环境 | `detect()` 多一次目录列举；badge 需要新状态「托管中」；`SoftwareStatus` 提前加 `managed_by`（见 §9.1） |
| **D5** | A3（装了 brew node 的 mac）列为**发版前 checklist**，不阻塞开工 | PATH 顺序改动的逻辑层已由合成 PATH 单测覆盖，缺的只是端到端确认一次 | 0.3.6 发布前需找一台 brew node / brew git 的机器点一轮 |

M0 范围据此调整：原「护栏 + 探测」扩为「护栏 + 探测 + `Guidance` 事件通路 + 卡片显示托管方 + 卸载安全化」，任务拆分与串行流水线见 `docs/plan-m0-node-guardrails.md` §7。
