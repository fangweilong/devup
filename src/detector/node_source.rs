//! Node 所有权探测：回答两个问题——「当前生效的 node 归谁」和「这台机器上装了哪些版本管理器」。
//!
//! 为什么要有这一层（背景见 `docs/design-node-manager-coexistence.md` §1）：用户本地装了
//! nvm / fnm / volta / mise 之类管理器时，DevUp 往系统位置写 runtime 会破坏对方自己的版本语义
//! （Windows 的 `C:\Program Files\nodejs` 甚至只是 nvm-windows 的 junction，MSI 会写穿它）。
//! 护栏（任务 D）拿本模块的结论做放行/拒绝决定，本模块只负责取证，**不做任何判定之外的动作**。
//!
//! 两条硬约束：
//! 1. **只读**：不 source 用户 rc、不调用任何 manager 命令、不写环境（设计文档 §12 R3）。
//!    GUI 进程（Finder/LaunchServices 启动）拿不到用户 shell 的 PATH 与环境变量，
//!    所以必须用 环境变量 / 已知目录 / rc 文本 / 路径归属 四路信号交叉取证，单一路径都不足以下结论。
//! 2. **采集与判定分离**：`detect()` 负责取证（读 env、读文件、跑只读子进程），
//!    `classify()` 是纯函数，全部依赖注入。没有这层拆分，表驱动单测就会依赖本机环境，
//!    并且并行运行的测试会互相抢环境变量。
//!
//! 遗留项：本 crate 是二进制 crate，`detect()` 还没有调用方（由任务 D 接入），rustc 因此
//! 把整棵调用树连类型带常量判为 dead code。用下面这一条模块级 `allow` 一次性兜住，而不是
//! 在几十个条目上逐个加注。**任务 D 接上 `detect()` 后必须删除本行 `#![allow(dead_code)]`**，
//! 否则以后真正死掉的代码会被它永久掩盖（任务 G 的验收包含
//! `grep -rn "allow(dead_code)" src/detector/` 必须为空这一条）。
//! `src/detector/mod.rs` 里那条 `#[allow(unused_imports)]` 同源同期清理。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 生效 node 的版本读取上限：本地执行 `node --version` 正常在几十毫秒级
const VERSION_TIMEOUT: Duration = Duration::from_millis(3000);
/// `npm config get prefix` 上限：npm 可能在等网络或钥匙串，卡住的探测不如没有探测
const NPM_TIMEOUT: Duration = Duration::from_millis(2000);
/// `pkgutil --pkgs` 上限：系统命令，慢也慢不到哪去，但不能阻塞 UI 刷新
const RECEIPT_TIMEOUT: Duration = Duration::from_millis(2000);

/// Windows 官方 MSI 的默认落点，也是 nvm-windows 默认接管（做成 junction）的目录
#[cfg(windows)]
const NODEJS_DIR_WINDOWS: &str = "C:\\Program Files\\nodejs";
/// 同一目录的归一化形态（见 `path_key`），归属判定用它，保证判定逻辑与平台分隔符无关
const WIN_NODEJS_DIR_KEY: &str = "c:/program files/nodejs";

/// macOS 官方 PKG 的落点目录
const USR_LOCAL_BIN_KEY: &str = "/usr/local/bin/";

/// Homebrew 根目录（Intel 的 `/usr/local` 版同时靠 `/cellar/` 命中，见 `owner_of`）
const BREW_ROOTS: &[&str] = &[
    "/opt/homebrew/",
    "/usr/local/homebrew/",
    "/home/linuxbrew/.linuxbrew/",
];

/// 会被扫描的 shell 启动文件（存在才读）。zsh 系优先，`~/.profile` 兼容 bash/sh 登录环境。
const RC_FILES: [&str; 4] = [".zprofile", ".zshrc", ".bashrc", ".profile"];

/// rc 文件匹配上限：正常 rc 只有几 KB，超大文件多半是误指向别处或被人塞了内容，不读
const RC_MAX_BYTES: u64 = 512 * 1024;

/// 机器上可能存在的 Node 版本管理器。`Nvm` 与 `NvmWindows` 是两个不同实现（目录结构、
/// 生效方式、提权要求都不同），不能合成一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerKind {
    Nvm,
    NvmWindows,
    Fnm,
    Volta,
    Mise,
    Asdf,
    Proto,
    N,
    Scoop,
    Nvs,
}

/// 当前生效的那份 node 归谁。描述的是「生效者」，不是「装过什么」——装了 nvm 但生效的是
/// brew node 很常见，所以它与 `NodeEnv::managers` 不等价（设计文档 §2 的设计约束）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeOwner {
    /// GUI 解析不到可用 node（不代表用户终端里没有）
    #[default]
    None,
    /// macOS 官方 PKG / Windows 官方 MSI 装在系统位置
    SystemPkg,
    /// 来自 Homebrew
    Brew,
    /// 由某个版本管理器托管
    Manager(ManagerKind),
    /// 有 node 但认不出来源，按保守处理（护栏侧等同 Manager 对待，见设计文档 D2）
    Unknown,
}

/// 一个已检测到的管理器实例：kind + 数据根目录 + 它自己装了哪些版本。
///
/// 版本挂在管理器身上而不是拍平到 `NodeEnv` 顶层：一台机器可以同时有 nvm 和 volta
/// （本机即如此），合并成一个列表就再也回答不了「26.12.0 是谁的」。
#[derive(Clone, Debug, PartialEq)]
pub struct Manager {
    pub kind: ManagerKind,
    pub root: PathBuf,
    /// 该管理器数据目录里实际存在的版本，语义版本降序，形如 "26.12.0"（展示方自行补 `v`）。
    /// 空列表有两种成因，调用方无法在本结构里区分：真没装版本，或该 kind 的目录布局未知
    /// （`Proto` / `Nvs` 不收集，见 `version_dir`）。
    pub versions: Vec<String>,
}

impl Manager {
    /// 采集期的构造：三路信号只负责认出「有没有人在托管」，版本由后面的
    /// `attach_versions` 填进来（扫目录属于文件系统动作，不进这些纯函数）。
    fn new(kind: ManagerKind, root: PathBuf) -> Manager {
        Manager {
            kind,
            root,
            versions: Vec::new(),
        }
    }
}

/// 一次探测的完整结论，供护栏决策、卡片展示、事后回验共用
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeEnv {
    pub owner: NodeOwner,
    /// 生效 node 的 canonical 路径（穿透 symlink/junction 后的真实位置）
    pub active_node: Option<PathBuf>,
    /// 生效 node 的版本，清洗后形如 "24.22.0"
    pub version: Option<String>,
    /// 机器上所有已检测到的管理器（可能多个并存，本机等即是），各自带自己的版本列表
    pub managers: Vec<Manager>,
    /// **仅**当 `owner = Manager(k)` 时为那一个管理器的版本（降序），其余情况为空。
    /// 逐管理器分别回答请直接遍历 `managers`，owner 命中哪一个用 `owner_manager()`。
    pub managed_versions: Vec<String>,
    /// `npm config get prefix` 的结果；**不参与 M0 的任何判定**，供护栏与 M3 使用
    pub npm_prefix: Option<PathBuf>,
    /// Windows：`C:\Program Files\nodejs` 是 reparse point（被 nvm 之类接管）
    pub nodejs_dir_is_reparse: bool,
    /// Windows：`NVM_SYMLINK` 正好指向 `C:\Program Files\nodejs`。
    ///
    /// 与上一条是 G1 的两个独立证据：链接属性可能被 MSI 写掉而变成实体目录，
    /// 但 nvm-windows 的配置仍指着它（设计文档 §3.3 的「链接被替换」修复场景）。
    pub nvm_symlink_is_nodejs_dir: bool,
    /// macOS：`org.nodejs.node.pkg` receipt 已登记（`/usr/local` 那份确实由 PKG 装入）
    pub pkg_receipt: bool,
}

/// `classify()` 的全部输入：由 `detect()` 采集而来，字段本身不含任何「读环境」的能力。
/// 结构体而非散参数，是为了让表驱动单测能整体构造一份合成现场。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeProbeInput {
    /// 三路信号（env / 已知目录 / rc）合并去重后的管理器，各自的版本已填好。
    /// 版本的唯一归属地就是这里，`NodeEnv` 顶层那份是从中挑出来的视图。
    pub managers: Vec<Manager>,
    /// `which("node")` 且 canonicalize 后的生效 node（canonicalize 失败时为原路径）
    pub active_node: Option<PathBuf>,
    pub version: Option<String>,
    pub npm_prefix: Option<PathBuf>,
    pub nodejs_dir_is_reparse: bool,
    /// `NVM_SYMLINK` 是否正好指向 `C:\Program Files\nodejs`（G1 的独立证据）
    pub nvm_symlink_is_nodejs_dir: bool,
    pub pkg_receipt: bool,
}

impl NodeEnv {
    /// `owner` 命中某个管理器时返回那一个，其余（`None` / `Unknown` / `Brew` / `SystemPkg`）返回 `None`。
    ///
    /// 逐管理器渲染请**直接遍历 `NodeEnv::managers`**——版本就挂在每个 `Manager` 身上，
    /// 那里是唯一真相源，不要再包一层「带版本的 managers」别名访问器。
    /// 空 `versions` 的条目同样要渲染：「装了 nvm 但一个版本都没」是决策矩阵规则 6 的输入
    /// （补齐他管理器里的第一个版本），过滤掉就看不见这个形态了。
    pub fn owner_manager(&self) -> Option<&Manager> {
        match self.owner {
            NodeOwner::Manager(kind) => self.managers.iter().find(|m| m.kind == kind),
            _ => None,
        }
    }
}

impl ManagerKind {
    /// 该 kind 未从环境/rc 里解析出路径时使用的默认数据根目录。
    ///
    /// 仅 `NvmWindows` / `Nvs` 会读 `APPDATA` 环境变量（它们只有 Windows 形态，在 mac 上
    /// 永不被调用），其余 kind 只依赖传入的 home，保持可注入测试。
    pub fn default_root(self, home: &Path) -> PathBuf {
        let appdata = || -> PathBuf {
            #[cfg(windows)]
            {
                if let Some(a) = std::env::var_os("APPDATA") {
                    return PathBuf::from(a);
                }
            }
            home.join("AppData").join("Roaming")
        };
        match self {
            ManagerKind::Nvm => home.join(".nvm"),
            ManagerKind::NvmWindows => appdata().join("nvm"),
            ManagerKind::Fnm => fnm_default_dir(home),
            ManagerKind::Volta => home.join(".volta"),
            ManagerKind::Mise => home.join(".local").join("share").join("mise"),
            ManagerKind::Asdf => home.join(".asdf"),
            ManagerKind::Proto => home.join(".proto"),
            // `n` 的默认前缀就是 /usr/local（与官方 PKG 同地盘，见设计文档 §1.3）
            ManagerKind::N => PathBuf::from("/usr/local"),
            ManagerKind::Scoop => home.join("scoop").join("apps").join("nodejs"),
            ManagerKind::Nvs => appdata().join("nvs"),
        }
    }

    /// 该 kind 存放「已安装版本」的目录（版本名取自目录名）。
    ///
    /// `Proto` / `Nvs` 的目录布局没有把握，返回 `None` 表示本阶段不收集它们的版本，
    /// 而不是「没装版本」——M0 不因此误报，需要时再由后续任务补（见交付报告）。
    fn version_dir(self, root: &Path) -> Option<PathBuf> {
        match self {
            // ~/.nvm/versions/node/v26.12.0
            ManagerKind::Nvm => Some(root.join("versions").join("node")),
            // %APPDATA%\nvm\v24.22.0（版本目录直接挂在根下）
            ManagerKind::NvmWindows => Some(root.to_path_buf()),
            // $FNM_DIR/node-versions/v22.11.0/installation
            ManagerKind::Fnm => Some(root.join("node-versions")),
            // ~/.volta/tools/image/node/24.22.0
            ManagerKind::Volta => Some(root.join("tools").join("image").join("node")),
            ManagerKind::Mise => Some(root.join("installs").join("node")),
            ManagerKind::Asdf => Some(root.join("installs").join("nodejs")),
            ManagerKind::Proto => None,
            // N_PREFIX/lib/node_modules/n/versions/node/24.22.0
            ManagerKind::N => Some(
                root.join("lib")
                    .join("node_modules")
                    .join("n")
                    .join("versions")
                    .join("node"),
            ),
            // ~/scoop/apps/nodejs/<version>（current 是 junction，被版本名规则自然过滤）
            ManagerKind::Scoop => Some(root.to_path_buf()),
            ManagerKind::Nvs => None,
        }
    }
}

/// macOS 上 fnm 的默认数据目录是 `~/Library/Application Support/fnm`，Linux 是 `~/.fnm`。
/// 两者都探一遍更稳（用户可能手工设过 FNM_DIR，env 那一路会覆盖）。
#[cfg(target_os = "macos")]
fn fnm_default_dir(home: &Path) -> PathBuf {
    home.join("Library").join("Application Support").join("fnm")
}

#[cfg(not(target_os = "macos"))]
fn fnm_default_dir(home: &Path) -> PathBuf {
    home.join(".fnm")
}

/// `n` 的托管目录取「版本目录」而不是整个 `N_PREFIX`。
///
/// 若直接用 `/usr/local`，官方 PKG 装的 `/usr/local/bin/node` 会被前缀比对判成
/// `Manager(N)`（因为 PKG 落点天然在 n 的前缀里）。让 n 只认领它真正的地盘
/// `<prefix>/lib/node_modules/n/versions/node`，PKG 归 PKG；而 n 生效时
/// `/usr/local/bin/node` 是指向版本目录的软链，canonicalize 后照样命中。
/// PKG 与 n 抢同一目录这件事本身由护栏 G4 专门处理。
fn n_manager_root(prefix: &Path) -> PathBuf {
    let versions = ManagerKind::N
        .version_dir(prefix)
        .expect("N 已登记版本目录");
    if versions.is_dir() {
        versions
    } else {
        // 还没有任何 `n install` 过的版本目录，退回前缀本身，至少 rc/env 这条证据不丢
        prefix.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// 纯字符串路径逻辑（跨平台共用，Windows 用例也能在 macOS 上跑同一张表）
// ---------------------------------------------------------------------------

/// 路径归一化，用于「是否落在某目录下」的比较：`\` 与 `/` 同视、连续分隔符折叠为一个、
/// 去尾部分隔符、大小写折叠。不读写文件系统。
///
/// 大小写折叠的理由：Windows 与 macOS 默认卷（APFS 大小写不敏感）都不区分大小写；
/// Linux 上把管理器目录写成不同大小写的情况几乎不存在，而判宽的代价是「多认一份托管」，
/// 方向与护栏的保守性一致（宁可少给功能，不可写坏别人目录）。
pub fn path_key(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut prev_sep = false;
    for ch in p.chars() {
        let sep = ch == '/' || ch == '\\';
        if sep {
            // 连续分隔符只留一个，统一落成 `/`
            if !prev_sep {
                out.push('/');
            }
            prev_sep = true;
        } else {
            for l in ch.to_lowercase() {
                out.push(l);
            }
            prev_sep = false;
        }
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// `cand_key` 是否位于 `root_key` 目录内（含等于自身）。两个参数都必须先过 `path_key`。
///
/// 关键细节：不能只用 `starts_with`，否则 `/usr/local2/node` 会被判进 `/usr/local`。
///
/// 公开给任务 D 复用：护栏 G1/G2/G6 的判据同样是「目标路径 canonical 后是否落在某个
/// `manager.root` 内」，两边必须共用一套归一化与边界规则，各写一份迟早漂移成两种结论。
/// 用法：`is_within(&path_key(目标), &path_key(管理器根))`。
pub fn is_within(cand_key: &str, root_key: &str) -> bool {
    if root_key.is_empty() {
        return false;
    }
    if root_key == "/" {
        return cand_key.starts_with('/');
    }
    cand_key == root_key
        || (cand_key.starts_with(root_key)
            && cand_key.as_bytes().get(root_key.len()) == Some(&b'/'))
}

/// 两段路径是否指向同一位置（归一化后相等）。reparse 判定的纯逻辑部分：
/// 「canonicalize 的结果与自身不相等」即说明这条路径是链接而不是实体。
fn same_path(a: &str, b: &str) -> bool {
    path_key(a) == path_key(b)
}

// ---------------------------------------------------------------------------
// 版本字符串处理
// ---------------------------------------------------------------------------

/// 把目录名/命令输出清洗成裸版本号（`v26.12.0` -> `26.12.0`）。
///
/// 与 `Detector::clean_version_string` 同口径（那边面向任意 CLI 输出，这里只面向
/// 版本目录名，因此要求更严：任一自然段非纯数字就不认，`latest` / `current` /
/// `installation` / `aliases` 这些别名目录不会被当成版本）。
fn clean_version_name(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches(['v', 'V']);
    if s.is_empty() || !s.contains('.') {
        return None;
    }
    let ok = s
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()));
    if ok { Some(s.to_string()) } else { None }
}

/// 版本比较键：按 `.` 切段转 u64 后整体逐段比较。字典序会把 `v9` 排到 `v26` 之前，
/// 必须按数值逐段比（`[26]` > `[9]`）；段数不同时短的视为更小（`24.22.0` > `24`）。
fn version_key(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|seg| seg.parse::<u64>().unwrap_or(0))
        .collect()
}

fn cmp_version_desc(a: &String, b: &String) -> std::cmp::Ordering {
    version_key(b).cmp(&version_key(a))
}

/// 原地按语义版本降序排序并去重（输入可能来自多个管理器目录的合并）
fn sort_versions_desc(v: &mut Vec<String>) {
    v.sort_by(cmp_version_desc);
    v.dedup();
}

// ---------------------------------------------------------------------------
// 归属判定（纯函数）
// ---------------------------------------------------------------------------

/// 给定采集输入产出结论。**不读环境变量、不碰文件系统、不起进程**——一切依赖都在参数里。
pub fn classify(input: &NodeProbeInput) -> NodeEnv {
    let owner = match input.active_node.as_ref() {
        None => NodeOwner::None,
        Some(node) => owner_of(node, &input.managers, input.nodejs_dir_is_reparse),
    };
    NodeEnv {
        owner,
        active_node: input.active_node.clone(),
        version: input.version.clone(),
        managers: input.managers.clone(),
        managed_versions: pick_managed_versions(owner, &input.managers),
        npm_prefix: input.npm_prefix.clone(),
        nodejs_dir_is_reparse: input.nodejs_dir_is_reparse,
        nvm_symlink_is_nodejs_dir: input.nvm_symlink_is_nodejs_dir,
        pkg_receipt: input.pkg_receipt,
    }
}

/// 生效 node 的路径归属判定，顺序即优先级（设计文档 §3.1 第 4 路信号）。
///
/// 管理器前缀比对排在最前：`active_node` 已 canonicalize，Windows 上
/// `C:\Program Files\nodejs` 的 junction 会被解析到 `%APPDATA%\nvm\vX`，
/// 于是「看起来在系统目录、其实是 nvm 的」这种形态能被识破。
fn owner_of(node: &Path, managers: &[Manager], nodejs_dir_is_reparse: bool) -> NodeOwner {
    let key = path_key(&node.to_string_lossy());

    for m in managers {
        if is_within(&key, &path_key(&m.root.to_string_lossy())) {
            return NodeOwner::Manager(m.kind);
        }
    }

    // brew 的 node 无论软链（/opt/homebrew/bin/node）还是实体（Cellar 内）都能命中
    if key.contains("/cellar/") || BREW_ROOTS.iter().any(|r| key.starts_with(r)) {
        return NodeOwner::Brew;
    }

    // Windows 系统目录：目录本身是 reparse point 时不能判 SystemPkg——那说明它不是
    // MSI 的实体安装，而是别人（多半 nvm-windows）的链接，只是我们的管理器证据没接住。
    // 判 Unknown 比判 SystemPkg 安全：护栏会按「不许写系统目录」处理。
    if key.starts_with(WIN_NODEJS_DIR_KEY) {
        return if nodejs_dir_is_reparse {
            NodeOwner::Unknown
        } else {
            NodeOwner::SystemPkg
        };
    }

    // macOS 官方 PKG 落点。是否真由 PKG 装入由 `pkg_receipt` 决定（G4/G5 的判据），
    // 这里只回答「生效的这份位于系统目录」。
    if key.starts_with(USR_LOCAL_BIN_KEY) {
        return NodeOwner::SystemPkg;
    }

    NodeOwner::Unknown
}

/// 顶层 `managed_versions` 只回答一个问题：「**生效那份**归的管理器里装了哪些版本」。
///
/// owner 落在某个管理器上时取那一个管理器的版本；其余情况一律留空。
/// 刻意不做「owner 定位不到就合并全部管理器版本」的退化路径——合并列表会丢掉版本归属，
/// 而 nvm+volta 共存时给用户看一眼合并结果，比不给看更容易误导（他会以为 volta 的
/// 20.15.0 也是当前托管方能切过去的版本）。分管理器逐个回答请直接遍历 `NodeEnv::managers`，
/// 那份数据本来就在 `managers` 里，不需要在这里再造一个视图。
fn pick_managed_versions(owner: NodeOwner, managers: &[Manager]) -> Vec<String> {
    match owner {
        NodeOwner::Manager(kind) => managers
            .iter()
            .find(|m| m.kind == kind)
            .map(|m| m.versions.clone())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 采集：环境变量 / 已知目录 / shell rc
// ---------------------------------------------------------------------------

/// 环境变量 -> ManagerKind 的对照表。
///
/// 不在此表的是 `NVM_SYMLINK`：它不是管理器数据目录，而是 nvm-windows 的链接落点，
/// 单独作为 `nvm_symlink_is_nodejs_dir` 事实采集（G1 证据）。
const ENV_MANAGER_KEYS: &[(&str, ManagerKind)] = &[
    ("NVM_DIR", ManagerKind::Nvm),
    ("FNM_DIR", ManagerKind::Fnm),
    ("FNM_MULTISHELL_PATH", ManagerKind::Fnm),
    ("VOLTA_HOME", ManagerKind::Volta),
    ("MISE_DATA_DIR", ManagerKind::Mise),
    ("ASDF_DATA_DIR", ManagerKind::Asdf),
    ("PROTO_HOME", ManagerKind::Proto),
    ("N_PREFIX", ManagerKind::N),
];

/// 从环境变量值构造管理器列表（纯函数：env 读取与平台都作为参数注入）。
///
/// 两处必要的形状修正：
/// - `FNM_MULTISHELL_PATH` 是 fnm 为每个 shell 临时造的 `.../fnm/multishell/<uuid>`，
///   直接当根目录会让真正的版本目录 `node-versions` 落在它外面，因此截断到 `multishell` 之前；
/// - nvm-windows 的安装器也会设 `NVM_DIR`（指向 `%APPDATA%\nvm`），但那是另一个实现
///   （junction + `nvm use` 可能需提权），所以 `is_windows` 为真时 `Nvm` 一律换成 `NvmWindows`。
fn managers_from_env_with(
    home: &Path,
    is_windows: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<Manager> {
    let mut out = Vec::new();
    for (key, kind) in ENV_MANAGER_KEYS {
        let Some(raw) = lookup(key) else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut root = expand_tilde(raw, home);
        if *kind == ManagerKind::Fnm {
            root = truncate_at_component(&root, "multishell");
        }
        if *kind == ManagerKind::N {
            root = n_manager_root(&root);
        }
        let kind = if is_windows && *kind == ManagerKind::Nvm {
            ManagerKind::NvmWindows
        } else {
            *kind
        };
        out.push(Manager::new(kind, root));
    }
    out
}

/// 把路径截到某个目录组件之前（`.../fnm/multishell/ABCD` -> `.../fnm`）；找不到则原样返回。
///
/// 用 `ancestors()` 而不是字符串下标切片：字节偏移与字符偏移在含非 ASCII 的路径上会错位。
fn truncate_at_component(p: &Path, component: &str) -> PathBuf {
    for anc in p.ancestors() {
        let hit = anc
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case(component))
            .unwrap_or(false);
        if hit {
            return anc
                .parent()
                .filter(|x| !x.as_os_str().is_empty())
                .map(|x| x.to_path_buf())
                .unwrap_or_else(|| p.to_path_buf());
        }
    }
    p.to_path_buf()
}

/// 已知目录探测（纯判定，目录是否存在由参数注入）。
///
/// 这一路是 GUI 进程的主要证据来源：Finder 启动时 `NVM_DIR` 之类根本不存在，
/// 只有目录本身不会骗人。
fn managers_from_dirs_with(home: &Path, exists: impl Fn(&Path) -> bool) -> Vec<Manager> {
    known_dirs(home)
        .into_iter()
        .filter(|(_, p)| exists(p))
        .map(|(kind, root)| Manager::new(kind, root))
        .collect()
}

/// 已知目录表（绝对路径）：两平台通用项 + 当前平台专有项。
///
/// 分成两个函数而不是在 `vec!` 元素上写 `#[cfg]`，是因为 Windows 专有条目必须能被
/// 本机（macOS）的编译排除掉，同时不给非 Windows 构建留下 `mut` 之类的多余形态。
///
/// 两个刻意的缺席：Windows 版 fnm 的默认目录不在表内（布局没把握，由 `FNM_DIR` 覆盖）；
/// `n` 完全不参与目录探测——它的默认前缀就是 `/usr/local`，每条 mac 都有这个目录，
/// 按目录探 `n` 会把所有官方 PKG 用户误判成托管状态，所以 n 只认 env 与 rc 两条证据。
fn known_dirs(home: &Path) -> Vec<(ManagerKind, PathBuf)> {
    common_known_dirs(home)
        .into_iter()
        .chain(platform_known_dirs(home))
        .collect()
}

fn common_known_dirs(home: &Path) -> Vec<(ManagerKind, PathBuf)> {
    vec![
        (ManagerKind::Nvm, home.join(".nvm")),
        (ManagerKind::Fnm, fnm_default_dir(home)),
        (ManagerKind::Volta, home.join(".volta")),
        (
            ManagerKind::Mise,
            home.join(".local").join("share").join("mise"),
        ),
        (ManagerKind::Asdf, home.join(".asdf")),
        (ManagerKind::Proto, home.join(".proto")),
        (
            ManagerKind::Scoop,
            home.join("scoop").join("apps").join("nodejs"),
        ),
    ]
}

/// nvm-windows / nvs 的数据目录在 `%APPDATA%` 下（home 即 `%USERPROFILE%`）；
/// 这些路径在 mac 上没有意义，因此不进通用表。**Windows 分支本机未验证。**
#[cfg(windows)]
fn platform_known_dirs(home: &Path) -> Vec<(ManagerKind, PathBuf)> {
    vec![
        (
            ManagerKind::NvmWindows,
            home.join("AppData").join("Roaming").join("nvm"),
        ),
        (
            ManagerKind::Nvs,
            home.join("AppData").join("Roaming").join("nvs"),
        ),
    ]
}

#[cfg(not(windows))]
fn platform_known_dirs(_home: &Path) -> Vec<(ManagerKind, PathBuf)> {
    Vec::new()
}

/// rc 匹配表：`(片段, kind, 是否从同一行取路径)`。
///
/// 片段一律小写，匹配时把整行 lowercase 后做 `contains`；第三项为 `true` 的片段本身以
/// `=` 结尾，会从同一行 `=` 右侧取路径（`export NVM_DIR="$HOME/.nvm"`），取不到就退回该
/// kind 的默认根目录。刻意只做字符串匹配，绝不 source、绝不执行（见文件头约束 1）。
///
/// 表里没有 `scoop` / `nvs` 的片段（它们在 rc 里激活的样子太多变，交给已知目录与 env 取证）；
/// `n` 反过来只有 rc 这一路（它的默认前缀 `/usr/local` 每条 mac 都有，不能按目录探测）。
const RC_PATTERNS: &[(&str, ManagerKind, bool)] = &[
    ("export nvm_dir=", ManagerKind::Nvm, true),
    ("nvm.sh", ManagerKind::Nvm, false),
    // oh-my-zsh 的 zsh-nvm 插件：用户 rc 里只有插件名，既没有 NVM_DIR 也没有 nvm.sh
    ("zsh-nvm", ManagerKind::Nvm, false),
    ("volta_home=", ManagerKind::Volta, true),
    (".volta/bin", ManagerKind::Volta, false),
    ("fnm env", ManagerKind::Fnm, false),
    ("fnm shell", ManagerKind::Fnm, false),
    ("fnm_dir=", ManagerKind::Fnm, true),
    ("mise activate", ManagerKind::Mise, false),
    ("mise_data_dir=", ManagerKind::Mise, true),
    ("asdf_data_dir=", ManagerKind::Asdf, true),
    ("proto_home=", ManagerKind::Proto, true),
    ("export n_prefix=", ManagerKind::N, true),
];

/// 从一行 rc 文本识别管理器（纯函数）。注释行跳过——`# export NVM_DIR=...` 是被用户
/// 主动停用的配置，把它算成托管证据会凭空拦住一次正常安装。
fn rc_match_line(line: &str, home: &Path) -> Option<Manager> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let lower = t.to_lowercase();
    for (pat, kind, want_path) in RC_PATTERNS {
        if !lower.contains(pat) {
            continue;
        }
        // 取路径的片段一律以 `=` 结尾，因此「变量引用」形态（`$PROTO_HOME/bin`、
        // `${VOLTA_HOME}`）天然不会命中——读它不等于设它。不需要额外排除逻辑。
        let root = if *want_path {
            value_after_eq(t)
                .map(|v| expand_tilde(&v, home))
                .unwrap_or_else(|| kind.default_root(home))
        } else {
            kind.default_root(home)
        };
        // 刻意不按平台改写 kind：被扫描的是 `.zshrc` / `.bashrc` 这类 POSIX shell 文件，
        // 里面出现 `nvm.sh` / `export NVM_DIR=` 的就是 Unix 版 nvm（nvm-windows 不写 rc，
        // 它靠注册表环境变量）。判定与运行平台无关，同一张表在两个 OS 上都成立。
        return Some(Manager::new(*kind, root));
    }
    None
}

/// 取一行里第一个 `=` 右侧的值。
///
/// 两种结束规则必须分开处理，否则都会拿到错的根目录：
/// - 带引号：截到配对引号为止——路径里**可以有空格**（`"$HOME/Library/Application Support/fnm"`），
///   按空白切会把路径腰斩；
/// - 不带引号：第一个空白或 `;` 结束（`export NVM_DIR=$HOME/.nvm; export PATH=...`），
///   不处理 `;` 会把分号带进路径变成一个不存在的目录名。
fn value_after_eq(line: &str) -> Option<String> {
    let (_, rest) = line.split_once('=')?;
    let rest = rest.trim();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    let v = match quote {
        Some(q) => {
            let tail = &rest[1..];
            match tail.find(q) {
                Some(end) => &tail[..end],
                // 未闭合的引号（多行续写）：取到行尾，交给 `expand_tilde` 处理
                None => tail,
            }
        }
        None => rest.split([' ', '\t', ';']).next().unwrap_or(rest),
    };
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// 展开 `~` / `$HOME` / `${HOME}`。其余原样返回（相对路径按 home 解析）。
///
/// 不尝试解析 `$VOLTA_HOME` 之类的变量引用（那需要实现一个 shell 变量表），
/// 交回默认根目录即可——探测的目标是「有没有人在托管」，不是还原完整 rc 语义。
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        return home.join(rest);
    }
    if s == "~" {
        return home.to_path_buf();
    }
    for var in ["$HOME/", "${HOME}/", "$HOME", "${HOME}"] {
        if let Some(rest) = s.strip_prefix(var) {
            let rest = rest.trim_start_matches(['/']);
            return if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            };
        }
    }
    PathBuf::from(s)
}

/// **只读**扫描 shell rc 文件，识别用户在自己的 shell 里激活了哪些管理器。
///
/// Finder 启动的 GUI 进程拿不到 shell 环境，这是唯一能还原「用户终端里其实有 nvm」的手段。
/// 读到的内容只做字符串匹配（见 `RC_PATTERNS`），绝不 source、绝不执行——避免任意代码执行
/// 与副作用（设计文档 §12 R3）。
pub fn scan_shell_rc(home: &Path) -> Vec<Manager> {
    let mut out = Vec::new();
    for name in RC_FILES {
        let path = home.join(name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        // 只认普通文件：目录 / FIFO 之类不该被当 rc 读（读 FIFO 会直接把探测卡死）
        if !meta.is_file() || meta.len() > RC_MAX_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            // 非 UTF-8 的 rc 直接跳过：宁可不认这个管理器，也不引入 lossy 解码的误判面
            continue;
        };
        for line in text.lines() {
            if let Some(m) = rc_match_line(line, home) {
                out.push(m);
            }
        }
    }
    dedup_kinds(out)
}

// ---------------------------------------------------------------------------
// 采集：node 本体 / 子进程事实
// ---------------------------------------------------------------------------

/// Unix：复用 `platform::which`（其顺序已与 `child_path()` 一致，用户 PATH 优先）。
/// Windows：本模块不改动 `platform.rs`，而 `platform::which` 只有 unix 实现，
/// 因此这里用 `where.exe` 自取一条（与 `Detector::check_command_windows` 同思路）。
///
/// **Windows 分支本机未验证**（无 Windows 环境）。
#[cfg(unix)]
fn find_active_node() -> Option<PathBuf> {
    crate::platform::which("node")
}

#[cfg(windows)]
fn find_active_node() -> Option<PathBuf> {
    let child_path = crate::platform::child_path();
    let mut cmd = Command::new("where.exe");
    cmd.arg("node.exe");
    cmd.env("PATH", &child_path);
    cmd.creation_flags(crate::detector::CREATE_NO_WINDOW);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // 先把 Cow 绑定下来再取行：在一行表达式里从临时值借出 &str 会超出临时值存活范围
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(PathBuf::from(line))
}

/// canonicalize 是为了穿透 symlink/junction：Windows 上 nvm 的 `Program Files\nodejs`
/// 链接、macOS 上 brew 的 `bin/node` 软链，都必须还原成真实位置才能做归属前缀比对。
/// 解析失败时退回原路径——拿不到真实目标不等于「没有 node」。
fn resolve_real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// 跑一条只读命令并返回 stdout（失败或超时返回 `None`，安静降级，不 panic）。
///
/// 用 `spawn` + `try_wait` 轮询而不是 `Command::output()`：std 的 `output()` 没有超时能力，
/// 而 npm 这类命令在用户机器上可能等网络、等钥匙串，一次刷新绝不能把 DevUp 卡住。
/// 前提：只用于输出很小的命令（版本号 / prefix / 包 id 列表）——大输出会填满管道缓冲，
/// 使子进程阻塞到超时，那类命令不应走这里。
fn run_captured(program: &Path, args: &[&str], limit: Duration) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("PATH", crate::platform::child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // GUI 进程派生的子进程不该在用户桌面上闪出控制台窗口（与 env_check 侧同一约束）
    #[cfg(windows)]
    command.creation_flags(crate::detector::CREATE_NO_WINDOW);
    let mut child = command.spawn().ok()?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if started.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    };
    if !status.success() {
        return None;
    }
    let out = child.stdout.take()?;
    let text = std::io::read_to_string(&mut std::io::BufReader::new(out)).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 生效 node 的版本。直接执行解析到的那份 node 的**绝对路径**，不靠 PATH——
/// 我们关心的正是「这一份是什么版本」。
fn read_node_version(node: &Path) -> Option<String> {
    run_captured(node, &["--version"], VERSION_TIMEOUT)
        .map(|raw| crate::detector::Detector::clean_version_string(&raw))
}

/// npm 可执行文件：优先用与生效 node 同目录的 npm（保证 node/npm/prefix 同源，
/// 设计文档 §8 的第 1 条同源原则在这里先落到探测侧）。
#[cfg(unix)]
fn find_npm(node: Option<&Path>) -> Option<PathBuf> {
    let sibling = node?.parent()?.join("npm");
    if sibling.is_file() {
        return Some(sibling);
    }
    crate::platform::which("npm")
}

#[cfg(windows)]
fn find_npm(node: Option<&Path>) -> Option<PathBuf> {
    // npm 在 Windows 上是 npm.cmd 批处理包装，需要 cmd.exe 才能执行
    let dir = node?.parent()?;
    for name in ["npm.cmd", "npm.exe"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `npm config get prefix`：失败/超时留 `None`。M0 不参与任何判定，只为护栏与
/// M3 的前缀统一预留事实（设计文档 §8）。
/// 跑 `npm config get prefix` 取原始输出。分平台的原因是调用形态，不是逻辑：
/// Windows 上 npm 是 `npm.cmd` 批处理包装，必须经 cmd.exe 才能执行。
#[cfg(windows)]
fn npm_prefix_text(npm: &Path) -> Option<String> {
    let lossy = npm.to_string_lossy().to_string();
    run_captured(
        Path::new("cmd"),
        &["/c", lossy.as_str(), "config", "get", "prefix"],
        NPM_TIMEOUT,
    )
}

#[cfg(not(windows))]
fn npm_prefix_text(npm: &Path) -> Option<String> {
    run_captured(npm, &["config", "get", "prefix"], NPM_TIMEOUT)
}

/// `npm config get prefix`：失败/超时留 `None`。M0 不参与任何判定，只为护栏与
/// M3 的前缀统一预留事实（设计文档 §8）。
fn read_npm_prefix(node: Option<&Path>) -> Option<PathBuf> {
    let npm = find_npm(node)?;
    npm_prefix_text(&npm).map(PathBuf::from)
}

/// macOS：`/usr/local` 那份 node 是否真由官方 PKG 装入。
/// 失败一律当 false 处理：认不出 receipt 的代价是护栏多拦一次，
/// 反过来（误判 true）会让卸载走 `rm -rf` 删掉别人装的东西。
#[cfg(target_os = "macos")]
fn read_pkg_receipt() -> bool {
    run_captured(Path::new("pkgutil"), &["--pkgs"], RECEIPT_TIMEOUT)
        .map(|s| {
            s.lines()
                .any(|l| l.trim().starts_with("org.nodejs.node.pkg"))
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn read_pkg_receipt() -> bool {
    false
}

/// Windows：`(C:\Program Files\nodejs 是否 reparse, NVM_SYMLINK 是否指向它)`。
///
/// 零依赖做法（不引入 `windows` crate / `GetFileAttributesW`）：reparse point 被
/// `canonicalize` 解析到目标，于是「归一化后与自身不相等」即是链接。代价是分不清
/// symlink 与 junction，但对护栏来说两者都不能写，不需要区分。
///
/// **Windows 分支本机未验证**（无 Windows 环境），G1 的真机结论待补测。
#[cfg(windows)]
fn read_windows_nodejs_facts() -> (bool, bool) {
    let dir = Path::new(NODEJS_DIR_WINDOWS);
    let is_reparse = match std::fs::canonicalize(dir) {
        // 目录不存在（没装过 node 也没有 nvm）不算 reparse
        Ok(real) => !same_path(&real.to_string_lossy(), NODEJS_DIR_WINDOWS),
        Err(_) => false,
    };
    let symlink_hit = std::env::var("NVM_SYMLINK")
        .map(|v| !v.trim().is_empty() && same_path(v.trim(), NODEJS_DIR_WINDOWS))
        .unwrap_or(false);
    (is_reparse, symlink_hit)
}

#[cfg(not(windows))]
fn read_windows_nodejs_facts() -> (bool, bool) {
    (false, false)
}

/// 给每个管理器填上它自己已安装的版本。版本列表**降序**，与 kind 无关的排序口径见
/// `sort_versions_desc`（`v9` 必须排在 `v26` 之后，字典序会反过来）。
///
/// 这一路服务 D4「显示但不代管」：GUI 的 PATH 里解析不到 node 时，卡片仍要能说出
/// 「由 nvm 托管 · v26.12.0」，而不是把用户已经装好的东西报成「未安装」。
/// 只列目录名，不调用任何 manager 命令（`nvm list` 之类要 source shell function，超出 M0 边界）。
///
/// 挂在 `Manager` 身上而不是另建一张 `(kind -> versions)` 平行表：一处数据一个真相源，
/// 两处都要维护迟早和 `managers` 去重后的结果对不上。
fn attach_versions(managers: Vec<Manager>) -> Vec<Manager> {
    managers
        .into_iter()
        .map(|mut m| {
            m.versions = manager_versions(m.kind, &m.root);
            m
        })
        .collect()
}

fn manager_versions(kind: ManagerKind, root: &Path) -> Vec<String> {
    let Some(dir) = kind.version_dir(root) else {
        return Vec::new();
    };
    let mut vs = list_version_dirs(&dir);
    sort_versions_desc(&mut vs);
    vs
}

fn list_version_dirs(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        // 用 `is_dir()`（跟随链接）而不是 `file_type()`：scoop 的版本目录可能是链接
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(v) = entry.file_name().to_str().and_then(clean_version_name) {
            out.push(v);
        }
    }
    out
}

/// 按 kind 去重，保留首次出现（调用方按 env -> 目录 -> rc 的可靠度顺序喂进来）。
/// `dedup_by` 只清连续重复，这里要的是跨条目去重，手写更直白。
fn dedup_kinds(ms: Vec<Manager>) -> Vec<Manager> {
    let mut out: Vec<Manager> = Vec::with_capacity(ms.len());
    for m in ms {
        if !out.iter().any(|e| e.kind == m.kind) {
            out.push(m);
        }
    }
    out
}

/// 四路信号取证并产出结论：环境变量 -> 已知目录 -> shell rc -> 路径归属。
///
/// 只读、无副作用（最多触发 `platform::child_path()` 的进程 PATH 刷新，与现有探测一致）。
/// 不做缓存，每次刷新/每次安装前各调一次即可：读几个文件 + 两三条子进程。
pub fn detect() -> NodeEnv {
    let home = crate::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"));

    // 1. 环境变量（从终端启动 DevUp 时最可靠）
    let env_lookup = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut managers = managers_from_env_with(&home, cfg!(windows), env_lookup);
    // 2. 已知目录（Finder 启动时 env 全空，这一路是唯一证据）
    managers.extend(managers_from_dirs_with(&home, |p| p.is_dir()));
    // 3. shell rc 文本（识破 GUI 看不见的 nvm 激活）
    managers.extend(scan_shell_rc(&home));
    // 去重后再列版本：同一 kind 被三路信号都认到时只该列一次目录
    let managers = attach_versions(dedup_kinds(managers));

    // 4. 归属判定
    let active_node = find_active_node().map(|p| resolve_real(&p));
    let version = active_node.as_deref().and_then(read_node_version);
    let npm_prefix = read_npm_prefix(active_node.as_deref());
    let (nodejs_dir_is_reparse, nvm_symlink_is_nodejs_dir) = read_windows_nodejs_facts();
    let pkg_receipt = read_pkg_receipt();

    classify(&NodeProbeInput {
        managers,
        active_node,
        version,
        npm_prefix,
        nodejs_dir_is_reparse,
        nvm_symlink_is_nodejs_dir,
        pkg_receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成现场统一用一个假 home，避免任何真实用户目录进入判定路径
    fn fake_home() -> PathBuf {
        PathBuf::from("/Users/u")
    }

    /// 构造「没扫到任何版本」的管理器（等价于真实采集里目录不存在的形态）
    fn mgr(kind: ManagerKind, root: &str) -> Manager {
        Manager::new(kind, PathBuf::from(root))
    }

    /// 构造带版本列表的管理器，入参按 `v26.12.0` 这类目录名写，走同一套清洗与降序排序，
    /// 以免测试自己手工维护顺序而验不到真实形态
    fn mgrv(kind: ManagerKind, root: &str, dirs: &[&str]) -> Manager {
        let mut versions: Vec<String> = dirs.iter().filter_map(|d| clean_version_name(d)).collect();
        sort_versions_desc(&mut versions);
        Manager {
            kind,
            root: PathBuf::from(root),
            versions,
        }
    }

    /// 表驱动 owner 判定：Windows 那几行的路径分隔符与本机无关，`path_key` 归一化后
    /// 走的是同一段字符串逻辑，所以在 macOS 上能跑同一张表。
    #[test]
    fn classify_owner_table() {
        struct Case {
            name: &'static str,
            node: Option<&'static str>,
            managers: Vec<Manager>,
            reparse: bool,
            want: NodeOwner,
        }
        // 管理器根目录全部随输入给出、判定不读 home，因此这张表不需要假 home
        let cases = [
            Case {
                name: "没有 node 就是 None",
                node: None,
                managers: vec![],
                reparse: false,
                want: NodeOwner::None,
            },
            Case {
                name: "nvm 版本目录 -> Manager(Nvm)",
                node: Some("/Users/u/.nvm/versions/node/v26.12.0/bin/node"),
                managers: vec![mgr(ManagerKind::Nvm, "/Users/u/.nvm")],
                reparse: false,
                want: NodeOwner::Manager(ManagerKind::Nvm),
            },
            Case {
                name: "volta 镜像目录 -> Manager(Volta)",
                node: Some("/Users/u/.volta/tools/image/node/24.22.0/bin/node"),
                managers: vec![mgr(ManagerKind::Volta, "/Users/u/.volta")],
                reparse: false,
                want: NodeOwner::Manager(ManagerKind::Volta),
            },
            Case {
                name: "scoop current 已穿透到版本目录 -> Manager(Scoop)",
                node: Some("C:\\Users\\u\\scoop\\apps\\nodejs\\24.22.0\\node.exe"),
                managers: vec![mgr(ManagerKind::Scoop, "C:\\Users\\u\\scoop\\apps\\nodejs")],
                reparse: false,
                want: NodeOwner::Manager(ManagerKind::Scoop),
            },
            Case {
                name: "Windows junction 被 canonicalize 穿透到 nvm 目录 -> Manager(NvmWindows)",
                // 这正是设计文档要识破的形态：路径已不在 Program Files，证据在 nvm 根目录里
                node: Some("C:\\Users\\u\\AppData\\Roaming\\nvm\\v24.22.0\\node.exe"),
                managers: vec![mgr(
                    ManagerKind::NvmWindows,
                    "C:\\Users\\u\\AppData\\Roaming\\nvm",
                )],
                reparse: true,
                want: NodeOwner::Manager(ManagerKind::NvmWindows),
            },
            Case {
                name: "mise 安装目录 -> Manager(Mise)",
                node: Some("/Users/u/.local/share/mise/installs/node/24.22.0/bin/node"),
                managers: vec![mgr(ManagerKind::Mise, "/Users/u/.local/share/mise")],
                reparse: false,
                want: NodeOwner::Manager(ManagerKind::Mise),
            },
            Case {
                name: "n 的版本目录 -> Manager(N)（/usr/local/bin/node 已穿透到此处）",
                node: Some("/usr/local/lib/node_modules/n/versions/node/22.11.0/bin/node"),
                managers: vec![mgr(
                    ManagerKind::N,
                    "/usr/local/lib/node_modules/n/versions/node",
                )],
                reparse: false,
                want: NodeOwner::Manager(ManagerKind::N),
            },
            Case {
                name: "Apple Silicon Homebrew Cellar -> Brew",
                node: Some("/opt/homebrew/Cellar/node/24.22.0/bin/node"),
                managers: vec![],
                reparse: false,
                want: NodeOwner::Brew,
            },
            Case {
                name: "Intel Homebrew（/usr/local 版）靠 /cellar/ 命中 -> Brew",
                node: Some("/usr/local/Cellar/node/18.19.0/bin/node"),
                managers: vec![],
                reparse: false,
                want: NodeOwner::Brew,
            },
            Case {
                name: "brew 目录下的 bin/node（canonicalize 失败的兜底形态）-> Brew",
                node: Some("/opt/homebrew/bin/node"),
                managers: vec![],
                reparse: false,
                want: NodeOwner::Brew,
            },
            Case {
                name: "macOS 官方 PKG 落点 -> SystemPkg",
                node: Some("/usr/local/bin/node"),
                managers: vec![],
                reparse: false,
                want: NodeOwner::SystemPkg,
            },
            Case {
                name: "Windows 实体目录 -> SystemPkg（裸 MSI 机器，现状放行）",
                node: Some("C:\\Program Files\\nodejs\\node.exe"),
                managers: vec![],
                reparse: false,
                want: NodeOwner::SystemPkg,
            },
            Case {
                name: "Windows 目录是 reparse 且没有管理器证据 -> Unknown",
                // 判 SystemPkg 会诱导护栏放 MSI 写穿链接，这里必须保守
                node: Some("C:\\Program Files\\nodejs\\node.exe"),
                managers: vec![],
                reparse: true,
                want: NodeOwner::Unknown,
            },
            Case {
                name: "大小写与分隔符混用不影响 Windows 判定",
                node: Some("c:\\program files\\NODEJS\\node.exe"),
                managers: vec![mgr(ManagerKind::Nvm, "C:\\Users\\u\\AppData\\Roaming\\nvm")],
                reparse: false,
                want: NodeOwner::SystemPkg,
            },
            Case {
                name: "认不出的路径 -> Unknown（不猜归属）",
                node: Some("/opt/tools/node"),
                managers: vec![mgr(ManagerKind::Nvm, "/Users/u/.nvm")],
                reparse: false,
                want: NodeOwner::Unknown,
            },
            Case {
                name: "同前缀陷阱：/usr/local2 不等于在 /usr/local 之内",
                node: Some("/usr/local2/bin/node"),
                managers: vec![mgr(ManagerKind::N, "/usr/local")],
                reparse: false,
                want: NodeOwner::Unknown,
            },
            Case {
                name: "装了 nvm 但生效的是 brew node（本机常见形态）-> Brew",
                node: Some("/opt/homebrew/Cellar/node/24.22.0/bin/node"),
                managers: vec![mgr(ManagerKind::Nvm, "/Users/u/.nvm")],
                reparse: false,
                want: NodeOwner::Brew,
            },
        ];

        for c in cases {
            let input = NodeProbeInput {
                managers: c.managers.clone(),
                active_node: c.node.map(PathBuf::from),
                nodejs_dir_is_reparse: c.reparse,
                ..Default::default()
            };
            let got = classify(&input);
            assert_eq!(got.owner, c.want, "用例失败: {}", c.name);
            assert_eq!(
                got.active_node.is_some(),
                c.node.is_some(),
                "active_node 必须原样透出"
            );
        }
    }

    /// 版本归属的两条口径：
    /// - 顶层 `managed_versions` 只在 owner 命中某个管理器时有值（那一个管理器的版本）；
    /// - owner 定位不到时顶层**留空**，分管理器的问题直接遍历 `managers` 回答。
    ///
    /// 本机形态就是 nvm + volta 共存，所以这个用例同时是那条真机回归的合成版。
    #[test]
    fn classify_scopes_managed_versions_to_owner() {
        let nvm = mgrv(ManagerKind::Nvm, "/Users/u/.nvm", &["26.12.0", "24.22.0"]);
        let volta = mgrv(
            ManagerKind::Volta,
            "/Users/u/.volta",
            &["20.15.0", "16.14.2"],
        );
        let input = NodeProbeInput {
            managers: vec![nvm, volta],
            active_node: Some(PathBuf::from(
                "/Users/u/.nvm/versions/node/v26.12.0/bin/node",
            )),
            ..Default::default()
        };

        let env = classify(&input);
        assert_eq!(env.owner, NodeOwner::Manager(ManagerKind::Nvm));
        assert_eq!(
            env.managed_versions,
            vec!["26.12.0".to_string(), "24.22.0".to_string()]
        );
        // 只取生效那一份：把 volta 的 20.15.0 混进「由 nvm 托管」是纯粹的困惑
        assert!(!env.managed_versions.contains(&"20.15.0".to_string()));
        // owner_manager 必须指到那一个管理器本体（卡片要的是它的 root，不只是版本）
        let owner = env
            .owner_manager()
            .expect("owner 命中 nvm 时必须取到该管理器");
        assert_eq!(owner.kind, ManagerKind::Nvm);
        assert_eq!(owner.root, PathBuf::from("/Users/u/.nvm"));

        // GUI 的 PATH 里解析不到 node（D4 最需要「由 nvm 托管 · vX」的场景）：
        // 顶层列表必须为空，但两个管理器的版本必须分别还在
        let lost = classify(&NodeProbeInput {
            active_node: None,
            ..input.clone()
        });
        assert_eq!(lost.owner, NodeOwner::None);
        assert!(
            lost.managed_versions.is_empty(),
            "owner 未定位到管理器时不得凭空给一份合并列表"
        );
        assert!(
            lost.owner_manager().is_none(),
            "owner=None 时不该指定托管方"
        );

        let per_manager: Vec<(ManagerKind, Vec<String>)> = lost
            .managers
            .iter()
            .map(|m| (m.kind, m.versions.clone()))
            .collect();
        assert_eq!(
            per_manager,
            vec![
                (
                    ManagerKind::Nvm,
                    vec!["26.12.0".to_string(), "24.22.0".to_string()]
                ),
                (
                    ManagerKind::Volta,
                    vec!["20.15.0".to_string(), "16.14.2".to_string()]
                ),
            ]
        );

        // 装了管理器但一个版本都没有：必须仍然可见（决策矩阵规则 6 的输入）
        let bare = classify(&NodeProbeInput {
            managers: vec![mgr(ManagerKind::Nvm, "/Users/u/.nvm")],
            ..Default::default()
        });
        assert!(bare.managed_versions.is_empty());
        assert_eq!(bare.managers.len(), 1, "空版本列表的管理器不能被过滤掉");
        assert!(bare.managers[0].versions.is_empty());
    }

    /// `classify` 必须把采集到的事实原样透出（护栏 G1/G5 直接吃这几个标志）
    #[test]
    fn classify_passes_facts_through() {
        let input = NodeProbeInput {
            nodejs_dir_is_reparse: true,
            nvm_symlink_is_nodejs_dir: true,
            pkg_receipt: true,
            npm_prefix: Some(PathBuf::from("/Users/u/.nvm/versions/node/v26.12.0")),
            version: Some("26.12.0".into()),
            ..Default::default()
        };
        let env = classify(&input);
        assert!(env.nodejs_dir_is_reparse);
        assert!(env.pkg_receipt);
        assert_eq!(
            env.npm_prefix.as_deref(),
            Some(Path::new("/Users/u/.nvm/versions/node/v26.12.0"))
        );
        assert_eq!(env.version.as_deref(), Some("26.12.0"));

        let empty = classify(&NodeProbeInput::default());
        assert_eq!(empty, NodeEnv::default());
    }

    /// 路径归一化 + 目录包含关系（reparse 判定的纯逻辑部分）
    #[test]
    fn path_key_and_within_table() {
        struct Case {
            name: &'static str,
            a: &'static str,
            b: &'static str,
            want_within: bool,
            want_same: bool,
        }
        let cases = [
            Case {
                name: "大小写不同视为同一位置",
                a: "C:\\Program Files\\nodejs\\node.exe",
                b: "c:/program files/NodeJS/node.exe",
                want_within: true,
                want_same: true,
            },
            Case {
                name: "尾部分隔符不参与比较",
                a: "C:\\Users\\u\\.nvm\\",
                b: "/Users/u/.nvm",
                want_within: false, // 不同机器根，只验 same 语义前先各自看 key
                want_same: false,
            },
            Case {
                name: "反斜杠与正斜杠同视",
                a: "C:\\Users\\u\\.nvm\\versions\\node\\v26.12.0\\bin\\node",
                b: "C:\\Users\\u\\.nvm",
                want_within: true,
                want_same: false,
            },
            Case {
                name: "连续分隔符折叠（C:\\\\a\\\\b）",
                a: "C:\\\\Users\\\\u\\\\.volta//bin//node",
                b: "C:/Users/u/.volta",
                want_within: true,
                want_same: false,
            },
            Case {
                name: "兄弟目录名同前缀不算在内",
                a: "/Users/u/.nvm-backup/bin/node",
                b: "/Users/u/.nvm",
                want_within: false,
                want_same: false,
            },
            Case {
                name: "自身算在内",
                a: "/usr/local",
                b: "/usr/local",
                want_within: true,
                want_same: true,
            },
            Case {
                name: "reparse 判定：链接目标与自身不同即真",
                a: "C:\\Users\\u\\AppData\\Roaming\\nvm\\v24.22.0",
                b: "C:\\Program Files\\nodejs",
                want_within: false,
                want_same: false,
            },
            Case {
                name: "空根目录不吞掉一切",
                a: "/usr/local/bin/node",
                b: "",
                want_within: false,
                want_same: false,
            },
        ];
        for c in cases {
            assert_eq!(
                is_within(&path_key(c.a), &path_key(c.b)),
                c.want_within,
                "within 用例失败: {}",
                c.name
            );
            assert_eq!(
                same_path(c.a, c.b),
                c.want_same,
                "same 用例失败: {}",
                c.name
            );
        }

        assert_eq!(
            path_key("C:\\Program Files\\nodejs\\"),
            "c:/program files/nodejs"
        );
        assert_eq!(path_key("/"), "/");
        assert_eq!(path_key(""), "");
    }

    /// rc 扫描：识别 fixture 里的管理器，并证明「只读未执行」
    #[test]
    fn scan_shell_rc_detects_managers_without_executing() {
        let sentinel = std::env::temp_dir().join("devup-rc-was-executed");
        let _ = std::fs::remove_file(&sentinel);

        let home = temp_case_dir("rc");
        let rc = "export NVM_DIR=\"$HOME/.nvm\"\n\
                  [ -s \"$NVM_DIR/nvm.sh\" ] && \\. \"$NVM_DIR/nvm.sh\"\n\
                  eval \"$(fnm env --use-on-cd)\"\n\
                  export VOLTA_HOME=\"$HOME/.volta\"\n\
                  eval \"$(mise activate zsh)\"\n\
                  export PROTO_HOME=\"$HOME/.proto\"\n\
                  # export ASDF_DATA_DIR=\"/deliberately/commented/out\"\n\
                  export PATH=\"$HOME/.npm-global/bin:$PATH\"\n\
                  echo PWNED_FROM_RC > /tmp/devup-rc-was-executed\n";
        std::fs::write(home.join(".zshrc"), rc).unwrap();
        std::fs::write(
            home.join(".zprofile"),
            "export N_PREFIX=\"$HOME/n-prefix\"\n",
        )
        .unwrap();

        let found = scan_shell_rc(&home);
        let kinds: Vec<ManagerKind> = found.iter().map(|m| m.kind).collect();
        for want in [
            ManagerKind::Nvm,
            ManagerKind::Fnm,
            ManagerKind::Volta,
            ManagerKind::Mise,
            ManagerKind::Proto,
            ManagerKind::N,
        ] {
            assert!(kinds.contains(&want), "应识别出 {want:?}，实际 {kinds:?}");
        }
        assert!(
            !kinds.contains(&ManagerKind::Asdf),
            "注释掉的配置不得算成托管证据"
        );

        // 同行解析出的路径要带上（而不是退回默认根目录）
        let nvm = found.iter().find(|m| m.kind == ManagerKind::Nvm).unwrap();
        assert_eq!(nvm.root, home.join(".nvm"));
        let n = found.iter().find(|m| m.kind == ManagerKind::N).unwrap();
        // N 只认领版本目录（见 `n_manager_root` 的注释）；fixture 里不存在则退回前缀
        assert!(
            n.root == n_manager_root(&home.join("n-prefix")),
            "N_PREFIX 应展开为 fixture 路径，实际 {:?}",
            n.root
        );

        // 只读未执行的证据：这行若被 source 就会落盘
        assert!(!sentinel.exists(), "rc 扫描绝不能执行用户脚本");

        std::fs::remove_dir_all(&home).ok();
    }

    /// rc 行匹配的纯函数面：赋值/引用/注释三类形态
    #[test]
    fn rc_match_line_table() {
        let h = fake_home();
        let cases = [
            ("export NVM_DIR=\"/Users/u/.nvm\"", Some(ManagerKind::Nvm)),
            ("  export NVM_DIR=/Users/u/.nvm  ", Some(ManagerKind::Nvm)),
            (
                "[ -s \"/Users/u/.nvm/nvm.sh\" ] && . \"/Users/u/.nvm/nvm.sh\"",
                Some(ManagerKind::Nvm),
            ),
            ("plugins=(git zsh-nvm)", Some(ManagerKind::Nvm)),
            (
                ". \"/Users/u/.volta/bin/volta\" env",
                Some(ManagerKind::Volta),
            ),
            ("eval \"$(fnm env --use-on-cd)\"", Some(ManagerKind::Fnm)),
            (
                "export VOLTA_HOME=\"$HOME/.volta\"",
                Some(ManagerKind::Volta),
            ),
            ("alias b='mise activate zsh'", Some(ManagerKind::Mise)),
            ("# export NVM_DIR=/old/path", None),
            ("export PATH=\"/usr/bin:$PATH\"", None),
            // 读变量不等于设变量：`$VOLTA_HOME/bin` 里没有 `=`，命中不了赋值片段
            ("export PATH=\"$VOLTA_HOME/bin:$PATH\"", None),
            (
                "export PATH=\"$HOME/.volta/bin:$PATH\"",
                Some(ManagerKind::Volta),
            ),
            ("echo 'hello node'", None),
        ];
        for (line, want) in cases {
            let got = rc_match_line(line, &h).map(|m| m.kind);
            assert_eq!(got, want, "行匹配失败: {line:?}");
        }

        // 赋值取到路径；引用形态（$VOLTA_HOME/bin）只认 kind、不造 bogus 根目录
        let m = rc_match_line("export PROTO_HOME=\"${HOME}/.proto\"", &h).unwrap();
        assert_eq!(m.root, h.join(".proto"));
        let m = rc_match_line("export FNM_DIR=/opt/fnm-data", &h).unwrap();
        assert_eq!(m.root, PathBuf::from("/opt/fnm-data"));

        // 带引号的值可以含空格：mac 上 fnm 的默认目录就在 `Application Support` 里，
        // 按空白截会把路径腰斩成一个不存在的目录
        let m = rc_match_line(
            "export FNM_DIR=\"$HOME/Library/Application Support/fnm\"",
            &h,
        )
        .unwrap();
        assert_eq!(
            m.root,
            h.join("Library").join("Application Support").join("fnm")
        );

        // 不带引号时 `;` 是值的结束，不能把分号带进路径
        let m = rc_match_line(
            "export NVM_DIR=$HOME/.nvm; export PATH=$PATH:$NVM_DIR/bin",
            &h,
        )
        .unwrap();
        assert_eq!(m.root, h.join(".nvm"));

        // 取不到路径时退回该 kind 的默认根目录，而不是留下一个空根
        let m = rc_match_line("export NVM_DIR=", &h).unwrap();
        assert_eq!(m.root, h.join(".nvm"));
    }

    /// 环境变量那一路：env 与平台都注入，不碰真实环境（并行测试不争抢 env）。
    /// `is_windows` 显式传入而不是 `cfg!`，所以 nvm -> nvm-windows 的改写也能在同一张表里验。
    #[test]
    fn managers_from_env_table() {
        let h = fake_home();
        let cases: &[(&str, &str, bool, Option<ManagerKind>, &str)] = &[
            (
                "NVM_DIR",
                "/Users/u/.nvm",
                false,
                Some(ManagerKind::Nvm),
                "/Users/u/.nvm",
            ),
            // nvm-windows 的安装器同样设 NVM_DIR，同一变量在两个平台上是两个不同实现
            (
                "NVM_DIR",
                "C:\\Users\\u\\AppData\\Roaming\\nvm",
                true,
                Some(ManagerKind::NvmWindows),
                "C:\\Users\\u\\AppData\\Roaming\\nvm",
            ),
            ("NVM_SYMLINK", "C:\\Program Files\\nodejs", true, None, ""),
            (
                "VOLTA_HOME",
                "/Users/u/.volta",
                false,
                Some(ManagerKind::Volta),
                "/Users/u/.volta",
            ),
            (
                "MISE_DATA_DIR",
                "~/.local/share/mise",
                false,
                Some(ManagerKind::Mise),
                "/Users/u/.local/share/mise",
            ),
            (
                "ASDF_DATA_DIR",
                "~/.asdf",
                false,
                Some(ManagerKind::Asdf),
                "/Users/u/.asdf",
            ),
            (
                "PROTO_HOME",
                "${HOME}/.proto",
                false,
                Some(ManagerKind::Proto),
                "/Users/u/.proto",
            ),
            (
                "FNM_DIR",
                "/opt/fnm-data",
                false,
                Some(ManagerKind::Fnm),
                "/opt/fnm-data",
            ),
            // fnm 的 multisheel 路径要截回数据根，否则版本目录落在根外面
            (
                "FNM_MULTISHELL_PATH",
                "/Users/u/Library/Application Support/fnm/multishell/ABCD1234",
                false,
                Some(ManagerKind::Fnm),
                "/Users/u/Library/Application Support/fnm",
            ),
            ("N_PREFIX", "", false, None, ""),
        ];
        for (key, val, is_win, want_kind, want_root) in cases {
            let found = managers_from_env_with(&h, *is_win, |k| {
                if k == *key {
                    Some(val.to_string())
                } else {
                    None
                }
            });
            match want_kind {
                Some(k) => {
                    let m = found
                        .iter()
                        .find(|m| m.kind == *k)
                        .unwrap_or_else(|| panic!("{key} 应识别出 {k:?}"));
                    assert_eq!(m.root, PathBuf::from(want_root), "{key} 的根目录不对");
                }
                None => assert!(found.is_empty(), "{key}={val:?} 不应产出管理器"),
            }
        }

        // N_PREFIX 只认领 n 自己的版本目录；版本目录不存在时退回前缀（见 `n_manager_root`）
        let found = managers_from_env_with(&h, false, |k| {
            if k == "N_PREFIX" {
                Some("/usr/local".into())
            } else {
                None
            }
        });
        assert_eq!(found[0].kind, ManagerKind::N);
        assert_eq!(found[0].root, n_manager_root(Path::new("/usr/local")));
    }

    /// 已知目录那一路：注入存在性判定
    #[test]
    fn managers_from_dirs_table() {
        let h = fake_home();
        let present = [".nvm", ".volta", "scoop/apps/nodejs"];
        let found = managers_from_dirs_with(&h, |p| present.iter().any(|rel| p == h.join(rel)));
        let kinds: Vec<ManagerKind> = found.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&ManagerKind::Nvm));
        assert!(kinds.contains(&ManagerKind::Volta));
        assert!(kinds.contains(&ManagerKind::Scoop));
        assert!(
            !kinds.contains(&ManagerKind::Asdf),
            "不存在的目录不该被认成管理器"
        );
    }

    /// 托管版本排序：必须按语义版本降序，字典序会把 v9 排到 v26 前面
    #[test]
    fn managed_versions_sort_descending() {
        let mut raw: Vec<String> = ["v16.20.2", "v26.12.0", "v24.22.0"]
            .iter()
            .map(|s| clean_version_name(s).unwrap())
            .collect();
        sort_versions_desc(&mut raw);
        assert_eq!(
            raw,
            vec![
                "26.12.0".to_string(),
                "24.22.0".to_string(),
                "16.20.2".to_string()
            ]
        );

        // 单位数主版本 / 段数不同 / 别名目录
        let mut mixed = vec![
            "9.11.2".to_string(),
            "26.12.0".to_string(),
            "24".to_string(),
        ];
        sort_versions_desc(&mut mixed);
        assert_eq!(
            mixed,
            vec![
                "26.12.0".to_string(),
                "24".to_string(),
                "9.11.2".to_string()
            ]
        );

        assert_eq!(clean_version_name("v26.12.0").as_deref(), Some("26.12.0"));
        assert_eq!(clean_version_name("24.22.0"), Some("24.22.0".to_string()));
        for junk in [
            "current",
            "latest",
            "lts",
            "installation",
            "aliases",
            "v26.12.0-rc1",
            "node",
        ] {
            assert_eq!(clean_version_name(junk), None, "{junk} 不是版本目录名");
        }
    }

    /// 真实目录列举（fixture）：版本挂到各管理器身上，按降序，别名目录不算版本
    #[test]
    fn attach_versions_fills_each_manager() {
        let home = temp_case_dir("versions");
        let node_root = home.join(".nvm").join("versions").join("node");
        for v in [
            "v24.22.0",
            "v26.12.0",
            "v16.20.2",
            "alias-of-latest",
            "current",
        ] {
            std::fs::create_dir_all(node_root.join(v)).unwrap();
        }
        let nvm_root = home.join(".nvm");
        let volta_root = home.join(".volta");

        let filled = attach_versions(vec![
            // 故意乱序喂进去，验证排序不是靠调用方给对顺序
            mgr(ManagerKind::Nvm, nvm_root.to_str().unwrap()),
            // 版本目录不存在（本机没装 volta 的版本镜像）-> 管理器保留、versions 为空
            mgr(ManagerKind::Volta, volta_root.to_str().unwrap()),
            // 目录布局没把握的 kind 不收集版本（version_dir 返回 None）-> 同样是空
            mgr(ManagerKind::Proto, home.join(".proto").to_str().unwrap()),
        ]);

        assert_eq!(filled.len(), 3, "列不出版本也不能把管理器本身丢掉");
        assert_eq!(
            filled[0].versions,
            vec![
                "26.12.0".to_string(),
                "24.22.0".to_string(),
                "16.20.2".to_string()
            ]
        );
        assert!(
            filled[1].versions.is_empty(),
            "没扫到目录时留空而不是造数据"
        );
        assert!(filled[2].versions.is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    fn temp_case_dir(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "devup-node-source-{tag}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 人工核对全量探测结果：cargo test -- --ignored --nocapture
    ///
    /// 期望（本机 nvm + volta 共存）：owner = Manager(Nvm)，active_node 落在
    /// `~/.nvm/versions/node/<当前版本>/bin/node`，与用户终端 `node -v` 一致。
    #[test]
    #[ignore]
    fn dump_node_env() {
        let env = detect();
        println!("owner          = {:?}", env.owner);
        println!("active_node    = {:?}", env.active_node);
        println!("version        = {:?}", env.version);
        // 整个结构打印，Debug 里带各管理器自己的 versions —— nvm+volta 共存时必须能看出
        // 哪个版本属于哪个管理器，只看合并后的顶层列表会丢这个信息
        println!("managers       = {:?}", env.managers);
        println!("managed_versions = {:?}", env.managed_versions);
        println!("npm_prefix     = {:?}", env.npm_prefix);
        println!("nodejs_dir_is_reparse = {}", env.nodejs_dir_is_reparse);
        println!(
            "nvm_symlink_is_nodejs_dir = {}",
            env.nvm_symlink_is_nodejs_dir
        );
        println!("pkg_receipt    = {}", env.pkg_receipt);
        println!(
            "（两条 *_nodejs_dir / reparse 属 Windows 事实，本机恒为 false，代码路径未经 Windows 验证）"
        );
    }
}
