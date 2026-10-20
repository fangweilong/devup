# M0 实施计划：Node 所有权护栏（止血）

> 分支：`feat/node-manager-coexistence`
> 上游设计：`docs/design-node-manager-coexistence.md`（§1 现状缺陷、§4 护栏 G1–G6、§10 M0）
> 目标版本：0.3.6（patch 发布）
> 状态：评审已定案（D1–D5 见设计文档 §13）；步 1 已完成并提交 `7ffa93d`；现按 §7 流水线串行实施 A–G

## 1. M0 做什么

只消除破坏性行为，**不新增任何安装能力**。做完后的效果：

1. macOS/Linux 子进程 PATH 不再把 Homebrew 抢在用户自己的 node 之前；
2. 向 manager 托管的目录写 runtime 的所有路径（MSI / PKG / brew 新增安装）**被硬拦**，给出原因与建议命令；
3. 卸载不再误删非 PKG 来源的 node。

做完后 DevUp **还不能**代跑 `nvm install`（那是 M2）。拦住的点击会退化为「拒绝 + 指引」，这是 M0 唯一的用户可感回归，必须在发布说明里写明。

### 非目标（明确不做）

| 不做 | 原因 |
|---|---|
| `ViaManager` 路由 / 驱动任何 manager | M2 |
| `devup_private` 私有 runtime | M4 |
| 事后回验 `Verify` 事件、badge 口径改 effective | M1 |
| Agent 的 npm prefix 统一与 `prefix_on_path` 校验 | M3 |
| 把 nvm 目录注入 GUI 进程的搜索 PATH（让 node 变成可操作 runtime） | 会连带改变 Agent 安装落点，属 M3 范畴。按 D4，M0 只把 `~/.nvm/versions/node/*` 的最新目录名**用于展示「由 nvm 托管」**，不注入 PATH、不改 `installed` 判定 |
| node 卡片的 mode 设置面板 | UI 到 M4；M0 只加 `config.json` 字段 |

---

## 2. 改动清单（按文件）

### 2.1 `src/platform.rs` — Unix PATH 合并顺序 ✅ 已完成

**现状**（`platform.rs:143-166`）：`extra_search_dirs()` 前置，用户 PATH 被挤到后面。

**改法**：拆出一个不碰环境变量的纯函数，便于单测：

```rust
/// 按「用户现有 PATH 优先、补充目录兜底」的顺序合并 PATH（纯函数，不改环境变量）
pub fn merge_path(current: &str, extras: &[PathBuf]) -> String
```

`child_path()` Unix 分支改为：

```rust
let current = std::env::var("PATH").unwrap_or_default();
let joined = merge_path(&current, &extra_search_dirs());
// SAFETY: 同现状
unsafe { std::env::set_var("PATH", &joined) };
joined
```

**实际实现与计划有一处重要偏差：`extras` 不是无脑追加到末尾，而是插在第一个系统兜底目录之前。**

计划原文写「Finder 启动时原 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`，extras 照常兜底，**不受损**」——实现时证明这句是错的：那个场景下 current 本身就等于系统目录，纯追加会让 `/usr/bin/git`（CLT）排到 `/opt/homebrew/bin/git` 之前。装了 brew git 的用户，Git 卡片会从「2.56 · brew」翻成「2.39 · system」（`env_check.rs` 的 `infer_install_method` 把 `/usr/bin` 前缀判为 `system`），版本与托管方式双双误报。因此新增：

```rust
/// Unix 系统兜底目录：登录环境自带，不代表用户主动配置的优先级
#[cfg(unix)]
const SYSTEM_PATH_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/System/Cryptexes/App/usr/bin"];
```

`merge_path` 最终规则：

- 切分 `current`：丢弃空分段（旧实现的 `parts.push(current)` 会把首尾 `:` 原样带入），重复项保留**首次出现**位置；
- `current` 的相对顺序**原样保留、不重排**（用户自己把 `/usr/bin` 写在 `~/.volta/bin` 之前，就维持这个先后）；
- 定位第一个命中 `SYSTEM_PATH_DIRS` 的分段，把 `extras` 中尚未出现的目录按序插在它之前；current 中没有系统目录时退化为追加到末尾；
- 比较为大小写敏感的精确字符串比较（与旧实现一致）。

一句话：**既不让 brew 压掉 nvm（原 bug），也不让 `/usr/bin` 压掉 brew（新坑）**。

Windows 分支（`platform.rs:76-134`）不动——它本来就是「注册表 → 用户 → extras」的追加语义。

**影响面**：`which()`（`platform.rs:214`）读的是已被 `child_path()` 写回的进程 PATH，因此以下调用者的解析结果会变（从终端启动 DevUp 时）：
`Detector::check_command_unix`、`NpmInstaller::wrap_command`、`MacInstaller::brew_available` / `brew_installed_formula` / `brew_run`。

**实现记录**（改动仅 `src/platform.rs`，+186 / −16）：

- 新增 `merge_path` + `SYSTEM_PATH_DIRS`，`child_path()` Unix 分支改用它们；
- **顺带修了一处同类缺陷**：`which()` 原来是 `env PATH` 直接 `chain(extra_search_dirs())`，即「补充目录永远排在最后」——它只是**碰巧**因为 `detect_all()` 先调了 `refresh_process_path()`（`env_check.rs:486`）才拿到正确顺序。任何在首次刷新之前发生的 `which()` 调用都会退化成 `/usr/bin` 优先。现改为与 `child_path()` 共用 `merge_path`，顺序不再依赖调用时序。该函数走真实文件系统，未写单测，由 A1/A1b 探针覆盖；
- 新增 2 个测试函数（表驱动含 9 个用例）+ 1 个 `--ignored` 人工探针，`cargo test` = 14 passed / 2 ignored；
- 仓库整体并非 rustfmt-clean（全仓 101 处既有 diff，`platform.rs` 内余下 6 处均在原有代码区），故只保证新增代码 fmt-clean，**没有跑 `cargo fmt`** 以免污染无关文件；
- Windows 目标本机无法 `cargo check`（`aws-lc-sys` 的 C 编译缺 Windows SDK 头文件，与本次改动无关），改为 cfg 审查确认 `merge_path` / `SYSTEM_PATH_DIRS` / 测试模块全在 `#[cfg(unix)]` 守卫内，Windows 分支不可见；`which()` 本就是 `#[cfg(unix)]`。Windows 侧仍待 CI 与真机验证。

### 2.2 `src/detector/node_source.rs` — 新增（M0 最小版）

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagerKind { Nvm, NvmWindows, Fnm, Volta, Mise, Asdf, Proto, N, Scoop, Nvs }

#[derive(Clone, Debug)]
pub struct Manager { pub kind: ManagerKind, pub root: PathBuf, pub from: ManagerSource }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeOwner { None, SystemPkg, Brew, Manager(ManagerKind), Unknown }

#[derive(Clone, Debug, Default)]
pub struct NodeEnv {
    pub owner: NodeOwner,
    pub managers: Vec<Manager>,
    pub active_node: Option<PathBuf>,      // canonicalize 后
    pub npm_prefix: Option<PathBuf>,
    pub nodejs_dir_is_reparse: bool,       // Win: C:\Program Files\nodejs
    pub pkg_receipt: bool,                 // mac: org.nodejs.node.pkg 已登记
    pub hints: Vec<String>,                // 拒绝时展示给用户的建议命令
}

pub enum Channel { WindowsMsi, MacPkg, MacBrewInstall }

pub struct NodeSource;
impl NodeSource {
    pub fn detect() -> NodeEnv;                       // 只读，无副作用
    pub fn assert_install_allowed(env: &NodeEnv, ch: Channel) -> Result<()>;  // 违规即 bail 人话
    pub fn assert_uninstall_allowed(env: &NodeEnv, ch: Channel) -> Result<()>;
}
```

`detect()` 的四路信号（M0 只做「是否有人在托管」，**不调用任何 manager 命令**）：

1. **环境变量**：`NVM_DIR`、`NVM_SYMLINK`、`FNM_DIR`、`FNM_MULTISHELL_PATH`、`VOLTA_HOME`、`MISE_DATA_DIR`、`ASDF_DATA_DIR`、`PROTO_HOME`、`N_PREFIX`。
2. **已知目录探测**：`~/.nvm`、`~/.fnm`、`~/Library/Application Support/fnm`、`~/.volta`、`~/.local/share/mise`、`~/.asdf`、`~/.proto`、`~/scoop/apps/nodejs`、`%APPDATA%\nvm`、`%APPDATA%\nvs`。
3. **shell 配置只读解析**（Finder 启动时唯一能识破 nvm 的手段，`env_check.rs` 现在完全没有这条）：
   ```rust
   /// 只读正则扫描，绝不 source / 执行用户 rc（隐私与副作用约束见设计文档 §12 R3）
   fn scan_shell_rc(home: &Path) -> Vec<Manager>
   ```
   扫描文件：`~/.zprofile`、`~/.zshrc`、`~/.bashrc`、`~/.profile`（存在才读）。
   匹配：`export\s+NVM_DIR=`、`nvm\.sh`、`VOLTA_HOME`/`\.volta/bin`、`fnm env|fnm shell`、`mise activate`、`ASDF_DATA_DIR`、`PROTO_HOME`、`export\s+N_PREFIX=`。
4. **归属判定**：
   - `active_node = which("node")` → `std::fs::canonicalize`；与任一 `manager.root` 做前缀比对 → `Manager(kind)`；落在 Homebrew Cellar → `Brew`；落在 `/usr/local/bin`（Win: `C:\Program Files\nodejs`）→ `SystemPkg`；有 node 但都不匹配 → `Unknown`；无 node → `None`。
   - Win reparse：`canonicalize(C:\Program Files\nodejs) != 自身`（归一化后比较，忽略大小写与尾分隔符）即为 reparse point；另判 `NVM_SYMLINK` 与该目录相等。零新依赖（工程目前只有 `winreg`）。
   - mac receipt：`pkgutil --pkgs` 输出含 `org.nodejs.node.pkg`。
   - `npm_prefix`：`npm config get prefix`（超时 2s，失败留 `None`，**不参与拦截判定**，M0 只用于日志）。

**缓存**：`detect()` 每次 `Refresh` 与每次安装前各调一次即可（读几个文件 + 两次子进程），M0 不引缓存层。

### 2.3 `src/detector/mod.rs` — 注册模块

`node_source` 加到 `pub mod` 列表（现仅 `env_check`，`detector/mod.rs:1-2`）。

### 2.4 `src/installer/mod.rs` — 四个接入点

每个分支在**执行任何写操作之前**（Win `nodejs` 分支在 `installer/mod.rs:343` 进入后立即、mac 在 `547` 后、卸载在 `459` / `617` 后）插入：

```rust
let node_env = crate::detector::NodeSource::detect();
if crate::config::node_guards_enabled(&mirror_or_app_config) {   // mode != "system"
    NodeSource::assert_install_allowed(&node_env, Channel::WindowsMsi)?;
}
log(format!("Node 所有权判定: owner={:?} managers={:?}", node_env.owner, node_env.managers));
```

注意：护栏必须在 `create_http_client` / `download_file` **之前**判定（`installer/mod.rs:281-284` 是先建 client 和临时目录的），否则用户会看到「下载 100% 然后被拒」——下载几十 MB 才被拦是最差体验。因此接入点实际放在 `install_target_windows` / `install_target_macos` 的 match 之前统一做一次前置检查，按 `software_id == "nodejs"` 分派 channel。

**呈现方式按 D3 定为「非错误指引」，不是 `bail!`**。护栏返回结构化判定而不是 `Result`，由调用方发新事件：

```rust
pub enum GuardDecision {
    Allow,
    Refuse { reason: String, commands: Vec<String>, note: Option<String> },
}
// installer 侧：
match NodeSource::check_install(&node_env, Channel::WindowsMsi) {
    GuardDecision::Refuse { reason, commands, note } => {
        let _ = event_sender.send(AppEvent::Guidance { target_id, reason, commands, note });
        return Ok(());          // 不是 Err：这是按设计的正确行为
    }
    GuardDecision::Allow => {}
}
```

`AppEvent::Guidance` 在前端渲染成**黄色/中性横幅**（带「复制命令」），不复用 `InstallError` 的红色日志样式。卸载侧同理。`mode = "system"` 跳过护栏时，日志首行打印一条警告说明本次已绕过所有权检查。

### 2.5 `src/installer/macos.rs` — 卸载安全化（G5）

`remove_pkg_node_files`（`macos.rs:161-164`）加两道前置：
1. `pkg_receipt` 为 false → 拒绝，提示「`/usr/local` 下的 node 不是官方 PKG 装入（可能是 n / 源码编译 / brew 链接），DevUp 不代为删除」；
2. 为 true 时，删除前逐项判断 `/usr/local/bin/node` 的 `read_link`/canonical 是否落在 manager 根目录内，落在则跳过该文件并记录。

### 2.6 `src/config.rs` — 逃生口（唯一的行为开关）✅ D1 已定：M0 不做 UI

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// "auto"(默认，启用全部护栏) | "system"(跳过护栏，恢复 0.3.5 行为)
    pub mode: String,
}
```
挂到 `AppConfig`（`config.rs:54-59`）：`pub node: NodeConfig`。`#[serde(default)]` + 默认值 `"auto"` 保证老 `config.json` 平滑升级，沿用 `config.rs:158-199` 现有测试风格补一条缺字段回退单测。

**M0 不为其提供 UI**：`system` 需要用户手改 `~/Library/Application Support/devup/config.json`（Win: `%APPDATA%\devup\config.json`）。理由：护栏误判时不能只留「改代码」这一条路；而给一个能绕过安全闸的默认可见开关，风险大于收益。跳过护栏时每次安装日志首行打印警告。

### 2.7 `src/detector/env_check.rs` — 仅一处口径修正

`infer_install_method`（`env_check.rs:231-259`）现在把路径含 `nvm` 判为 `"npm"`，这与「npm 全局包」混为一谈。M0 改为按 `NodeEnv.owner` 产出 `"nvm"` / `"volta"` / `"fnm"` 等真实托管标签（前端仅原样展示，`ui/app.js:532` 读的是 `winget` 分支，不受影响）。**按 D4，`managed_by` 提前进 M0**：`SoftwareStatus` 新增 `managed_by: Option<String>`，且「检测到 manager 但 GUI PATH 里解析不到 node」时**不再显示未安装**，而是显示 `由 nvm 托管`，版本从 `~/.nvm/versions/node/*` 目录名取最新一个。要点：

- `installed` 仍按「能否真正执行到 node」判定（GUI 场景为 false），但前端遇到 `managed_by = Some("nvm")` 时**不渲染「未安装」**，改渲染「由 nvm 托管」；
- badge 新增一个状态 `托管中`（既不是「需更新」也不是「已是最新」）——因为 DevUp 不认为自己对那份 runtime 负责；
- `latest_version` 仍照常展示，供用户知道终端里该装到哪个版本；
- 该 runtime **不被当作可操作 target**：Agent 安装仍按现有 PATH 解析结果走，M0 不改这一口径（M3 才统一）。

`ui/app.js:984` 附近的 badge 判定与 `style.css` 需配合加一个状态样式。

---

## 3. 护栏判定表（M0 最终规则）

判定基准是**「这次写入的目标位置归谁」**，而不是「机器上有没有 manager」——后者会大量误伤（装了 nvm 但日常用独立 MSI node 的用户很常见）。

| 编号 | 拦截点 | 触发条件（全部满足） | 动作 |
|---|---|---|---|
| **G1** | Win `msiexec node-*.msi` | `nodejs_dir_is_reparse` **或** `NVM_SYMLINK == C:\Program Files\nodejs` **或** MSI 目标 canonicalize 后落在任一 `manager.root` 内 | **拒绝**：MSI 会写穿 nvm 的链接、摧毁 `nvm use` 语义 |
| **G3** | mac `brew install node` | `brew_installed_formula("node") == false` **且** `managers` 非空 | **拒绝**：这是「凭空新增并行 runtime」。若 brew 已有 node（owner=Brew）→ 放行 `brew upgrade`，属正常升级 |
| **G4** | mac 官方 PKG 安装 | `managers` 含 `N` **或** `N_PREFIX=/usr/local` **或** `/usr/local/bin/node` 已存在但 `pkg_receipt == false` | **拒绝**：PKG 落点已被 `n` / 源码编译 / 他人占用 |
| **G5** | mac 卸载 node | `pkg_receipt == false` 且 brew 未装 node | **拒绝**（现状是盲 `rm -rf /usr/local/...`） |
| **G6** | Win 卸载 node | `Get-Package 'Node.js*'` 的 `InstallLocation` 落在任一 `manager.root` 内 | **拒绝**该项卸载 |
| **G2** | 所有渠道（总则） | 目标目录 canonical 前缀命中 manager 数据目录 | 由 G1/G4/G6 分别落地，G2 作为单测的通用断言 |

`Unknown` 归属（找到了 node 但认不出来源）：M0 **不拦安装**（避免误伤），但日志与卡片提示「Node 来源未识别：<路径>」，并在 M1 的回验里如实展示。保守只体现在卸载侧（G5/G6 拒绝）。

**拒绝时的文案模板**（`assert_*` 的 `bail!` 内容，走现有 `AppEvent::InstallError` → 前端日志区）：

```
当前 Node.js 由 nvm 托管（~/.nvm，当前 v26.12.0）。
DevUp 不会向该目录写入，也不会新增一份并行运行时来抢 PATH。
建议在你的终端执行：nvm install 24.22.0
（确需 DevUp 接管系统安装，请在 config.json 设 node.mode = "system"，该模式会跳过全部护栏）
```

---

## 4. 测试

### 4.1 单测（进 `cargo test`，全部无网络、无副作用）

| 测试 | 断言 |
|---|---|
| `merge_path_keeps_user_order` | `merge_path("/Users/x/.nvm/versions/node/v26/bin:/usr/bin", extras)` 里 `.nvm` 在 `/opt/homebrew/bin` 之前 |
| `merge_path_dedup_and_empty_segments` | 空分段被丢弃、extras 已存在不重复追加、保留首次出现位置 |
| `owner_from_path_table` | 给定合成路径集（`~/.nvm/...`、`/opt/homebrew/Cellar/node/...`、`/usr/local/bin/node`、`C:\Program Files\nodejs`、`~/.volta/tools/image/node/...`、`~/scoop/apps/nodejs/...`）→ 期望 `NodeOwner` 表驱动断言 |
| `scan_shell_rc_detects_managers` | fixture rc 文件（含 `export NVM_DIR` / `eval "$(fnm env)"` / `volta` / `mise activate`）→ 识别对应 kind；**fixture 里放一行 `echo PWNED`，断言输出无副作用**（不执行） |
| `assert_install_allowed_matrix` | G1/G3/G4/G5/G6 每条一个允许用例 + 一个拒绝用例（用 `NodeEnv` 构造，不碰真机） |
| `config_node_mode_default` | `{}` → `node.mode == "auto"`（对齐 `config.rs:163-175` 现有风格） |

纯函数化是关键前提：`detect()` 里的「读 env + 读文件 + 跑子进程」与「给定输入判定归属」要分开，后者全部走参数注入，否则单测会依赖本机环境和并行 test 的 env 竞争。

### 4.2 真机验收（对应设计文档 §11 T1–T6）

| # | 机器 | 步骤 | 通过标准 |
|---|---|---|---|
| A1 ✅ | macOS（本机：nvm + volta 共存，`~/.nvm` 有 v16/v24/v26） | `cargo test -- --ignored --nocapture dump_path_resolution` | `which(node)` = `~/.nvm/versions/node/v26.12.0/bin/node`，与用户终端 `node -v` 一致。**判别力受限，须如实记录**：本机 `/opt/homebrew/bin` 与 `/usr/local/bin` 都没有 node，所以这一条改前改后都返回 nvm，端到端区分不出差异；顺序正确性由表驱动单测 + 下面的 A1b 合成探针证明 |
| A1b ✅ | macOS，合成 Finder 启动环境 | `env PATH="/usr/bin:/bin:/usr/sbin:/sbin" <test-bin> -- --ignored --nocapture dump_path_resolution` | 合并后为 `brew → /usr/local/bin → ~/.local/bin → ~/.npm-global/bin → /usr/bin → /bin …`：补充目录确实排在系统目录之前（brew git 不被 CLT git 遮蔽），`which(brew)` 仍命中；`which(node)` 为 `None`（GUI 认不出 nvm 是 M0 明确的非目标，由步 2 的护栏负责拒绝） |
| A2 | macOS 同上 | 点 Node 更新（brew 未装 node 的情形） | 拒绝 + 文案，`brew list node` 仍为空 |
| A3 | macOS，另有 brew node 的机器 | 点更新 | 放行 `brew upgrade node`（回归现状）。**D5：本条为发版前 checklist，不阻塞开工** |
| A4 | macOS | 点卸载 | `pkg_receipt` 判定生效，不再盲删 |
| A5 | Windows + nvm-windows | 点更新 | 拒绝，**`C:\Program Files\nodejs` 的 reparse 属性与 `nvm use` 行为不变**（用 `dir /AL` 前后对比） |
| A6 | Windows，裸 MSI node | 点更新 | 放行，MSI 正常升级 |
| A7 | 任一平台 | 从 Finder/开始菜单启动（非终端） | manager-only 用户：卡片按 D4 显示「由 nvm 托管 · vX」而非「未安装」；点击 → `Guidance` 非错误横幅 + 可复制命令 |

**Windows 侧（A5/A6）已按评审决定标记为「待验」**：M0 先发 macOS，Windows 护栏代码照常实现与提交，但发布说明只能写「已阻断向 manager 托管目录的写入」，不能写「已在 nvm-windows 真机验证」。设计文档 §12 R8（MSI 遇 junction 的三种具体表现未实测）同样保持 open——护栏的正确性不依赖于是哪一种（一律拒写），但这条只能等 Windows 环境补测。补测清单届时单独出（含 `dir /AL` 前后对比、注册表 Node.js 项检查、`nvm use` 回归）。

---

## 5. 发布与回滚

- 单独一次提交 + patch 版本 `0.3.6`；发布说明必须写明「检测到版本管理器时，DevUp 不再安装/卸载 Node，改为给出命令建议」——这是行为变更，不是纯 bug 修复。
- 回滚路径：`config.json` 设 `node.mode = "system"`（不需重新发版）。
- 代码侧回滚：护栏与 `merge_path` 分属两个提交，可单独 revert；建议拆成
  `fix(platform): Unix PATH 改为追加语义，保留用户优先级`
  `feat(detector): Node 所有权探测与安装护栏`

## 6. 工作量与顺序

| 步 | 内容 | 规模 |
|---|---|---|
| 1 | ✅ 已完成：`merge_path` 纯函数 + 单测 + 接进 `child_path`（实现细节见 §2.1 实现记录） | 小 |
| 2 | `node_source.rs`：类型 + `scan_shell_rc` + `detect` + 表驱动单测 | 中（主要工作量） |
| 3 | `assert_*` 六条护栏 + 文案 | 小 |
| 4 | 四处接入点（安装/卸载 × 两平台），注意放在下载之前 | 小 |
| 5 | `macos.rs` 卸载安全化 + `NodeConfig` + `env_check` 标签修正 | 小 |
| 6 | A1/A2/A4/A7 本机验收；A5/A6 需 Windows 环境 | — |

> 本节的手工步骤表已被 §7 流水线取代执行顺序，工作量估算仍适用。

评审已全部定案（D1–D5 见设计文档 §13）：逃生口只进 `config.json`、`Unknown` 只拦卸载、Windows 标待验只发 macOS、护栏拒绝走 `Guidance` 非错误事件、卡片显示「由 nvm 托管」取代「未安装」。

---

## 7. 实施流水线（一任务一子 agent，串行）

**协作约定**（每个子 agent 的 prompt 里都要带）：

1. 只改本任务列出的文件；**不得跑 `cargo fmt`**（仓库整体非 fmt-clean，跑了会污染无关文件）；不得改与本任务无关的代码风格。
2. **不得提交、不得推送**——由主 agent 复核 diff 后提交。
3. 必须自带单测；结束时 `cargo test` 必须全绿，并给出「改了哪些函数 + 新增哪些断言 + 未覆盖的点」三行摘要。
4. 不动 `docs/`（文档由主 agent 更新，避免并发写冲突）。
5. Windows 分支代码照常写，但标注为「本机未验证」，不得声称已验证。

| 任务 | agent 名 | 范围（文件） | 依赖 | 完成判据 |
|---|---|---|---|---|
| **A** 逃生口 | `node-cfg` | `src/config.rs` | — | `NodeConfig.mode` 缺字段回退 `"auto"`；`guards_enabled()` 单测（含 `"system"`、空串、未知值） |
| **B** 所有权探测 | `node-env` | `src/detector/node_source.rs`(新)、`src/detector/mod.rs` | — | 表驱动 owner 判定全绿；`scan_shell_rc` fixture 含 `echo PWNED` 且断言无副作用；`--ignored` 探针输出本机 `NodeEnv` |
| **C** 指引事件通路 | `node-guide` | `src/state.rs`、`src/main.rs`、`ui/app.js`、`ui/style.css` | — | `AppEvent::Guidance` 端到端可渲染（临时手动触发入口在测试后移除）；非红色横幅 + 命令可复制；`cargo test` 全绿 |
| **D** 护栏与接入 | `node-guard` | `src/detector/node_source.rs`、`src/installer/mod.rs` | A B C | G1–G6 各一条允许 + 一条拒绝单测；拒绝发生在 `download_file` **之前**；走 `Guidance` 且返回 `Ok(())` |
| **E** 显示但不代管 | `node-card` | `src/detector/env_check.rs`、`ui/app.js`、`ui/style.css` | B C | `managed_by` 落到 `SoftwareStatus`；nvm-only 机器显示「由 nvm 托管 · vX」而非「未安装」；badge 新状态 `托管中` |
| **F** 卸载安全化 | `node-uninstall` | `src/installer/macos.rs`、`src/installer/mod.rs` | B D | 无 PKG receipt 时拒绝删除并说明；Win 卸载按 `InstallLocation` 过滤 manager 目录 |
| **G** 回归与收口 | `node-verify` | 全量只读 + `README.md` | A–F | `cargo test` + A1b/A2/A4/A7 探针实跑记录；README 增「行为变更」小节；护栏拒绝文案复核 |

任务间由主 agent 串行推进：**子 agent 完成 → 我复核 diff 并跑测试 → 更新 §8 状态表与本文件相关小节 → 提交 → 关闭该 agent → 开下一个**。

## 8. 任务状态表（每完成一个任务由主 agent 更新）

| 任务 | 状态 | 提交 | 备注 |
|---|---|---|---|
| 步 1 PATH | ✅ 已完成 | `7ffa93d` | 含计划偏差修正（补充目录插在系统兜底目录之前） |
| A `node-cfg` | ⏳ 待开 | — | |
| B `node-env` | ⏳ 待开 | — | M0 最大工作量 |
| C `node-guide` | ⏳ 待开 | — | |
| D `node-guard` | ⏳ 待开 | — | |
| E `node-card` | ⏳ 待开 | — | |
| F `node-uninstall` | ⏳ 待开 | — | |
| G `node-verify` | ⏳ 待开 | — | |
