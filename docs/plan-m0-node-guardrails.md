# M0 实施计划：Node 所有权护栏（止血）

> 分支：`feat/node-manager-coexistence`
> 上游设计：`docs/design-node-manager-coexistence.md`（§1 现状缺陷、§4 护栏 G1–G6、§10 M0）
> 目标版本：0.3.6（patch 发布）
> 状态：评审已定案（D1–D5 见设计文档 §13）；步 1 与流水线 A–G **已全部完成**（G 收口于 2026-10-20，实录见 §10）；G 收口期间经用户追加任务 **H**（托管弹窗直通指引 + 卸载建议修正，见 R21/R22）也已完成；剩余动作只有发版（0.3.6 版本号提升 + tag，见 §5）

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

**实现定稿与上述规格的差异**（任务 B 落地后确定，后续以本节为准）：

- 自由函数 `NodeSource::detect()/classify()` 而非 `impl NodeSource`（模块本身即命名空间，`detector::node_source::detect()`）；
- `Manager` 多一个字段 `versions: Vec<String>`（语义降序），`NodeEnv` 顶层 `managed_versions` 只是「owner 命中那一个」的视图；`NodeEnv::owner_manager() -> Option<&Manager>` 取托管方本体，逐管理器渲染直接遍历 `managers`；
- `NodeEnv` 多一个字段 `nvm_symlink_is_nodejs_dir: bool`——G1 的两个触发条件是「目录是 reparse」与「`NVM_SYMLINK` 指向该目录」，后者是独立证据（链接可能已被上一次 MSI 写成实体目录，而 nvm 配置仍指着它），不放进输出任务 D 就拿不到；
- 采集与判定分离为 `detect()`（读 env / 目录 / rc / 子进程）与 `classify(&NodeProbeInput) -> NodeEnv`（纯函数）；
- `pub fn path_key` / `pub fn is_within` 供任务 D 复用同一套归一化与边界规则（各写一份迟早漂移成两种结论）；
- 护栏接口按 D3 改为 `GuardDecision::{Allow, Refuse}`，不再是不带信息的 `Result<()>`。

`detect()` 的四路信号（M0 只做「是否有人在托管」，**不调用任何 manager 命令**）：

1. **环境变量**：`NVM_DIR`、`NVM_SYMLINK`、`FNM_DIR`、`FNM_MULTISHELL_PATH`、`VOLTA_HOME`、`MISE_DATA_DIR`、`ASDF_DATA_DIR`、`PROTO_HOME`、`N_PREFIX`。
2. **已知目录探测**：`~/.nvm`、`~/.fnm`、`~/Library/Application Support/fnm`、`~/.volta`、`~/.local/share/mise`、`~/.asdf`、`~/.proto`、`~/scoop/apps/nodejs`、`%APPDATA%\nvm`、`%APPDATA%\nvs`。
3. **shell 配置只读解析**（Finder 启动时唯一能识破 nvm 的手段，`env_check.rs` 现在完全没有这条）：
   ```rust
   /// 只读扫描，绝不 source / 执行用户 rc（隐私与副作用约束见设计文档 §12 R3）
   fn scan_shell_rc(home: &Path) -> Vec<Manager>
   ```
   扫描文件：`~/.zprofile`、`~/.zshrc`、`~/.bashrc`、`~/.profile`（存在才读）。
   匹配（实现用小写字符串 `contains`，**不引入 regex crate**）：`export nvm_dir=`、`nvm.sh`、`volta_home=`、`.volta/bin`、`fnm env`/`fnm shell`、`fnm_dir=`、`mise activate`、`mise_data_dir=`、`asdf_data_dir=`、`proto_home=`、`export n_prefix=`，以及实现期补强的 `zsh-nvm`。
   三条实现期确立的规则：① 注释行跳过（`# export NVM_DIR=` 是用户主动停用，算成托管证据会凭空拦住一次正常安装）；② 取路径的片段一律以 `=` 结尾，因此 `$PROTO_HOME/bin`、`${VOLTA_HOME}` 这类「读它」的形态天然不命中；③ 不按运行平台改写 kind——被扫的是 POSIX shell 文件，出现 `nvm.sh` 的就是 Unix 版 nvm，nvm-windows 不写 rc 而靠注册表环境变量。
   `zsh-nvm` 这条是必要的补强：本机正是 oh-my-zsh 插件形态，`NVM_DIR` 未设、`.zshrc` 里也没有 `export NVM_DIR=`，不认这个片段就会漏判 nvm。
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
        crate::state::send_guidance(event_sender, target_id, reason, commands, note);
        return Ok(false);       // false = 本轮未执行；外层据此跳过 InstallSuccess（见 §9 台账 R1）
    }
    GuardDecision::Allow => {}
}
```

`AppEvent::Guidance` 在前端渲染成**黄色/中性横幅**（带「复制命令」），不复用 `InstallError` 的红色日志样式。卸载侧同理。`mode = "system"` 跳过护栏时，日志首行打印一条警告说明本次已绕过所有权检查。

**任务 C 已把通路落地**（`ba89d60`），D 只需调用，不必改 `state.rs` / `main.rs` / `ui/`：
```rust
crate::state::send_guidance(event_sender, target_id /* &str */, reason /* impl Into<String> */, commands /* Vec<String> */, note /* Option<String> */)
```
- 线上形状是 `{"type":"Guidance","data":{...}}`（`AppEvent` 无 `rename_all`），已有 `include_str!("../ui/app.js")` 的同源单测钉住，D 改字段会挂测试；
- `note` 缺省序列化为显式 `null`，前端按空处理；`commands` 可为空数组，横幅照常渲染；
- 前端收到 `InstallSuccess` 会 `delete state.guidance[target_id]`，所以 D 在一次**成功**安装后想留说明得走 `Log`，不要用 `Guidance`；
- `handleGuidance` 已代管「结束本轮」的收尾（清 `activeTaskId`、`hideProgress()`、`enableAllButtons()`），D 拒绝后**只需 return `Ok(())`**，不要再自己改任务态。

一处遗留 nit（不阻塞，D 顺手可改）：`ui/style.css` 横幅的边框与背景写死 `rgba(210,153,34,…)`，与 `--warning: #d29922` 同值两处；将来调色板变动，横幅不会跟着变。

### 2.5 `src/installer/macos.rs` — 卸载安全化（G5）

`remove_pkg_node_files`（`macos.rs:161-164`）加两道前置：
1. `pkg_receipt` 为 false → 拒绝，提示「`/usr/local` 下的 node 不是官方 PKG 装入（可能是 n / 源码编译 / brew 链接），DevUp 不代为删除」；
2. 为 true 时，删除前逐项判断 `/usr/local/bin/node` 的 `read_link`/canonical 是否落在 manager 根目录内，落在则跳过该文件并记录。

### 2.6 `src/config.rs` — 逃生口（唯一的行为开关）✅ D1 已定：M0 不做 UI

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// "auto"(默认，启用全部护栏) | "system"(跳过护栏，恢复 0.3.5 行为)
    pub mode: String,
}
impl Default for NodeConfig { /* mode = "auto" */ }   // 手写，与 MirrorConfig 风格一致

impl AppConfig {
    /// 仅显式 "system"（trim + 小写）跳过；空串/未知值/乱填一律启用护栏
    pub fn node_guards_enabled(&self) -> bool;
}
```
挂到 `AppConfig`（`config.rs:54-59`）：`pub node: NodeConfig`。`#[serde(default)]` + 默认值 `"auto"` 保证老 `config.json` 平滑升级，沿用 `config.rs:158-199` 现有测试风格补缺字段回退单测。

**两条实现期裁定**（任务 A 复核时定，后续复核以本节为准，不要按旧 snippet 改回去）：

1. `Default` 用**手写 impl** 而非 derive——对齐同文件 `MirrorConfig` 的既有写法。
2. `node_guards_enabled()` 目前只有测试调用，`cargo build` 会报 `method is never used`，因此加了 `#[allow(dead_code)]` + 注释指明来源。**遗留项：任务 D（护栏接入）必须移除这个 allow**，否则它会永久留着。已登记在 §8。

**M0 不为其提供 UI**：`system` 需要用户手改 `~/Library/Application Support/devup/config.json`（Win: `%APPDATA%\devup\config.json`）。理由：护栏误判时不能只留「改代码」这一条路；而给一个能绕过安全闸的默认可见开关，风险大于收益。跳过护栏时每次安装日志首行打印警告。

### 2.7 `src/detector/env_check.rs` — 仅一处口径修正

`infer_install_method`（`env_check.rs`）把路径含 `nvm` 判为 `"npm"`，用来描述 **runtime** 是错的（Agent 落在 `~/.nvm/versions/node/*/bin` 下确实就是 npm 全局包，判 `"npm"` 没错）。**任务 E 的裁定（§9 R11）：不重写 `infer_install_method`，只在 nodejs 卡片上用 `NodeEnv.owner` 覆盖 `install_method`**（Manager → `label()`，Brew → `"brew"`，`SystemPkg`/`Unknown`/`None` 沿用旧推断）。原规格那句「M0 改为按 owner 产出真实托管标签」写糙了——字面执行会连带改 8 张卡片的呈现，属于我自己给的约束之外的改动。**按 D4，`managed_by` 提前进 M0**：`SoftwareStatus` 新增 `managed_by: Option<String>`，且「检测到 manager 但 GUI PATH 里解析不到 node」时**不再显示未安装**，而是显示 `由 nvm 托管`，版本从 `~/.nvm/versions/node/*` 目录名取最新一个。要点：

- `installed` 仍按「能否真正执行到 node」判定（GUI 场景为 false），但前端遇到 `managed_by = Some("nvm")` 时**不渲染「未安装」**，改渲染「由 nvm 托管」；
- badge 新增一个状态 `托管中`（既不是「需更新」也不是「已是最新」）——因为 DevUp 不认为自己对那份 runtime 负责；
- `latest_version` 仍照常展示，供用户知道终端里该装到哪个版本；
- 该 runtime **不被当作可操作 target**：Agent 安装仍按现有 PATH 解析结果走，M0 不改这一口径（M3 才统一）。

**本机实测数据点（影响 E 的渲染，别按「副管理器为空」想当然）**：`managers` 里 Volta 并不空——`~/.volta/tools/image/node/` 实装 `16.14.2` 与 `20.15.0`，与 nvm 的三个版本并存。所以 owner=`Manager(Nvm)` 时，卡片只能取 `managed_versions`（nvm 那三个），**不得**把 volta 的两个版本混进「由 nvm 托管」；owner 为 `None`/`Unknown` 时按 `managers` 逐个渲染成「nvm（26.12.0 / 24.22.0 / 16.20.2）、volta（20.15.0 / 16.14.2）」这种各归各的形态，而不是合并成一条版本列表。取托管方本体用 `owner_manager()`（要 `root` 给用户提供路径证据就靠它）。

`ui/app.js:984` 附近的 badge 判定与 `style.css` 需配合加一个状态样式。

**任务 D 已交付给 E 的现成接口（别重复实现）**：`ManagerKind::label() -> &'static str`（nvm / nvm-windows / fnm / Volta / mise / asdf / proto / n / Scoop / nvs，文案取名字统一走它，别再各处 match）；`NodeEnv::owner_manager()` 给托管方本体（要 `root` 给用户当路径证据就靠它）；`NodeEnv::managed_versions` 只在 owner 命中时非空。**口径**：渲染「由 X 托管」一律按 owner 取那一个，遍历 `managers` 只用于「本机还有哪些管理器」，两者混用就会把 volta 的版本写进 nvm 的托管说明里。

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

**拒绝时的文案模板**（最终落地为 `check_install` / `check_uninstall` 返回的 `GuardDecision::Refuse`，经 `send_guidance` 走 `AppEvent::Guidance`，**不是** `InstallError`）：

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
| A2 ✅ | macOS 同上 | 点 Node 更新（本机以「重新安装」等价通路触发，见 R20） | **G 实录命中**：黄色 Guidance 横幅（G3/MacBrewInstall），无任何下载/写入 |
| A3 | macOS，另有 brew node 的机器 | 点更新 | 放行 `brew upgrade node`（回归现状）。**D5：本条为发版前 checklist，不阻塞开工** |
| A4 ✅ | macOS | 点卸载 | **G 实录命中**：G5 拒绝横幅，未弹授权框、未执行删除 |
| A5 | Windows + nvm-windows | 点更新 | 拒绝，**`C:\Program Files\nodejs` 的 reparse 属性与 `nvm use` 行为不变**（用 `dir /AL` 前后对比） |
| A6 | Windows，裸 MSI node | 点更新 | 放行，MSI 正常升级 |
| A7 ✅ | macOS（受限 PATH 模拟 launchd 环境，见 R20；真 `open DevUp.app` 待发版产物补跑） | 非终端环境启动 | **G 实录命中**：node 卡「由 nvm 托管 · v26.12.0」而非「未安装」 |

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
6. 提交由主 agent 负责，每个任务两个提交：代码提交（`feat|fix(scope): …`）+ 文档状态提交（`docs(plan): …`，把 §8 状态表填上真实提交号）。
7. 格式化只允许 `rustfmt --edition 2024 <显式文件清单>`，且**清单里不得出现含 `mod xxx;` 声明的文件**——rustfmt 会跟随模块声明递归格式化子模块。任务 B 收尾时主 agent 把 `src/detector/mod.rs` 传了进去，导致 `env_check.rs` 被整体重排（+96/−32 纯格式化改动，无逻辑变化，已 `git checkout` 还原）。格式化的可执行判据分两种：**流水线新建的文件**必须 `rustfmt --edition 2024 <该文件>` 后 `--check` 无输出（这类文件没有 `mod xxx;` 声明，单独格式化不外溢）；**既有文件一律不动**，判断有没有偷跑格式化的证据看 `git diff --stat` 的 **deletions 数量**——真跑过 fmt 会产生几十处删除，语义改动的删除数应与改动规模相称（任务 E 是 412 增 / 7 删、后续 11 增 / 19 删，都属正常）。既有文件里新增的行天然可能不够 canonical（`env_check.rs` 从 12 处 hunk 变 16 处），这是可接受的，不为此整片重排。
8. 子 agent 的 `result` / idle 摘要**不得作为验收依据**。任务 B 期间实测到三类不一致：报告「27 passed」时工作树其实编译不过（真实的半成品中间态）；`TaskStop` 返回成功后其进程仍存活并继续投递消息；自述「已 `touch` 两个文件强制重编译」但文件 mtime 仍是主 agent 的操作时刻、15:05 后无任何写入痕迹。所有数字一律由主 agent 自己重跑 `cargo test` / `cargo build` / `git status` / `stat` 得出。

| 任务 | agent 名 | 范围（文件） | 依赖 | 完成判据 |
|---|---|---|---|---|
| **A** 逃生口 | `node-cfg` | `src/config.rs` | — | `NodeConfig.mode` 缺字段回退 `"auto"`；`guards_enabled()` 单测（含 `"system"`、空串、未知值） |
| **B** 所有权探测 | `node-env` | `src/detector/node_source.rs`(新)、`src/detector/mod.rs` | — | 表驱动 owner 判定全绿；`scan_shell_rc` fixture 含 `echo PWNED` 且断言无副作用；`--ignored` 探针输出本机 `NodeEnv` |
| **C** 指引事件通路 | `node-guide` | `src/state.rs`、`src/main.rs`、`ui/app.js`、`ui/style.css` | — | `AppEvent::Guidance` 序列化形状有单测锁定；前端渲染为**中性/黄色横幅**（非 `InstallError` 红字）且命令可一键复制；`cargo test` 全绿。**不做临时触发入口**：本机就装着 nvm，任务 D 落地后点「更新」即是最真实的视觉验证 |
| **D** 护栏与接入 | `node-guard` | `src/detector/node_source.rs`、`src/installer/mod.rs`、`src/config.rs`、`src/state.rs`、`src/detector/mod.rs`（后两个只为清 allow，实际文件清单比本表原稿多这两项） | A B C | G1–G6 各一条允许 + 一条拒绝单测；拒绝发生在 `download_file` **之前**；走 `crate::state::send_guidance(...)` 且返回 `Ok(())`；**移除下列四处 `allow`**（`config.rs` 的 `node_guards_enabled`、`state.rs` 的 `Guidance` 变体与 `send_guidance`、`node_source.rs` 的模块级 `#![allow(dead_code)]`、`detector/mod.rs` 的 `#[allow(unused_imports)]`）——任务 G 的 `grep -rn "allow(dead_code)\|allow(unused_imports)" src/` 必须只剩与本流水线无关的既有项；`detect()` 内含最长约 2s 的子进程等待（`npm config get prefix` / `pkgutil` / `node --version`），接入时必须确认它不跑在 UI 事件循环线程上，必要时 `spawn_blocking` |
| **E** 显示但不代管 | `node-card` | `src/detector/env_check.rs`、`ui/app.js`、`ui/style.css` | B C | `managed_by` 落到 `SoftwareStatus`；nvm-only 机器显示「由 nvm 托管 · vX」而非「未安装」；badge 新状态 `托管中` |
| **F** 卸载安全化 | `node-uninstall` | `src/installer/macos.rs`、`src/installer/mod.rs` | B D | 无 PKG receipt 时拒绝删除并说明；Win 卸载按 `InstallLocation` 过滤 manager 目录 |
| **G** 回归与收口 | `node-verify` | 全量只读 + `README.md` | A–F | `cargo test` + A1b/A2/A4/A7 探针实跑记录；README 增「行为变更」小节；护栏拒绝文案复核 |

任务间由主 agent 串行推进：**子 agent 完成 → 我复核 diff 并跑测试 → 更新 §8 状态表与本文件相关小节 → 提交 → 关闭该 agent → 开下一个**。

## 8. 任务状态表（每完成一个任务由主 agent 更新）

| 任务 | 状态 | 提交 | 备注 |
|---|---|---|---|
| 步 1 PATH | ✅ 已完成 | `7ffa93d` | 含计划偏差修正（补充目录插在系统兜底目录之前） |
| A `node-cfg` | ✅ 已完成 | `4930f8f` | 17 passed / 0 warning；**遗留：`node_guards_enabled()` 上的 `#[allow(dead_code)]` 必须在任务 D 移除**（已写进 D 的完成判据） |
| B `node-env` | ✅ 已完成 | `24c6a9c` | 27 passed / 3 ignored、build 0 warning、新文件 rustfmt-clean。**过程记录**：第一版交回时工作树编译不过（结构改动做了一半），第二版补齐但 `managers_with_versions()` 实为 `managers` 的别名，最后两项由主 agent 代为收尾；主 agent 自己误用 `rustfmt src/detector/mod.rs` 递归重排了 `env_check.rs`（已还原，教训进 §7 约束 7） |
| C `node-guide` | ✅ 已完成 | `ba89d60` | 30 passed / 3 ignored、build 0 warning、`node --check` 通过、381 insertions + **0 deletions**（未跑 fmt 的硬证据）、只碰清单内 3 文件。**新增两处 `allow` 已并入 D 的清理判据**；`main.rs` 零改动即接通（派发是变体无关的 serde） |
| D `node-guard` | ✅ 已完成 | `b442fdf` | 34 passed / 4 ignored、build 0 warning、四处 allow 全清（grep 只剩 `proxy.rs` / `updater.rs` 的既有项）、接入点均在下载之前、`detect()` 走 `spawn_blocking`；`node_source.rs` 被它带回 9 处 fmt hunk，已由主 agent 单独 rustfmt 复原为 0。**7 处偏离的裁定见 §9 台账** |
| E `node-card` | ✅ 已完成 | `33ac30e` + `e61afc8` | 39 passed / 5 ignored、build 0 warning、`node --check` OK、`detect_all()` 每轮只取证一次；A7 现场实跑复现（GUI 模拟：`managed_by=nvm`、`installed=false`，不再报「未安装」）。两处返工见 R12/R13；R9 决定不动 `main.rs`；**R14 的前端组合无自动化覆盖，是 G 的走查项** |
| F `node-uninstall` | ✅ 已完成 | `b4ff86e` | 43 passed / 6 ignored、build 0 warning、只碰 `macos.rs` + `installer/mod.rs`（546 增 / 13 删，无格式化外溢）、`node_source.rs` 一字未动仍 0 fmt hunk。提权守卫已代码级确认：`removable.is_empty()` 早退，`run_admin_shell` 不可达。偏离裁定见 §9 R15–R18 |
| G `node-verify` | ✅ 已完成 | `5876c57` | 全量只读复核 43 passed / 0 warning / grep·rustfmt 达标；6 探针实跑 + GUI 实点走查（G3/G5 黄色横幅、复制、A7）全部命中；README 增「行为变更（0.3.6）」；走查发现既有缺陷 **R19**（确认弹窗在 mac 显示 Windows 命令）已顺手修复。实录见 §10 |
| H 指引直通弹窗 | ✅ 已完成 | `f2f915b` | G 收口期间用户追加：托管机器点安装/卸载**先弹管理器指引**而非假确认窗（`guard_hint` / `uninstall_hint` 由 `check_install`/`check_uninstall` 同源预览）；过程中用户又点破卸载建议误用安装族命令（R22）。47 passed / 0 warning、新文件 fmt-clean、两站弹窗经用户实点确认 |

## 9. 实现期裁定台账

子 agent 交付时会带「偏离」清单，这里逐条留档，避免后来人靠聊天记录考古。

| # | 偏离 | 裁定 | 理由 |
|---|---|---|---|
| **R1**（D-A） | 四个平台函数从 `Result<()>` 改为 `Result<bool>`，`false` = 护栏已出指引、本轮未执行 | **采纳，属必要修正** | 外层 `install_target` 原本在平台函数正常返回后**无条件**发 `InstallSuccess`，而前端收到它第一件事就是 `delete state.guidance[target_id]`（`ui/app.js:92-94`）——照 `Ok(())` 写会当场抹掉刚挂上的横幅，还把「未代为执行」渲染成「安装完成」。任务 C 的注释早就写明「护栏拒绝路径不会再来 InstallSuccess」，是 D 的规格没接住这条 |
| **R2**（D-B） | Refuse 的 `commands` 里用 `{ver}` 令牌，由调用侧 `apply_target_version()` 注入 `Detector::get_latest_version("nodejs")` | **采纳** | 「判定是纯函数」与「版本不硬编码、取自全局缓存」不能让同一个函数既满足又保持表驱动单测不受线上状态漂移 |
| **R3**（D-C） | 护栏开关在 preflight 里 `AppConfig::load()` 从磁盘读，而不是用 `main.rs` 内存里那份 `Arc<Mutex<AppConfig>>` | **采纳** | `node.mode` 在 M0 没有 UI（D1），唯一改法是手改 config.json，磁盘本就是这件事的真相源；换来的是跨层签名不动、不碰 `main.rs`。代价是一次几 KB 文件读，已与探测合进同一个 blocking 任务 |
| **R4**（D-D） | 改写任务 B 的 `platform_known_dirs`：从整段 `#[cfg(windows)]` 改为运行时 `if !cfg!(windows) { return Vec::new(); }` | **采纳，覆盖任务 B 的实现决定** | 清掉模块级 allow 后，`ManagerKind::Nvs` 在 mac 构建里报「never constructed」；那两个条目只是路径拼接、不碰 Windows API，B 当初「必须靠 cfg 排除」的理由不成立。探测语义零变化（非 Windows 仍是空表）。**B 与 D 在同一文件上重叠，这类覆盖必须显式记档** |
| **R5**（D-E） | `same_path` 加 `#[cfg(any(windows, test))]` | **采纳** | 唯一非测试调用方是 `#[cfg(windows)]` 函数，不加会在 mac 构建里成死代码 |
| **R6**（D-F） | mac 卸载按实际路由传渠道：`brew_installed_formula("node")` 为真 → `MacBrewInstall`（`check_uninstall` 放行）；只有要走 `remove_pkg_node_files` 才按 `MacPkg` 适用 G5 | **采纳** | G5 拦的是「闭眼 `rm -rf /usr/local/bin/node`」，`brew uninstall node` 删的是 brew 自己记账的目录，不在射程内。副作用已知：装了 brew node 却被 nvm 遮蔽的机器仍会放行卸载——这是「谁装的谁移除」的一致口径，不改成严格字面 |
| **R7**（D-G） | `owner = Unknown` 本身不触发拒绝（D2），但 G1 的三条**直接证据**（reparse / `NVM_SYMLINK` / 目录落在 manager.root 内）成立时照拦 | **采纳，并据此精确化 D2** | `classify` 的表里就有一条「目录是 reparse 却查不到管理器证据 → Unknown」；若连这种也放行，G1 的第一条触发条件形同虚设。D2 的本意一直是「归属不明**且无直接证据**时不误拦」，设计文档 §13 D2 已补这句限定 |
| **R8**（主 agent 自身） | D 把 `node_source.rs` 的 fmt hunk 从 0 带回 9 处 | **已复原** | 该文件是流水线新建、按 §7 约束必须保持 rustfmt-clean；单独 `rustfmt --edition 2024` 该文件安全（已确认它没有文件级 `mod` 声明，不会递归外溢） |
| **R9**（E-线程） | 启动首轮 `main.rs:382` 的 `detect_all()` 在主线程（早于 `:410` 的 `event_loop.run`），新增取证会不会拖慢首屏 | **不改 `main.rs`** | E 给了同机 A/B 对照（把首轮取证临时换成 `NodeEnv::default()`，各 3 次，测完逐字复原、HEAD 无残留）：新增成本 **GUI +28ms / 终端 +180ms**，而首轮本来就在这条线程跑 550–910ms 的既有探测。两个候选解法（展示路径跳过 `npm_prefix` 取证、首轮挪进 `rt.spawn` 走 `tokio_event_tx` 桥）都是在优化它的一小部分，不值得换跨层改动 + 首屏事件时序变化 |
| **R10**（E-①） | `owner = Brew` / `SystemPkg` 时 `managed_by = None` | **采纳** | 那时生效 runtime 就在 DevUp 能直接操作的位置上，说「托管中」是假陈述；表驱动各钉一条用例 |
| **R11**（E-②） | 没按规格重写 `infer_install_method`，只在 nodejs 卡片覆盖 | **采纳，且比规格更对** | 见 §2.7 已改正的口径：重写会连带改 8 张卡片，撞我自己在任务 C/E 都写过的「不动 agent 渲染」 |
| **R12**（E-③④） | `#[serde(default)]` 补上；`hasUpdate` 曾加 `!isManaged` | ①**采纳**；②**已撤回**（`e61afc8`） | ②的副作用是 E 自己上报的：托管 + 已装的卡片会掉到「卸载 + 重新安装」行，而托管机器上卸载同样被 G5 拒绝——在两个都不会执行的动作里挑一个，还把主行动藏了。现在 `hasUpdate` 原判定、`isManaged` 只管 badge 与 meta |
| **R13**（E-⑤） | `version_key` 曾在 `env_check.rs` 复制一份 + 注释「改动须同步」 | **已去重**（`e61afc8`） | 复制是我「不许动 `node_source.rs`」这条硬约束逼出来的次优解，正解是导出：`node_source.rs` 只改 `fn` → `pub fn` 一行（**这是 G 收口 grep 时该认下来的授权破例**，不算越界），`env_check` 调 `node_source::version_key`。§2.2 警告的漂移风险就此消失 |
| **R14**（E，遗留） | 「托管中 badge + 升级最新按钮」并存**是设计意图**，且这条组合没有任何自动化测试钉住 | **交给 G 的 GUI 走查** | JS 侧只有 C 那条读 `handleGuidance` 文本的契约测试覆盖前端；视觉复核时别当矛盾状态改回去。要长期防回归得给 `renderAll` 抽一层可测纯函数，不属 M0 范围 |
| **R15**（F-①） | `pkg_removal_targets` 加了第三个参数 `resolve: impl Fn(&Path) -> Option<PathBuf>` | **采纳** | 我的规格同时要求「纯函数、不读 fs」与「按 canonical 目标判定」，这两条没有注入解就不可兼得，而「候选不在磁盘上也能判定」那条测试正是靠注入才成立。生产侧 `fs_canonical_target` 只做 `canonicalize` 读取 |
| **R16**（F-②③④） | `remove_pkg_node_files` 返回 `Result<PkgRemoval{deleted,skipped,executed}>`；跳过项「逐项 log，只在全部挡住时出 `Guidance`」；「正在弹出系统授权框」日志移入真会弹的分支 | **采纳** | ③是对的：删了部分又跳了部分时卸载确实成功，这时出 Guidance 反而误导；④是诚实性修正，原来那句日志无条件打会对着「其实不弹框」的轮次撒谎 |
| **R17**（F-⑤） | 全局包预告的触发面用「`npm_prefix` 与 `lib/node_modules` 双向包含」而非字面 `== /usr/local`，且要求该目录本次确被放行 | **采纳** | 字面判据会漏掉 prefix 落在其内的形态，也会在 prefix 指向 nvm 目录时误报（全局包不住在要删的地方）；`prefix=None`（npm 不可用/超时）不触发，符合「无证据不猜」 |
| **R18**（F-⑥） | 改了 D 的 G6 PowerShell 模板：`-eq` / `StartsWith` → `[StringComparison]::Ordinal` | **采纳，属真实缺陷修复** | PS 默认是 culture-sensitive 比较，撇号/连字符在某些 culture 下按可忽略处理，会把只差标点的无关包判成托管方，全部判中时还会谎报「注册项全部指向管理器目录」。这是「第二套归一化规则」的实例，与 §9 R13 同类。**Windows 未验证**（无环境无 pwsh，纯静态推理），列进 A5/A6 补测 |
| **R19**（G 走查发现） | `getExecutionPlan` 的确认弹窗命令预览在 mac 上显示 **Windows 模板**（node 重装预览 `msiexec`、node 卸载预览 `powershell Get-Package`、git 预览 `Git-Installer`），与后端实际执行（`brew install node` / `installer -pkg` / CLT）完全不符 | **G 内顺手修复**（用户裁定），`5876c57` | `git diff main...HEAD` 证明是 main 既有缺陷、非 M0 回归；但 G 的「拒绝文案复核」正该覆盖「用户点确认前看到的承诺」这层——预览对破坏性动作做虚假陈述不可留。修法用文件内**已有**的 `isMac` 常量（`ui/app.js:35`）为 nodejs/git 分支 mac 文案，Windows 分支逐字不动；claudecode 卸载注释里 `%USERPROFILE%…claude.exe` 同步按平台给 `~/.local/bin/claude`。超 G「只读+README」原范围，经用户明确同意后并入 |
| **R20**（G 走查口径） | 走查两处与原清单的字面差异：① node 按钮是「重新安装」非「升级最新」（本机 nvm 生效 26.12.0 > LTS 24.22.0，`hasUpdate=false` 的自然渲染，R12 撤回后按钮行本就不受 `isManaged` 影响）；② A7 用 `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin` 重启同一二进制模拟，未跑打包 `.app` 双击 | **接受为等效验证，如实记录** | ①两个按钮走同一个 `install_target` 护栏通路，G3 横幅照出，R14 要钉的「托管 badge + 主行动按钮并存」成立；②launchd 环境的决定性变量就是那条默认 PATH，模拟与真 Finder 启动在探测层不可区分。真机 `open DevUp.app` 仍留在待验表，发版前用打包产物补一遍即可 |
| **R21**（G 收口期间用户追加） | 托管机器的安装/卸载点击不再先弹「确认执行」窗：后端把**护栏对当前渠道的预览判定**随卡片下发（`SoftwareStatus.guard_hint` / `uninstall_hint`，由 `node_source::guidance_preview` 调同一个 `check_install`/`check_uninstall` + `apply_target_version` 生成），会拒 → 黄色指引窗（reason/commands/note 零自造，逐条独立复制 + 「复制全部」），会放行（有 receipt 的卸载、brew 自管、mode=system）→ 普通确认窗原样 | **采纳（用户裁定「完整方案」）** | 「确认了个注定不会执行的动作」是体验缺陷，而前端**绝不能自己猜**会不会拒——预览必须就是判定函数的那一次调用；后端 `node_preflight` 仍是最终权威，弹窗分流不豁免任何实时判定。行距/文案两处微调系用户实点反馈（「命令分开单独复制」「弹窗显示不同的管理器命令」——后者确认安装侧本就逐 kind，与卸载侧同表同源） |
| **R22**（H 期间用户点破） | 卸载被 G5/G6 拒绝时，横幅/指引窗建议的是**安装族**命令（`nvm install 24.22.0` = DevUp 的目标版本，用户本机根本没装 24.x）——「别删」的现场教「装」，语义整个反转 | **修复，属真实缺陷** | `refuse()` 拆为 `refuse_with`，G5/G6 与两处脚本侧指引（Win G6 全命中、mac 卸载全跳过）切到新增的 `uninstall_hint_commands`：逐 kind 的**移除**表 `guard_uninstall_commands`（`{ver}` 填**托管方已装版本**——owner 命中优先 `env.version`，否则该 manager 自己 `versions[0]`，构建时就地填掉，不再借安装侧的 `apply_target_version` 管线）；无版本可填退化为只读 `nvm ls` 类命令，nvs 的移除别名语法没把握只给 `nvs ls`（与 install 侧「只写确有其事」同纪律）。**Windows 移除形态未实机验证**。测试：`uninstall_hint_commands_never_install` 全 kind 表驱动钉「无令牌残留 + 无安装命令 + 无缺参半成品」 |

> F 的一条方法论点值得留在台账里：本机 dump 显示 6 条候选**全部可删**（没有任何 `/usr/local` 路径链进 `~/.nvm`/`~/.volta`），而生产里 `pkg_receipt=false` 会让 G5 先拒绝——所以「本机跑不出跳过项」**不等于**「跳过逻辑验证过了」，跳过分支的证据只在合成 symlink 现场的表驱动单测里。它主动把这两层分开说了。

## 10. 交接（换设备继续时先读这一节）

### 分支与提交
分支 `feat/node-manager-coexistence`，已推送到 origin。每个任务两个提交（代码 + 文档状态）：

| 任务 | 代码提交 | 文档提交 |
|---|---|---|
| 步 1 PATH 合并顺序 | `7ffa93d` | （并入 `a7257a9`） |
| A `node.mode` 开关 | `4930f8f` | `046ae22` |
| B 所有权探测 | `24c6a9c` | `c4f46d4`、`dc5d137` |
| C Guidance 通路 | `ba89d60` | `897958a` |
| D 护栏 G1–G6 | `b442fdf` | `86404ec` |
| E 卡片显示托管方 | `33ac30e`、`e61afc8` | `8d01736` |
| F 卸载安全化 | `b4ff86e` | `0405d46`、`baeedc8` |
| G 回归与收口（含 R19 弹窗修复） | `5876c57` | 与本节同批 |
| H 指引直通弹窗（含 R22 卸载命令修正） | `f2f915b` | 与本节同批 |

**基线数字**（H 之后）：`cargo test` = **47 passed / 6 ignored**、`cargo build` = **0 warning**、`node --check ui/app.js` 通过；`node_source.rs` 的 `rustfmt --edition 2024 --check` 必须无输出。新设备上先跑这几个确认环境一致（macOS；node 在 `~/.nvm/versions/node/v26.12.0/bin`）。

### G、H 已完成——M0 全部任务收口，只剩发版
H（R21/R22）是 G 收口期间用户追加的：托管机器点安装/卸载直接弹管理器指引（预览=判定函数本身，会放行的动作仍是确认窗），卸载建议从安装族命令修正为各管理器的移除命令（版本=托管方已装那份）。两站弹窗经用户实点确认。
下面这段是 F 的原规格，留作记录；它已全部落地，三处裁定见 §9 R15–R18。**G 需要知道的接口变化**：
`MacInstaller::remove_pkg_node_files(&NodeEnv, &str, impl FnMut(String)) -> Result<PkgRemoval>`，`PkgRemoval { deleted, skipped, executed }`；`uninstall_target_macos("nodejs")` 对外契约不变（`Result<bool>`，`false` = 已出指引、**没弹框**、什么都没删）。`node_channel` 的 §2.2 口径问题 F 没碰，仍在原处。

**F 的规格（三件事）**：
1. **纯路径过滤**：新增 `pkg_removal_targets(&NodeEnv, &[PathBuf]) -> (可删, 因托管跳过)`，用 `node_source::path_key` + `is_within` 判断候选路径**自身或其 canonical 目标**是否落在任一 `manager.root` 内。核心用例是「候选在 `/usr/local/bin` 但符号链接指向 `~/.nvm/...` → 必须跳过」。`remove_pkg_node_files` 改为吃 `&NodeEnv`、动态拼 `rm -rf`，只删可删集合；**可删集合为空时不得调用 `run_admin_shell`**（那函数一调就弹 macOS 系统授权框，先弹框再解释是错的顺序）。
2. **一处诚实性**：`/usr/local/lib/node_modules` 里装着 DevUp 全局装的 claude / codex / pi / omp / opencode；当 `npm_prefix` 落在 `/usr/local` 时，执行前要 `log` 明「会一并移除全局包，建议先备份 `npm ls -g --depth=0`」。**只加日志，不改删除语义**。
3. **Windows 只做静态复核**：确认 G6 的 `InstallLocation` 过滤与 mac 过滤共用同一套归一化（不许出现第二套 `path_key`），不重写。无 Windows 环境与 pwsh，一律标「未验证」。

F 的禁区：不许真删文件、不许调用提权路径做"验证"、不碰 `ui/*` 与 `main.rs`、不许跑 `cargo fmt`/`rustfmt` 写操作（`macos.rs` 与 `installer/mod.rs` 是既有文件且含 `mod` 声明，格式化会外溢）。

### G 验收实录（2026-10-20，主会话亲自重跑 + 用户手动点击；截图暂存 `/tmp/devup-walkthrough/`）
1. **全量硬指标** ✅：`cargo test` = 43 passed / 6 ignored（原清单写「基线 39/5」是 F 之前的旧数，§10 基线段的 43/6 才是准）；`cargo build` = 0 warning；`node --check ui/app.js` 通过；`rustfmt --edition 2024 --check src/detector/node_source.rs` 无输出（注：本机 stable 工具链的 rustfmt 组件曾缺失，`rustup component add rustfmt` 后复验）。
2. **allow 残留 grep** ✅：只剩 `proxy.rs:20,63` + `updater.rs:3` 三条既有项；`node_source.rs:394` 的 `pub fn version_key` 为 R13 授权破例，原样保留。
3. **六探针实跑 + 文案复核** ✅：`dump_guard_decisions` 逐渠道打印本机判定——MacPkg 安装/卸载、MacBrewInstall 安装均 Refuse，WindowsMsi 双 Allow（本机 reparse/symlink 双 false，合规）；拒绝文案与 §3 模板同构（托管方 + `~/.nvm` 路径 + 当前版本 + `{ver}` 注入 24.22.0 + 逃生口 note）。
4. **GUI 实点走查** ✅（终端启动 debug 二进制；护栏均在下发/下载**之前**拒绝，全程零下载、零提权框、零删除）：
   - 首屏：node 卡 badge「托管中」+ `26.12.0 [nvm]` + `官方最新 v24.22.0` 照常展示；主行动按钮在场（本机因版本超前渲染为「重新安装」，等价通路，见 R20）；
   - 「重新安装→确认重装」→ 出**黄色**「DevUp 未代为执行」横幅，命中 **G3**（brew 可用故路由 MacBrewInstall；理由含 `~/.nvm` 与版本）——即 **A2 的通过标准**（brew 未装 node + managers 非空 → 拒绝）；
   - 复制按钮 → `pbpaste` 实测拿到 `nvm install 24.22.0`，wry 内剪贴板降级链路可用；
   - 「卸载→确认卸载」→ G5 拒绝横幅（`没有 org.nodejs.node.pkg receipt…不代为删除`），**A4 通过**；本机 `/usr/local/bin/node` 等 6 条候选本就不存在，探针「可删」是纯函数对静态清单的归属判定（F 设计如此），未发生任何删除即无落差；
   - **A7**：`env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin` 重启同二进制，node 卡显示「**由 nvm 托管 · v26.12.0**」而非「未安装」，按钮回落到「一键安装」（点击同样会被护栏接住）；
   - **其余卡片对照**：Homebrew 在受限环境经补充目录解析 `7.0.10 [brew]`（merge_path 顺序在真 app 生效）；Git `2.54.0 [system]` 两环境一致（R1 风险面未命中，本机无 brew git）；Claude Code 在两实例间 `[native_cli]↔[brew]` 跳变系外部多源安装（cmux shim/pnpm/nvm/brew cask 各有一份，cask 链接 18:12 刚建）+「用户 PATH 优先」新口径的共同结果，两实例跑的是同一份探测代码，**非 M0 回归**。
5. **发现并修复 R19**（用户裁定 G 内顺手修）：确认弹窗命令预览在 mac 显示 Windows 模板，`5876c57`。
6. **README** ✅：增「📌 行为变更（0.3.6）」小节（含两条硬事实：拒绝发生在下载之前；逃生口 `node.mode="system"` 手改 `~/Library/Application Support/devup/config.json`，Win `%APPDATA%\devup\config.json`）；旧句「安装/卸载 Node 必弹授权框」限定到「未检测到版本管理器的机器」。

### 仍未验证（发布前 checklist，别在文档里写成已验）
| 项 | 状态 |
|---|---|
| A3 装了 brew node 的 mac（`brew upgrade node` 应放行） | **待一台机器**。本机 `/opt/homebrew/bin` 与 `/usr/local/bin` 都没有 node，A1 端到端判别力受限 |
| A5/A6 Windows nvm-windows + 裸 MSI | **待验**（按评审决定 M0 先发 macOS）。`cargo check --target x86_64-pc-windows-msvc` 卡在 `aws-lc-sys` 缺 Windows SDK，本机根本轮不到我们的 crate。F 又把 G6 模板的比较改成 `[StringComparison]::Ordinal`（R18），这条也压在 A5/A6 一起补 |
| mac 卸载端到端 | **半验**。G 走查真机确认了「G5 拒绝时确实不弹框、不落删除」（A4 点击实录）；「有 receipt 的机器上过滤后的脚本被授权框正确执行」仍未验——本机根本没有 PKG 现场，需要一台 `org.nodejs.node.pkg` 在册的机器 |
| 跳过分支的真机证据 | 本机 6 条候选全判可删（没有 `/usr/local` 路径链进管理器目录），且 `pkg_receipt=false` 让 G5 先拒绝——**「本机跑不出跳过项」不等于「跳过逻辑验证过了」**，证据只在合成 symlink 单测里 |
| R8 设计稿遗留：MSI 遇 junction 的三种表现 | open。护栏不依赖是哪一种（一律拒写） |
| G3 已知代价 | 装了 brew node 却被 nvm 遮蔽的机器，`brew upgrade` 会被拒绝一次。已写进 `g3_brew_install` 的 doc，不改 |
| 前端「托管中 + 主行动按钮」组合 | G 已真机走查（badge「托管中」与「重新安装」并存，点击进护栏，见 R20）；「托管中 + 升级最新」的**字面**组合要等生效版本低于 LTS 的机器才能亲眼出现，仍无自动化覆盖（R14 维持） |
| `system` 逃生口的端到端 | 只有单元测试层，没跑过「改 config.json → 下次安装真的绕过」 |
| 移除命令形态（R22 新增表） | 本机实点验过 nvm 一种；fnm/mise/asdf/proto/n/scoop 形态照官方文档写，没有对应环境逐条实跑；Windows（nvm-windows/scoop）与 nvs 的只读退化整体未验证，压进 A5/A6 补测 |

### 协作约束（避免新会话重新踩，全文见 §7）
1. 子 agent 的自述**不作为验收依据**：M0 期间实测到三类与文件系统不符（半成品报全绿、`TaskStop` 后进程仍活着并继续投递、自述 `touch` 但 mtime 未变）。所有数字由主 agent 重跑 `cargo test` / `cargo build` / `git status` / `stat` 得出。
2. `TaskStop` 只摘任务句柄，**不终止 pane 的进程**；要彻底回收得手动关 pane。
3. 格式化判据：新建文件 `rustfmt --check` 必须无输出；既有文件一律不动，用 `git diff --stat` 的 **deletions 数量**判断有没有偷跑格式化（真跑过会是几十处删除）。
4. `rustfmt <文件>` 会跟随 `mod xxx;` 声明递归格式化子模块——**曾因此把 `env_check.rs` 整体重排**（已还原）。含 `mod` 声明的文件永远不进格式化清单。
5. 每任务两个提交；推送只在用户要求时做。
