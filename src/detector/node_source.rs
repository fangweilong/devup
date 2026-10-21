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
//! 判定层（任务 D）就在本模块：`check_install()` / `check_uninstall()` 是护栏的纯函数，
//! 与采集层共用同一套 `path_key` / `is_within` 归一化规则，避免「探测说托管、护栏说没有」。
//! 判定不吃配置、不读环境、不起进程，输入只有 `&NodeEnv` + `Channel`；呈现方式（`Guidance`
//! 事件）与 `node.mode` 逃生口都在调用侧，见 `installer/mod.rs` 的 `node_preflight()`。

use serde::{Deserialize, Serialize};
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
#[cfg(target_os = "macos")]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

    /// 面向用户的名字：拒绝文案要指名道姓地说「谁在托管」，`Nvm` 打成 `nvm-windows`
    /// 才有意义（两者的命令形态与提权要求都不同）。大小写按各项目的官方写法。
    pub fn label(self) -> &'static str {
        match self {
            ManagerKind::Nvm => "nvm",
            ManagerKind::NvmWindows => "nvm-windows",
            ManagerKind::Fnm => "fnm",
            ManagerKind::Volta => "Volta",
            ManagerKind::Mise => "mise",
            ManagerKind::Asdf => "asdf",
            ManagerKind::Proto => "proto",
            ManagerKind::N => "n",
            ManagerKind::Scoop => "Scoop",
            ManagerKind::Nvs => "nvs",
        }
    }

    /// 该 kind 的建议命令（护栏拒绝时用；含 `{ver}` 的由 `apply_target_version` 填版本）。
    ///
    /// 只写**确有其事**的命令形态：拿不准版本参数的 kind（Scoop / nvs）给一条不指定版本的
    /// 通用命令，宁可不给细节，也不编造语法。nvm-windows 的 `nvm use` 会改 reparse point、
    /// 可能要求提权，但那是用户在自己终端里的决定，DevUp 只是不代做。
    /// **Windows 形态未实机验证。**
    fn guard_commands(self) -> &'static [&'static str] {
        match self {
            ManagerKind::Nvm => &["nvm install {ver}", "nvm use {ver}"],
            ManagerKind::NvmWindows => &["nvm install {ver}", "nvm use {ver}"],
            ManagerKind::Fnm => &["fnm install {ver}", "fnm default {ver}"],
            ManagerKind::Volta => &["volta install node@{ver}"],
            ManagerKind::Mise => &["mise install node@{ver}", "mise set -g node@{ver}"],
            ManagerKind::Asdf => &["asdf plugin add nodejs", "asdf install nodejs {ver}"],
            ManagerKind::Proto => &["proto install node {ver}"],
            // n 的用法就是 `n <版本>`（装完即切当前版本），`n install` 不是它的形态
            ManagerKind::N => &["n {ver}"],
            // 以下两条刻意不带 `{ver}`：这两个项目指定版本的语法我们没把握，
            // 给一条确实存在的通用命令好过编一个可能拼错的形态。**Windows 未验证。**
            ManagerKind::Scoop => &["scoop install nodejs-lts"],
            ManagerKind::Nvs => &["nvs add lts"],
        }
    }

    /// 该 kind 的**移除**命令（卸载拒绝横幅用；`{ver}` = 托管方已装的版本，
    /// 由 `uninstall_hint_commands` 构建时就填好，不走 `apply_target_version`——
    /// 那个令牌填的是「DevUp 本来要装的版本」，卸载指引指向没装的版本是反向的）。
    /// 与 `guard_commands` 同一条纪律：只写确有其事的形态；nvs 的移除别名语法没把握，
    /// 给只读的 `nvs ls` 让用户自己定位，不编 `nvs rm <alias>`。**Windows 形态未实机验证。**
    fn guard_uninstall_commands(self) -> &'static [&'static str] {
        match self {
            ManagerKind::Nvm => &["nvm uninstall {ver}"],
            ManagerKind::NvmWindows => &["nvm uninstall {ver}"],
            ManagerKind::Fnm => &["fnm uninstall {ver}"],
            ManagerKind::Volta => &["volta uninstall node"],
            ManagerKind::Mise => &["mise uninstall node@{ver}"],
            ManagerKind::Asdf => &["asdf uninstall nodejs {ver}"],
            ManagerKind::Proto => &["proto remove node {ver}"],
            ManagerKind::N => &["n rm {ver}"],
            ManagerKind::Scoop => &["scoop uninstall nodejs-lts"],
            ManagerKind::Nvs => &["nvs rm {ver}"],
        }
    }

    /// 版本化的移除命令没有版本可填时的**只读**退化（列出装了哪些版本，让用户自己挑）。
    /// 宁可只给查看命令，也不给 `nvm uninstall`（无参数）这种会直接报错的半成品。
    fn uninstall_probe_command(self) -> &'static str {
        match self {
            ManagerKind::Nvm => "nvm ls",
            ManagerKind::NvmWindows => "nvm list",
            ManagerKind::Fnm => "fnm list",
            ManagerKind::Volta => "volta list node",
            ManagerKind::Mise => "mise ls node",
            ManagerKind::Asdf => "asdf list nodejs",
            ManagerKind::Proto => "proto list",
            ManagerKind::N => "n ls",
            ManagerKind::Scoop => "scoop list",
            ManagerKind::Nvs => "nvs ls",
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
    let mut s = p.trim();
    // 剥离 Windows 扩展长度 / 设备命名空间前缀（例如 \\?\C:\ 或 \??\C:\ 或 \\.\C:\）
    if s.starts_with(r"\\?\UNC\") || s.starts_with("//?/UNC/") {
        let rest = &s[8..];
        return format!("//{}", path_key(rest));
    } else if s.starts_with(r"\\?\")
        || s.starts_with(r"\??\")
        || s.starts_with(r"\\.\")
        || s.starts_with("//?/")
        || s.starts_with("/??/")
        || s.starts_with("//./")
    {
        s = &s[4..];
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for ch in s.chars() {
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
    if out.starts_with("/?/unc/") {
        out = format!("/{}", &out[6..]);
    } else if out.starts_with("/?/") || out.starts_with("/./") {
        out = out[3..].to_string();
    } else if out.starts_with("/??/") {
        out = out[4..].to_string();
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
///
/// 唯一的非测试调用方 `read_windows_nodejs_facts()` 是 `#[cfg(windows)]`，所以这条也用 cfg
/// 门控（`test` 分支下的表驱动单测在 mac 上同样跑这套归一化）。不用压制警告的属性兜住
/// 整块——那会把以后真正死掉的比较逻辑一起藏住。
#[cfg(any(windows, test))]
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
pub fn version_key(v: &str) -> Vec<u64> {
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
            .find(|m| m.kind == kind && !m.versions.is_empty())
            .or_else(|| managers.iter().find(|m| m.kind == kind))
            .map(|m| m.versions.clone())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 护栏判定（纯函数：不吃配置、不读环境、不起进程，输入只有 &NodeEnv + Channel）
// ---------------------------------------------------------------------------

/// 本次写入/删除走的渠道。判定必须按渠道分派：同一个「装了 nvm」的现场，
/// `brew upgrade node` 是正常升级，`brew install node`（凭空多出一份并行 runtime）却要拦。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Windows 官方 MSI，落点 `C:\Program Files\nodejs`
    WindowsMsi,
    /// macOS 官方 PKG，落点 `/usr/local`
    MacPkg,
    /// macOS Homebrew 渠道（brew install / upgrade / uninstall node）
    MacBrewInstall,
}

/// 本平台 nodejs 写入/删除会走的渠道。`route_is_brew` 是调用方对「这次动的是 brew 还是系统
/// 包」的判断（macOS：安装看 `brew_available()`、卸载看 `brew_installed_formula("node")`）；
/// Windows 只有 MSI 一条路，该参数被忽略。
///
/// 用 `cfg!` 而不是 `#[cfg]`，两个理由：①「这次往哪写」的判断只该有一处，四个接入点各自
/// `#[cfg]` 迟早分叉；②`Channel::WindowsMsi` 因此在两个平台都有构造点——否则 mac 构建会把
/// 它判成 dead code，而本流水线明令不许再用 `allow` 压这种警告。
pub fn node_channel(route_is_brew: bool) -> Channel {
    if cfg!(windows) {
        Channel::WindowsMsi
    } else if route_is_brew {
        Channel::MacBrewInstall
    } else {
        Channel::MacPkg
    }
}

/// 护栏结论。`Refuse` 带的是「为什么没做 + 该怎么做」，不是错误码（设计文档 D3）：
/// 调用方把它转成 `AppEvent::Guidance` 非错误横幅，而不是 `InstallError` 红字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    Refuse {
        reason: String,
        commands: Vec<String>,
        note: Option<String>,
    },
}

/// 命令模板里的版本占位符，由 `apply_target_version()` 替换。
///
/// 为什么不让判定函数自己去拿版本：`Detector::get_latest_version()` 读的是全局缓存，
/// 纯函数一吃它，表驱动单测就跟着线上状态漂移；而把 `24.22.0` 写进源码是另一种错法
/// （LTS 会变）。所以版本由调用侧取、在这里以令牌形式传递。
pub const GUARD_VERSION_TOKEN: &str = "{ver}";

/// 逃生口提示（D1：M0 不给这个开关做 UI，只能手改配置文件）。
/// 护栏误判的代价是「少一次自动安装」，而绕过它的入口必须写得出、可回退。
///
/// 公开：G6 的脚本侧指引（installer 里拼）也要说同一句话，两处各写一份迟早改成两种说法。
pub const GUARD_ESCAPE_NOTE: &str = "确需 DevUp 接管系统安装，请在 config.json 把 node.mode 设为 \"system\"，该模式会跳过全部 Node 所有权检查。";

/// 托管方清单：owner 命中某个管理器时**只说那一个**（本机 nvm + volta 共存，把 volta 也写成
/// 「托管方」就是把不相干的东西算成责任方）；其余 owner（`None` / `Unknown` / `Brew` /
/// `SystemPkg`）把扫到的管理器都列出来——G3/G4 的真实现场正是「生效的不是他的，但他确实在用」。
fn custodians(env: &NodeEnv) -> Vec<&Manager> {
    match env.owner_manager() {
        Some(m) => vec![m],
        None => env.managers.iter().collect(),
    }
}

/// 组装 `Refuse`（安装族）：建议命令走 `hint_commands`（`{ver}` = DevUp 本来要装的那版）。
fn refuse(env: &NodeEnv, why: &str) -> GuardDecision {
    refuse_with(env, why, hint_commands(env))
}

/// 组装 `Refuse`（卸载族，G5/G6）：建议命令走 `uninstall_hint_commands`。
/// 卸载的拒绝若照抄安装族命令，就是在「别删」的现场教用户「装一个他没装的版本」——方向整个相反。
fn refuse_uninstall(env: &NodeEnv, why: &str) -> GuardDecision {
    refuse_with(env, why, uninstall_hint_commands(env))
}

/// 组装 `Refuse`：第一段说「谁托管、在哪」，第二段说「DevUp 为什么不动它」，
/// `commands` 给托管方自己的命令，`note` 给逃生口。
///
/// 托管方的 root 路径**必须**出现在 reason 里：只说「由 nvm 托管」是空话，用户无法自证；
/// 带上 `~/.nvm` 他才能立刻看出「哦，它读到的是我那个目录」。
fn refuse_with(env: &NodeEnv, why: &str, commands: Vec<String>) -> GuardDecision {
    let cs = custodians(env);
    let owner_kind = match env.owner {
        NodeOwner::Manager(k) => Some(k),
        _ => None,
    };

    let head = if cs.is_empty() {
        // 一点管理器证据都没有（只认出 reparse，或 /usr/local 那份不是 PKG 装的）：
        // 老实说「归属认不出」，而不是编一个托管方
        match env.active_node.as_ref() {
            Some(p) => format!(
                "DevUp 认不出这份 Node.js 的归属，也没有官方安装的凭证：{}。",
                p.display()
            ),
            None => "DevUp 没能确认这份 Node.js 的归属，无法保证这次写入不会踩到别人的东西。"
                .to_string(),
        }
    } else {
        let mut seen_kinds = std::collections::HashSet::new();
        let parts: Vec<String> = cs
            .iter()
            .filter(|m| seen_kinds.insert(m.kind))
            .map(|m| {
                let mut s = format!("{}（{}", m.kind.label(), m.root.display());
                // 版本只给「生效那份归的那个」补：列别人的版本会把用户引向错误的托管方
                if Some(m.kind) == owner_kind {
                    if let Some(v) = env.version.as_deref() {
                        s.push_str(&format!("，当前 v{v}"));
                    } else if let Some(v) = m.versions.first() {
                        s.push_str(&format!("，已装 v{v}"));
                    }
                }
                s.push('）');
                s
            })
            .collect();
        let joined = parts.join("、");
        if owner_kind.is_some() {
            // 拉丁名前留一个空格（中文排版），但全角 `）` 后不再加——「（… ） 托管」太松垮
            format!("当前 Node.js 由 {joined}托管。")
        } else {
            format!("这台机器上装着版本管理器 {joined}。")
        }
    };

    GuardDecision::Refuse {
        reason: format!("{head}\n{why}"),
        commands,
        note: Some(GUARD_ESCAPE_NOTE.to_string()),
    }
}

/// 归一化目标版本：去掉前导 `v`（`nvm install vv24` 不存在）
fn target_version(ver: &str) -> &str {
    ver.trim().trim_start_matches(['v', 'V'])
}

/// 单条命令的 `{ver}` 令牌替换。G6 的脚本侧指引走这里，保证命令形态与 `refuse()` 同源。
pub fn fill_version_token(command: &str, ver: &str) -> String {
    command.replace(GUARD_VERSION_TOKEN, target_version(ver))
}

/// 拒绝横幅的建议命令：逐托管方、按 kind 去重（nvm 与 nvm-windows 是两个 kind，命令形态
/// 各自列），`{ver}` 令牌保留给 `apply_target_version`。
///
/// 公开是因为 G6 有一条「跑到脚本才发现全部注册项都在管理器目录里」的现场：那不在
/// `check_*` 的静态判据内，调用方要自己拼指引，但命令形态必须与这里同源，否则两处迟早
/// 一边是 `nvm install`、一边是 `nvm i`。
pub fn hint_commands(env: &NodeEnv) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut seen: Vec<ManagerKind> = Vec::new();
    for m in &custodians(env) {
        if seen.contains(&m.kind) {
            continue;
        }
        seen.push(m.kind);
        commands.extend(m.kind.guard_commands().iter().map(|c| c.to_string()));
    }
    if commands.is_empty() {
        // 认不出托管方时给一条**只读**的定位命令，让用户自己看一眼这份 node 从哪来：
        // 比留一片空白有用，也不算编造安装语法。
        #[cfg(windows)]
        commands.push(r#"dir /AL "C:\Program Files\nodejs""#.to_string());
        #[cfg(not(windows))]
        commands.push("ls -l /usr/local/bin/node".to_string());
    }
    commands
}

/// 卸载拒绝横幅的建议命令（`hint_commands` 的卸载侧对孪生，G5/G6 与 mac 卸载全跳过的
/// 脚本侧指引都用它）。
///
/// 与安装侧的两处本质差别，都在这条函数里兑现：
/// 1. 命令形态换成**移除**（`nvm uninstall` 而不是 `nvm install`）——「DevUp 不删」的
///    现场给安装建议，等于没听懂用户要干什么；
/// 2. `{ver}` 填的是**该托管方自己已装的版本**（owner 命中时优先 `env.version`，
///    否则它 `versions` 的第一个；`versions` 已按语义降序），构建时就地填掉——
///    DevUp 的目标版本在这里毫无意义，而且真机上卸载拒绝走的横幅路径不该再等
///    `apply_target_version` 补一次只有安装侧正确的版本。
/// 版本一条都拿不到时退化为只读 list 命令（见 `uninstall_probe_command`）。
pub fn uninstall_hint_commands(env: &NodeEnv) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    let mut seen: Vec<ManagerKind> = Vec::new();
    let owner_kind = match env.owner {
        NodeOwner::Manager(k) => Some(k),
        _ => None,
    };
    for m in &custodians(env) {
        if seen.contains(&m.kind) {
            continue;
        }
        seen.push(m.kind);
        let ver: Option<&String> = if Some(m.kind) == owner_kind {
            env.version.as_ref().or(m.versions.first())
        } else {
            m.versions.first()
        };
        for tmpl in m.kind.guard_uninstall_commands() {
            if tmpl.contains(GUARD_VERSION_TOKEN) {
                match ver {
                    Some(v) => commands.push(tmpl.replace(GUARD_VERSION_TOKEN, target_version(v))),
                    None => commands.push(m.kind.uninstall_probe_command().to_string()),
                }
            } else {
                commands.push((*tmpl).to_string());
            }
        }
    }
    if commands.is_empty() {
        // 无托管方（如 mac 裸装但 receipt 缺失）：与安装侧同款只读定位命令
        #[cfg(windows)]
        commands.push(r#"dir /AL "C:\Program Files\nodejs""#.to_string());
        #[cfg(not(windows))]
        commands.push("ls -l /usr/local/bin/node".to_string());
    }
    commands
}

/// MSI / PKG 的系统落点是否落在某个管理器数据目录之内（G1 的第三条证据，也是 G6 的静态部分）。
///
/// 归一化与边界规则复用采集层的 `path_key` / `is_within`：判定与探测各写一份迟早漂移成
/// 两种结论（`/usr/local2` 与 `/usr/local` 那种同前缀陷阱就属于会漂移的地方）。
fn system_dir_owned_by_manager(env: &NodeEnv) -> bool {
    env.managers
        .iter()
        .any(|m| is_within(WIN_NODEJS_DIR_KEY, &path_key(&m.root.to_string_lossy())))
}

/// G1：不向已被版本管理器接管的 `C:\Program Files\nodejs` 执行 MSI。
///
/// 三条证据任一命中即拒：目录是 reparse point（MSI 会写穿链接）、`NVM_SYMLINK` 正好指着它
/// （链接已被上一次 MSI 写成实体目录、nvm 配置却还认它）、或该目录落在某个 `manager.root` 内。
fn g1_windows_msi(env: &NodeEnv) -> GuardDecision {
    if !env.nodejs_dir_is_reparse
        && !env.nvm_symlink_is_nodejs_dir
        && !system_dir_owned_by_manager(env)
        && !matches!(env.owner, NodeOwner::Manager(_))
    {
        return GuardDecision::Allow;
    }
    refuse(
        env,
        "当前系统由 Node 版本管理器接管，执行官方 MSI 会破坏版本管理器的生效环境并引入并行 runtime，所以 DevUp 不做该安装。",
    )
}

/// G3：检测到版本管理器时不执行 `brew install node`。
///
/// **简化的代价（必须写明）**：判据用的是 `owner != Brew`，而不是「brew 到底装没装 node」——
/// 后者要靠 `brew list` 子进程取证，M0 不为护栏新增进程（`detect()` 已经背着 2s 上限了）。
/// 于是这个现场会被误拦：用户确实装了 brew node，但日常被 nvm 遮蔽（owner = Manager），
/// 一次本可放行的 `brew upgrade` 会被拒绝。选择宁可拒绝，也不在「检测到管理器」的时候
/// 推动一份并行 runtime——放错的代价（PATH 里多出一份 node 抢解释）比多拦一次的代价大。
fn g3_brew_install(env: &NodeEnv) -> GuardDecision {
    if env.managers.is_empty() || env.owner == NodeOwner::Brew {
        return GuardDecision::Allow;
    }
    refuse(
        env,
        "Homebrew 这条路径会往 brew 的目录新装一份 Node，与版本管理器各说各话；DevUp 不新增这份并行 runtime，也不去抢 PATH。",
    )
}

/// G4：有管理器时不执行官方 PKG。两条判据分别是「凭空多一份」与「落点已被占」：
///
/// - `managers` 非空：PKG 只认 `/usr/local`，对 nvm / fnm / volta / mise 用户是新插一份
///   并行 runtime；对 `n` 更糟——`n` 的当前版本就在 `/usr/local`，PKG 直接覆盖它。
/// - `owner = SystemPkg` 且无 receipt：`/usr/local/bin/node` 确实在，但不是 PKG 装的
///   （源码编译、手动解压、`n` 留下的实体），DevUp 没资格替它做升级决定。
fn g4_mac_pkg(env: &NodeEnv) -> GuardDecision {
    let occupied_by_other = env.owner == NodeOwner::SystemPkg && !env.pkg_receipt;
    if env.managers.is_empty() && !occupied_by_other {
        return GuardDecision::Allow;
    }
    refuse(
        env,
        "官方 PKG 的安装位置固定在 /usr/local：要么凭空多出一份并行 runtime 抢 PATH，要么覆盖版本管理器当前指向的那一份，所以 DevUp 不执行 PKG 安装。",
    )
}

/// G5：macOS 卸载 PKG 前必须确认 receipt 存在（现状是闭眼 `rm -rf /usr/local/bin/node`）。
///
/// 没有 receipt 就说明那份 node 不是官方 PKG 装入的（`n`、源码编译、brew 链接都可能是），
/// 删掉它用户不会立刻察觉，而这是不可逆的——所以 `owner = Unknown` 在这里也算拦的理由
/// （D2：Unknown 只参与卸载侧的保守判定）。
fn g5_mac_pkg_uninstall(env: &NodeEnv) -> GuardDecision {
    if env.pkg_receipt || env.owner == NodeOwner::Brew {
        return GuardDecision::Allow;
    }
    refuse_uninstall(
        env,
        "没有 org.nodejs.node.pkg receipt，说明 /usr/local 那份 node 不是官方 PKG 装入的（可能是 n、源码编译或 Homebrew 链接），DevUp 不代为删除。",
    )
}

/// G6：Windows 卸载。
///
/// 精确判据是「`Get-Package` 的 `InstallLocation` 落在任一 `manager.root` 内」，而那需要跑
/// PowerShell 才知道——那部分过滤在 `installer::uninstall_target_windows` 的脚本里做，并按
/// 「一个都没命中」出指引。这里只负责静态可判的现场：系统目录本身已被 manager 接管，
/// 命中就不必再去跑卸载脚本。**Windows 分支本机未验证。**
fn g6_windows_uninstall(env: &NodeEnv) -> GuardDecision {
    if !env.nodejs_dir_is_reparse
        && !env.nvm_symlink_is_nodejs_dir
        && !system_dir_owned_by_manager(env)
        && !matches!(env.owner, NodeOwner::Manager(_))
    {
        return GuardDecision::Allow;
    }
    refuse_uninstall(
        env,
        "当前 Node.js 由版本管理器托管，DevUp 不代删托管方的 runtime，请使用对应版本管理器的卸载命令。",
    )
}

/// 安装侧护栏（G1 / G3 / G4）。
///
/// **D2：`owner = Unknown` 本身不是触发条件**——安装可逆（大不了再装一次），误拦的代价
/// 看得见也绕得开；三条规则各自的条件里都不检查 Unknown。Unknown 只在调用侧日志留一行痕
/// （`installer::node_preflight`）。注意这与 G1 不矛盾：MSI 那条看的是 reparse / NVM_SYMLINK /
/// 目录归属这些**直接证据**，它们成立时即便 owner 被判成 Unknown 也照样该拦。
pub fn check_install(env: &NodeEnv, ch: Channel) -> GuardDecision {
    match ch {
        Channel::WindowsMsi => g1_windows_msi(env),
        Channel::MacBrewInstall => g3_brew_install(env),
        Channel::MacPkg => g4_mac_pkg(env),
    }
}

/// 卸载侧护栏（G5 / G6）。比安装保守：删错不可逆，且用户不会立刻发现自己少了什么。
pub fn check_uninstall(env: &NodeEnv, ch: Channel) -> GuardDecision {
    match ch {
        Channel::WindowsMsi => g6_windows_uninstall(env),
        // brew 自管移除（`brew uninstall node`）删的是 brew 自己记账的目录，
        // 不在 G5「盲 rm /usr/local」的射程内
        Channel::MacBrewInstall => GuardDecision::Allow,
        Channel::MacPkg => g5_mac_pkg_uninstall(env),
    }
}

/// 把命令里的 `{ver}` 换成实际目标版本（前导 `v` 会被去掉，`nvm install vv24` 不存在）。
///
/// 版本由调用侧从 `Detector::get_latest_version("nodejs")` 取：护栏建议的版本必须与 DevUp
/// 本来要装的那一个一致，否则「建议命令」就成了猜。`reason` 不含该令牌（它讲的是归属，
/// 归属里没有「目标版本」这件事）。
pub fn apply_target_version(decision: GuardDecision, ver: &str) -> GuardDecision {
    match decision {
        GuardDecision::Allow => GuardDecision::Allow,
        GuardDecision::Refuse {
            reason,
            commands,
            note,
        } => GuardDecision::Refuse {
            reason,
            commands: commands
                .into_iter()
                .map(|c| fill_version_token(&c, ver))
                .collect(),
            note,
        },
    }
}

/// 卡片级指引预览（任务 H）：托管机器在**点确认之前**就能看到的指引内容，
/// 字段与 `GuardDecision::Refuse` 一致、可直接序列化成 `SoftwareStatus.guard_hint`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guidance {
    pub reason: String,
    pub commands: Vec<String>,
    pub note: Option<String>,
}

/// 给前端弹窗用的「这台机器现在会不会被护栏拒绝」预览（`uninstall` 选择查安装侧还是
/// 卸载侧那对函数，渠道也要按对应动作的真实路由传进来）。
///
/// **不许在这条路径之外另写一份文案**：预览走 `check_install` / `check_uninstall` +
/// `apply_target_version` 原样取结果，护栏改口径时弹窗横幅自动跟着变。返回 `None` =
/// 此刻判定是放行，前端按普通确认弹窗走（真点击后仍会经过 `node_preflight` 的实时判定，
/// 预览不是豁免）。
pub fn guidance_preview(
    env: &NodeEnv,
    ch: Channel,
    target_ver: &str,
    uninstall: bool,
) -> Option<Guidance> {
    let decision = if uninstall {
        check_uninstall(env, ch)
    } else {
        check_install(env, ch)
    };
    match apply_target_version(decision, target_ver) {
        GuardDecision::Refuse {
            reason,
            commands,
            note,
        } => Some(Guidance {
            reason,
            commands,
            note,
        }),
        GuardDecision::Allow => None,
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
    ("NVM_HOME", ManagerKind::NvmWindows),
    ("NVM_DIR", ManagerKind::Nvm),
    ("NVS_HOME", ManagerKind::Nvs),
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
///
/// 任务 B 原本把这张表整个 `#[cfg(windows)]` 掉。这里改成「跨平台构造、运行时按平台决定是否
/// 参与探测」，因为整段 cfg 掉会让 `ManagerKind::Nvs` 在 mac 构建里没有任何构造点，rustc 就报
/// dead code —— 而本流水线不许用 `allow` 压这类警告（探测语义没有任何变化：非 Windows 依旧
/// 返回空表，只是多构造两个随即丢弃的 PathBuf）。
#[cfg(windows)]
fn read_win_reg_env(k: &str) -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(env) = hkcu.open_subkey("Environment") {
        if let Ok(val) = env.get_value::<String, _>(k) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(env) =
        hklm.open_subkey(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment")
    {
        if let Ok(val) = env.get_value::<String, _>(k) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn read_win_reg_env(_k: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn read_nvm_windows_registry_root() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey(r"Software\Author Software\Preferences\nvm") {
        if let Ok(root) = key.get_value::<String, _>("InstallRoot") {
            let trimmed = root.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(key) = hklm.open_subkey(r"Software\Author Software\Preferences\nvm") {
        if let Ok(root) = key.get_value::<String, _>("InstallRoot") {
            let trimmed = root.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn read_nvm_windows_registry_root() -> Option<PathBuf> {
    None
}

fn read_nvm_settings_root(nvm_dir: &Path) -> Option<PathBuf> {
    let settings_file = nvm_dir.join("settings.txt");
    let content = std::fs::read_to_string(settings_file).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("root:") {
            let root = rest.trim();
            if !root.is_empty() {
                return Some(PathBuf::from(root));
            }
        }
    }
    None
}

fn platform_known_dirs(home: &Path) -> Vec<(ManagerKind, PathBuf)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut dirs = vec![
        (
            ManagerKind::NvmWindows,
            home.join("AppData").join("Roaming").join("nvm"),
        ),
        (
            ManagerKind::NvmWindows,
            home.join("AppData")
                .join("Local")
                .join("Author Software")
                .join("nvm"),
        ),
        (
            ManagerKind::NvmWindows,
            home.join("AppData").join("Local").join("nvm"),
        ),
        (
            ManagerKind::NvmWindows,
            PathBuf::from(r"C:\Program Files\nvm"),
        ),
        (
            ManagerKind::Fnm,
            home.join("AppData").join("Roaming").join("fnm"),
        ),
        (
            ManagerKind::Fnm,
            home.join("AppData").join("Local").join("fnm"),
        ),
        (
            ManagerKind::Volta,
            home.join("AppData").join("Local").join("Volta"),
        ),
        (
            ManagerKind::Nvs,
            home.join("AppData").join("Roaming").join("nvs"),
        ),
        (
            ManagerKind::Nvs,
            home.join("AppData").join("Local").join("nvs"),
        ),
        (ManagerKind::Nvs, PathBuf::from(r"C:\ProgramData\nvs")),
        (
            ManagerKind::Mise,
            home.join("AppData").join("Local").join("mise"),
        ),
    ];
    if let Some(install_root) = read_nvm_windows_registry_root() {
        dirs.push((ManagerKind::NvmWindows, install_root));
    }

    let mut extra = Vec::new();
    for (kind, dir) in &dirs {
        if *kind == ManagerKind::NvmWindows {
            if let Some(custom_root) = read_nvm_settings_root(dir) {
                extra.push((ManagerKind::NvmWindows, custom_root));
            }
        }
    }
    dirs.extend(extra);

    dirs
}

#[cfg(windows)]
fn managers_from_windows_registry() -> Vec<Manager> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let mut out = Vec::new();

    if let Some(install_root) = read_nvm_windows_registry_root() {
        if install_root.is_dir() {
            out.push(Manager::new(ManagerKind::NvmWindows, install_root));
        }
    }

    let roots = [
        (
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
    ];

    for (hkey, subkey_path) in roots {
        let base = RegKey::predef(hkey);
        let Ok(uninstall_key) = base.open_subkey(subkey_path) else {
            continue;
        };
        for name in uninstall_key.enum_keys().flatten() {
            let Ok(sub) = uninstall_key.open_subkey(&name) else {
                continue;
            };
            let display_name: String = sub.get_value("DisplayName").unwrap_or_default();
            let install_loc: String = sub.get_value("InstallLocation").unwrap_or_default();
            let trimmed_loc = install_loc.trim();
            if trimmed_loc.is_empty() {
                continue;
            }
            let loc_path = PathBuf::from(trimmed_loc);
            if !loc_path.is_dir() {
                continue;
            }

            let lower_name = display_name.to_lowercase();
            if lower_name.contains("nvm") {
                out.push(Manager::new(ManagerKind::NvmWindows, loc_path));
            } else if lower_name.contains("fnm") {
                out.push(Manager::new(ManagerKind::Fnm, loc_path));
            } else if lower_name.contains("volta") {
                out.push(Manager::new(ManagerKind::Volta, loc_path));
            }
        }
    }

    out
}

#[cfg(not(windows))]
fn managers_from_windows_registry() -> Vec<Manager> {
    Vec::new()
}

fn expand_nvm_settings(managers: &[Manager]) -> Vec<Manager> {
    let mut extra = Vec::new();
    for m in managers {
        if m.kind == ManagerKind::NvmWindows {
            if let Some(custom_root) = read_nvm_settings_root(&m.root) {
                if custom_root.is_dir() && custom_root != m.root {
                    extra.push(Manager::new(ManagerKind::NvmWindows, custom_root));
                }
            }
        }
    }
    extra
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

#[cfg(windows)]
fn which_in_child_path(cmd: &str) -> Option<PathBuf> {
    let child_path = crate::platform::child_path();
    let mut c = Command::new("where.exe");
    c.arg(cmd);
    c.env("PATH", &child_path);
    c.creation_flags(crate::detector::CREATE_NO_WINDOW);
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(PathBuf::from(line))
}

#[cfg(unix)]
fn which_in_child_path(cmd: &str) -> Option<PathBuf> {
    crate::platform::which(cmd)
}

#[cfg(not(any(windows, unix)))]
fn which_in_child_path(_cmd: &str) -> Option<PathBuf> {
    None
}

fn find_active_node() -> Option<PathBuf> {
    which_in_child_path("node").or_else(|| which_in_child_path("node.exe"))
}

fn managers_from_path() -> Vec<Manager> {
    let tools: &[(&str, ManagerKind)] = &[
        (
            "nvm",
            if cfg!(windows) {
                ManagerKind::NvmWindows
            } else {
                ManagerKind::Nvm
            },
        ),
        ("fnm", ManagerKind::Fnm),
        ("volta", ManagerKind::Volta),
        ("nvs", ManagerKind::Nvs),
        ("mise", ManagerKind::Mise),
        ("proto", ManagerKind::Proto),
        ("scoop", ManagerKind::Scoop),
    ];
    let mut out = Vec::new();
    for (tool, kind) in tools {
        let found = which_in_child_path(tool).or_else(|| {
            if cfg!(windows) {
                which_in_child_path(&format!("{tool}.exe"))
                    .or_else(|| which_in_child_path(&format!("{tool}.cmd")))
            } else {
                None
            }
        });
        if let Some(path) = found {
            if let Some(parent) = path.parent() {
                out.push(Manager::new(*kind, parent.to_path_buf()));
            }
        }
    }
    out
}

fn managers_from_active_node(active_node: &Path) -> Vec<Manager> {
    let mut out = Vec::new();
    let mut current = active_node.parent();
    while let Some(dir) = current {
        // 1. 检查目录下是否包含管理器可执行文件或特征配置
        if dir.join("nvm.exe").is_file()
            || dir.join("NVMWindows.Events.dll").is_file()
            || dir.join("settings.txt").is_file()
        {
            out.push(Manager::new(ManagerKind::NvmWindows, dir.to_path_buf()));
        }
        if dir.join("fnm.exe").is_file()
            || dir.file_name().and_then(|n| n.to_str()) == Some("node-versions")
        {
            let root = if dir.file_name().and_then(|n| n.to_str()) == Some("node-versions") {
                dir.parent().unwrap_or(dir).to_path_buf()
            } else {
                dir.to_path_buf()
            };
            out.push(Manager::new(ManagerKind::Fnm, root));
        }
        if dir.join("volta.exe").is_file()
            || dir.file_name().and_then(|n| n.to_str()) == Some(".volta")
        {
            out.push(Manager::new(ManagerKind::Volta, dir.to_path_buf()));
        }
        if dir.join("nvs.cmd").is_file() || dir.join("nvs.exe").is_file() {
            out.push(Manager::new(ManagerKind::Nvs, dir.to_path_buf()));
        }

        // 2. 检查该目录是否是形如 vX.Y.Z 的版本目录，其父目录是否包含多个版本
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if clean_version_name(name).is_some() {
                if let Some(parent) = dir.parent() {
                    let versions = list_version_dirs(parent);
                    if !versions.is_empty() {
                        let kind = if cfg!(windows) {
                            ManagerKind::NvmWindows
                        } else {
                            ManagerKind::Nvm
                        };
                        out.push(Manager::new(kind, parent.to_path_buf()));
                    }
                }
            }
        }

        current = dir.parent();
    }
    out
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
        .ok()
        .or_else(|| read_win_reg_env("NVM_SYMLINK"))
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
    let mut dirs_to_scan = Vec::new();
    if let Some(dir) = kind.version_dir(root) {
        dirs_to_scan.push(dir);
    }
    if kind == ManagerKind::NvmWindows {
        if let Some(settings_root) = read_nvm_settings_root(root) {
            if settings_root != root {
                dirs_to_scan.push(settings_root);
            }
        }
        let versions_subdir = root.join("versions");
        if versions_subdir.is_dir() {
            dirs_to_scan.push(versions_subdir);
        }
        if let Some(reg_root) = read_nvm_windows_registry_root() {
            if reg_root != root {
                dirs_to_scan.push(reg_root);
            }
        }
    }
    let mut vs = Vec::new();
    for dir in dirs_to_scan {
        vs.extend(list_version_dirs(&dir));
    }
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

fn dedup_kinds(ms: Vec<Manager>) -> Vec<Manager> {
    let mut out: Vec<Manager> = Vec::with_capacity(ms.len());
    for m in ms {
        let m_key = path_key(&m.root.to_string_lossy());
        // 如果已有相同 kind 且涵盖该路径（同一路径或其祖先目录），跳过
        let dominated = out.iter().any(|e| {
            if e.kind != m.kind {
                return false;
            }
            let e_key = path_key(&e.root.to_string_lossy());
            is_within(&m_key, &e_key)
        });
        if dominated {
            continue;
        }

        // 如果新根目录涵盖了已存在的子目录，剔除被涵盖的子目录
        out.retain(|e| {
            if e.kind != m.kind {
                return true;
            }
            let e_key = path_key(&e.root.to_string_lossy());
            !is_within(&e_key, &m_key)
        });

        out.push(m);
    }
    out
}

/// 四路信号取证并产出结论：环境变量 -> 已知目录 -> shell rc -> 路径归属。
///
/// 只读、无副作用（最多触发 `platform::child_path()` 的进程 PATH 刷新，与现有探测一致）。
/// 不做缓存，每次刷新/每次安装前各调一次即可：读几个文件 + 两三条子进程。
pub fn detect() -> NodeEnv {
    let home = crate::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"));

    // 1. 环境变量（从终端启动 DevUp 时最可靠，含注册表环境兜底）
    let env_lookup = |k: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| read_win_reg_env(k))
    };
    let mut managers = managers_from_env_with(&home, cfg!(windows), env_lookup);

    // 2. PATH 动态查找（识别用户安装到任意自定义目录中的 nvm/fnm/volta/mise 等）
    managers.extend(managers_from_path());

    // 3. Windows 标准注册表安装登记项（Uninstall 键中的 InstallLocation）
    managers.extend(managers_from_windows_registry());

    // 4. 已知默认目录兜底
    managers.extend(managers_from_dirs_with(&home, |p| p.is_dir()));

    // 5. shell rc 文本
    managers.extend(scan_shell_rc(&home));

    // 6. 查找当前生效的 node
    let active_node = find_active_node().map(|p| resolve_real(&p));

    // 7. 从 active_node 的祖先路径动态反推管理器与版本根目录（针对免安装/自定义目录）
    if let Some(node) = active_node.as_deref() {
        managers.extend(managers_from_active_node(node));
    }

    // 8. 针对已发现的所有 nvm-windows 实例，动态解析其 settings.txt 中的自定义 root
    managers.extend(expand_nvm_settings(&managers));

    // 去重后再列版本
    let managers = attach_versions(dedup_kinds(managers));

    // 9. 归属判定
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
            Case {
                name: "Windows verbatim \\?\\ 路径与普通路径归一化后视为同一位置",
                a: "\\\\?\\C:\\Program Files\\nodejs\\node.exe",
                b: "c:/program files/nodejs/node.exe",
                want_within: true,
                want_same: true,
            },
            Case {
                name: "Windows NT \\??\\ 命名空间路径与普通路径归一化后视为同一位置",
                a: "\\??\\C:\\Users\\u\\AppData\\Local\\Author Software\\nvm\\.shim\\node.exe",
                b: "C:\\Users\\u\\AppData\\Local\\Author Software\\nvm",
                want_within: true,
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
                "NVM_HOME",
                "C:\\Users\\u\\AppData\\Local\\Author Software\\nvm",
                true,
                Some(ManagerKind::NvmWindows),
                "C:\\Users\\u\\AppData\\Local\\Author Software\\nvm",
            ),
            (
                "NVS_HOME",
                "C:\\Users\\u\\AppData\\Local\\nvs",
                true,
                Some(ManagerKind::Nvs),
                "C:\\Users\\u\\AppData\\Local\\nvs",
            ),
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

    /// 护栏判定表：G1/G3/G4/G5/G6 各一条放行 + 一条拒绝，外加 D2 的保守口径。
    /// 全部用合成的 `NodeEnv`（不碰真机、不起进程），Windows 那几行走的是同一套
    /// `path_key` 字符串逻辑，所以在 macOS 上也能验。
    #[test]
    fn guard_decision_table() {
        #[derive(Clone, Copy, PartialEq)]
        enum Act {
            Install,
            Uninstall,
        }

        fn nvm() -> Manager {
            mgrv(ManagerKind::Nvm, "/Users/u/.nvm", &["v26.12.0", "v24.22.0"])
        }
        fn nvm_windows() -> Manager {
            mgrv(
                ManagerKind::NvmWindows,
                "C:\\Users\\u\\AppData\\Roaming\\nvm",
                &["v24.22.0"],
            )
        }

        struct Case {
            name: &'static str,
            act: Act,
            ch: Channel,
            env: NodeEnv,
            want_allow: bool,
            /// 拒绝时 reason 必须自证：托管方的 root 路径要出现在文案里
            want_root: Option<&'static str>,
            /// 拒绝时 commands 里必须有的子串（用于盯住「落点被非 PKG 占据」那类无托管方的现场）
            want_cmd: Option<&'static str>,
        }

        let cases = [
            // ---- G1 Win MSI 安装 ----
            Case {
                name: "G1 放行：裸 MSI 机器（实体目录、无管理器证据）",
                act: Act::Install,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    owner: NodeOwner::SystemPkg,
                    active_node: Some(PathBuf::from("C:\\Program Files\\nodejs\\node.exe")),
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G1 拒绝：Program Files\\nodejs 是 nvm-windows 的 junction",
                act: Act::Install,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    owner: NodeOwner::Manager(ManagerKind::NvmWindows),
                    managers: vec![nvm_windows()],
                    active_node: Some(PathBuf::from(
                        "C:\\Users\\u\\AppData\\Roaming\\nvm\\v24.22.0\\node.exe",
                    )),
                    version: Some("24.22.0".into()),
                    nodejs_dir_is_reparse: true,
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("C:\\Users\\u\\AppData\\Roaming\\nvm"),
                want_cmd: Some("nvm install"),
            },
            Case {
                name: "G1 拒绝：链接已被上次 MSI 写成实体，但 NVM_SYMLINK 仍指着它",
                act: Act::Install,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    managers: vec![nvm_windows()],
                    nvm_symlink_is_nodejs_dir: true,
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("AppData"),
                want_cmd: None,
            },
            Case {
                name: "G1 拒绝：MSI 落点整个落在某个 manager.root 内（第三条件）",
                // 合成的极端形态，只为单独钉住 is_within 那一条
                act: Act::Install,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    managers: vec![mgr(ManagerKind::Scoop, "C:\\Program Files")],
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("C:\\Program Files"),
                want_cmd: None,
            },
            // ---- G3 mac brew install ----
            Case {
                name: "G3 放行：owner 就是 Brew（那是正常升级）",
                act: Act::Install,
                ch: Channel::MacBrewInstall,
                env: NodeEnv {
                    owner: NodeOwner::Brew,
                    managers: vec![nvm()],
                    active_node: Some(PathBuf::from("/opt/homebrew/Cellar/node/24.22.0/bin/node")),
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G3 放行：机器上没有任何管理器",
                act: Act::Install,
                ch: Channel::MacBrewInstall,
                env: NodeEnv::default(),
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G3 拒绝：装了 nvm 且生效的不是 brew（凭空新增并行 runtime）",
                act: Act::Install,
                ch: Channel::MacBrewInstall,
                env: NodeEnv {
                    owner: NodeOwner::Manager(ManagerKind::Nvm),
                    managers: vec![nvm()],
                    active_node: Some(PathBuf::from(
                        "/Users/u/.nvm/versions/node/v26.12.0/bin/node",
                    )),
                    version: Some("26.12.0".into()),
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("/Users/u/.nvm"),
                want_cmd: Some("nvm install"),
            },
            // ---- G4 mac 官方 PKG 安装 ----
            Case {
                name: "G4 放行：裸 mac（无 node、无管理器）",
                act: Act::Install,
                ch: Channel::MacPkg,
                env: NodeEnv::default(),
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G4 放行：receipt 在册的官方 PKG 安装（正常升级）",
                act: Act::Install,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    owner: NodeOwner::SystemPkg,
                    active_node: Some(PathBuf::from("/usr/local/bin/node")),
                    pkg_receipt: true,
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G4 拒绝：有管理器（PKG 要么新增并行 runtime，要么覆盖 n 的当前版本）",
                act: Act::Install,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    managers: vec![nvm()],
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("/Users/u/.nvm"),
                want_cmd: None,
            },
            Case {
                name: "G4 拒绝：/usr/local/bin/node 在，但不是 PKG 装的",
                // 没有托管方（managers 空），文案只能报「归属认不出」，命令退化成只读定位
                act: Act::Install,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    owner: NodeOwner::SystemPkg,
                    active_node: Some(PathBuf::from("/usr/local/bin/node")),
                    pkg_receipt: false,
                    ..Default::default()
                },
                want_allow: false,
                want_root: None,
                want_cmd: if cfg!(windows) {
                    Some("dir /AL")
                } else {
                    Some("ls -l /usr/local/bin/node")
                },
            },
            // ---- G5 mac 卸载 ----
            Case {
                name: "G5 拒绝：没有 receipt 不得盲删 /usr/local/bin/node",
                act: Act::Uninstall,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    active_node: Some(PathBuf::from("/usr/local/bin/node")),
                    owner: NodeOwner::SystemPkg,
                    ..Default::default()
                },
                want_allow: false,
                want_root: None,
                want_cmd: if cfg!(windows) {
                    Some("dir /AL")
                } else {
                    Some("ls -l")
                },
            },
            Case {
                name: "G5 放行：receipt 在册",
                act: Act::Uninstall,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    owner: NodeOwner::SystemPkg,
                    pkg_receipt: true,
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G5 放行：生效 node 就是 brew 那份",
                act: Act::Uninstall,
                ch: Channel::MacPkg,
                env: NodeEnv {
                    owner: NodeOwner::Brew,
                    managers: vec![nvm()],
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G5 旁路：brew 自管卸载删的是 brew 记账的目录，不在盲 rm 射程内",
                act: Act::Uninstall,
                ch: Channel::MacBrewInstall,
                env: NodeEnv {
                    owner: NodeOwner::Manager(ManagerKind::Nvm),
                    managers: vec![nvm()],
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            // ---- G6 Win 卸载 ----
            Case {
                name: "G6 放行：裸 MSI 注册项（落点与任何管理器无关）",
                act: Act::Uninstall,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    owner: NodeOwner::SystemPkg,
                    managers: vec![mgr(ManagerKind::Scoop, "C:\\Users\\u\\scoop\\apps\\nodejs")],
                    ..Default::default()
                },
                want_allow: true,
                want_root: None,
                want_cmd: None,
            },
            Case {
                name: "G6 拒绝：系统目录指向 nvm-windows 自己的目录",
                act: Act::Uninstall,
                ch: Channel::WindowsMsi,
                env: NodeEnv {
                    managers: vec![nvm_windows()],
                    nodejs_dir_is_reparse: true,
                    ..Default::default()
                },
                want_allow: false,
                want_root: Some("AppData"),
                want_cmd: None,
            },
        ];

        for c in &cases {
            let got = match c.act {
                Act::Install => check_install(&c.env, c.ch),
                Act::Uninstall => check_uninstall(&c.env, c.ch),
            };
            match (&got, c.want_allow) {
                (GuardDecision::Allow, true) => {}
                (GuardDecision::Allow, false) => {
                    panic!("用例「{}」应拒绝，实际放行", c.name)
                }
                (
                    GuardDecision::Refuse {
                        reason,
                        commands,
                        note,
                    },
                    false,
                ) => {
                    assert!(!commands.is_empty(), "用例「{}」拒绝却没给命令", c.name);
                    assert!(note.is_some(), "用例「{}」缺逃生口提示", c.name);
                    if let Some(root) = c.want_root {
                        assert!(
                            reason.contains(root),
                            "用例「{}」的 reason 没自证托管方路径 {root}：{reason}",
                            c.name
                        );
                    }
                    if let Some(needle) = c.want_cmd {
                        assert!(
                            commands.iter().any(|x| x.contains(needle)),
                            "用例「{}」的命令里没有 {needle}：{commands:?}",
                            c.name
                        );
                    }
                }
                (GuardDecision::Refuse { reason, .. }, true) => {
                    panic!("用例「{}」应放行，实际拒绝：{reason}", c.name)
                }
            }
        }
    }

    /// D2：`owner = Unknown` 本身不拦安装（安装可逆，误拦的代价看得见也绕得开）。
    /// 这条是防手滑的锁——以后谁往安装条件里加 `Unknown` 就会挂在这里。
    #[test]
    fn guard_unknown_owner_never_blocks_install() {
        let env = NodeEnv {
            owner: NodeOwner::Unknown,
            active_node: Some(PathBuf::from("/opt/tools/node")),
            managers: vec![mgr(ManagerKind::Nvm, "/Users/u/.nvm")],
            ..Default::default()
        };
        // 渠道逐个试。mac 两条是纯 Unknown 现场（managers 会经由 G3/G4 自己拦，
        // 所以这里单独构造 managers 为空的版本）
        let bare = NodeEnv {
            managers: vec![],
            ..env.clone()
        };
        assert_eq!(check_install(&bare, Channel::MacPkg), GuardDecision::Allow);
        assert_eq!(
            check_install(&bare, Channel::MacBrewInstall),
            GuardDecision::Allow
        );
        // Windows：Unknown 且没有 reparse / NVM_SYMLINK 证据时必须放行（A6 裸 MSI 场景）
        assert_eq!(
            check_install(&env, Channel::WindowsMsi),
            GuardDecision::Allow
        );

        // 卸载侧相反：Unknown 参与保守判定（无 receipt 就不许盲删）
        assert_ne!(
            check_uninstall(&bare, Channel::MacPkg),
            GuardDecision::Allow,
            "D2 的保守只应体现在卸载侧"
        );
    }

    /// 版本令牌替换：护栏建议的版本必须与 DevUp 本来要装的那一个一致
    #[test]
    fn guard_apply_target_version_fills_token() {
        let env = NodeEnv {
            owner: NodeOwner::Manager(ManagerKind::Nvm),
            managers: vec![mgrv(ManagerKind::Nvm, "/Users/u/.nvm", &["v26.12.0"])],
            version: Some("26.12.0".into()),
            ..Default::default()
        };
        let got = apply_target_version(check_install(&env, Channel::MacBrewInstall), "v24.22.0");
        let GuardDecision::Refuse { commands, .. } = got else {
            panic!("nvm 在场的 brew install 必须被拦");
        };
        assert_eq!(
            commands,
            vec![
                "nvm install 24.22.0".to_string(),
                "nvm use 24.22.0".to_string()
            ]
        );
        // 令牌不能漏在文案里（用户会照抄横幅上的命令）
        for c in &commands {
            assert!(!c.contains(GUARD_VERSION_TOKEN), "命令残留令牌: {c}");
        }

        // Allow 路径原样返回
        assert_eq!(
            apply_target_version(GuardDecision::Allow, "v24.22.0"),
            GuardDecision::Allow
        );
    }

    /// 任务 H 的弹窗预览 = 拒绝横幅的同一条输出（同源钉死，防「弹窗一套、横幅另一套」）
    #[test]
    fn guidance_preview_is_the_same_output_as_refusal() {
        let env = NodeEnv {
            owner: NodeOwner::Manager(ManagerKind::Nvm),
            managers: vec![mgrv(ManagerKind::Nvm, "/Users/u/.nvm", &["v26.12.0"])],
            version: Some("26.12.0".into()),
            ..Default::default()
        };
        let got = guidance_preview(&env, Channel::MacPkg, "v24.22.0", false)
            .expect("nvm 托管机器的 MacPkg 必拒，预览必须给出");
        assert!(
            got.reason.contains("nvm"),
            "理由要点名托管方: {}",
            got.reason
        );
        assert!(
            got.reason.contains("/Users/u/.nvm"),
            "理由必须带路径证据（与横幅同一硬要求）"
        );
        assert_eq!(
            got.commands,
            vec![
                "nvm install 24.22.0".to_string(),
                "nvm use 24.22.0".to_string()
            ],
            "预览命令必须已填版本、与横幅逐字一致"
        );
        assert!(got.note.is_some(), "预览必须带逃生口提示");

        // 放行路径没有预览：前端保持普通确认弹窗（裸装机器不该看到「不代为执行」）
        let bare = NodeEnv {
            owner: NodeOwner::SystemPkg,
            pkg_receipt: true,
            ..Default::default()
        };
        assert_eq!(
            guidance_preview(&bare, Channel::MacPkg, "v24.22.0", false),
            None
        );

        // 卸载侧走 `check_uninstall`：无 receipt 的 mac 机必拒（G5），有 receipt 就放行（预览退出）
        let g5 = guidance_preview(&env, Channel::MacPkg, "v24.22.0", true)
            .expect("nvm 托管 + 无 receipt：卸载预览必须拒绝");
        assert!(
            g5.reason.contains("receipt"),
            "卸载拒绝要讲清 G5 依据: {}",
            g5.reason
        );
        assert_eq!(
            g5.commands,
            vec!["nvm uninstall 26.12.0".to_string()],
            "卸载指引必须是移除形态 + 已装版本——装命令或目标版本都是语义反转（用户走查点破）"
        );
        let with_receipt = NodeEnv {
            owner: NodeOwner::SystemPkg,
            pkg_receipt: true,
            ..Default::default()
        };
        assert_eq!(
            guidance_preview(&with_receipt, Channel::MacPkg, "v24.22.0", true),
            None,
            "有 receipt 的卸载放行——确认步骤必须留给真会删的动作"
        );
    }

    /// 卸载侧建议命令的三条纪律：每个 kind 都有命令、不留 `{ver}` 残留、
    /// **绝不混入安装族命令**（G 走查后用户点破的语义反转）；无版本可填时退化成只读 list。
    #[test]
    fn uninstall_hint_commands_never_install() {
        let kinds = [
            ManagerKind::Nvm,
            ManagerKind::NvmWindows,
            ManagerKind::Fnm,
            ManagerKind::Volta,
            ManagerKind::Mise,
            ManagerKind::Asdf,
            ManagerKind::Proto,
            ManagerKind::N,
            ManagerKind::Scoop,
            ManagerKind::Nvs,
        ];
        for kind in kinds {
            let env = NodeEnv {
                owner: NodeOwner::Manager(kind),
                managers: vec![mgrv(kind, "/Users/u/mgr-root", &["22.14.0"])],
                version: Some("22.14.0".into()),
                ..Default::default()
            };
            let got = uninstall_hint_commands(&env);
            assert!(!got.is_empty(), "{kind:?} 卸载指引不能为空");
            for c in &got {
                assert!(!c.contains(GUARD_VERSION_TOKEN), "{kind:?} 残留令牌: {c}");
                assert!(
                    !c.starts_with("nvm install") && !c.contains(" install "),
                    "{kind:?} 卸载指引混入安装命令: {c}"
                );
            }
            // 版本一条都没有：退化成只读 list，而不是无参数的半成品命令
            let env0 = NodeEnv {
                owner: NodeOwner::Manager(kind),
                managers: vec![mgrv(kind, "/Users/u/mgr-root", &[])],
                ..Default::default()
            };
            for c in uninstall_hint_commands(&env0) {
                assert!(
                    !c.contains(GUARD_VERSION_TOKEN),
                    "{kind:?} 无版本仍残留令牌: {c}"
                );
                assert!(
                    !c.ends_with("uninstall") && !c.ends_with(" remove") && !c.ends_with(" rm"),
                    "{kind:?} 无版本时不得给缺参数的移除命令: {c}"
                );
            }
        }
    }

    /// 每个 ManagerKind 都必须给得出至少一条命令：拒绝横幅不能是空的
    #[test]
    fn guard_commands_cover_every_manager_kind() {
        let kinds = [
            ManagerKind::Nvm,
            ManagerKind::NvmWindows,
            ManagerKind::Fnm,
            ManagerKind::Volta,
            ManagerKind::Mise,
            ManagerKind::Asdf,
            ManagerKind::Proto,
            ManagerKind::N,
            ManagerKind::Scoop,
            ManagerKind::Nvs,
        ];
        for kind in kinds {
            let env = NodeEnv {
                owner: NodeOwner::Manager(kind),
                managers: vec![mgr(kind, "/Users/u/mgr-root")],
                ..Default::default()
            };
            let got = apply_target_version(check_install(&env, Channel::MacPkg), "24.22.0");
            let GuardDecision::Refuse {
                reason, commands, ..
            } = got
            else {
                panic!("{kind:?} 有管理器在场时 PKG 必须被拦");
            };
            assert!(
                reason.contains(kind.label()),
                "{kind:?} 的文案没点名托管方：{reason}"
            );
            assert!(
                !commands.is_empty() && reason.contains("/Users/u/mgr-root"),
                "{kind:?} 缺命令或 reason 缺路径：{commands:?} / {reason}"
            );
            for c in &commands {
                assert!(!c.contains(GUARD_VERSION_TOKEN), "{kind:?} 残留令牌: {c}");
            }
        }
    }

    /// 真机护栏结论：`cargo test -- --ignored --nocapture dump_guard_decisions`
    ///
    /// 这是「点不到 GUI」时最接近真实的验证：本机装了 nvm + volta、没装 brew node、
    /// `pkg_receipt=false`，所以预期 `MacBrewInstall` 与 `MacPkg` 都该拒绝，
    /// `WindowsMsi` 该放行（两条 Windows 事实标志在本机恒 false，代码路径未经 Windows 验证）。
    #[test]
    #[ignore]
    fn dump_guard_decisions() {
        let env = detect();
        let ver = crate::detector::Detector::get_latest_version("nodejs")
            .unwrap_or_else(|| "?".to_string());
        println!("== 本机 NodeEnv ==");
        println!("owner          = {:?}", env.owner);
        println!("active_node    = {:?}", env.active_node);
        println!("version        = {:?}", env.version);
        println!("managers       = {:?}", env.managers);
        println!("pkg_receipt    = {}", env.pkg_receipt);
        println!(
            "reparse/symlink = {}/{}",
            env.nodejs_dir_is_reparse, env.nvm_symlink_is_nodejs_dir
        );
        println!("目标版本       = {ver}");

        for ch in [
            Channel::WindowsMsi,
            Channel::MacPkg,
            Channel::MacBrewInstall,
        ] {
            println!("\n---- 渠道 {ch:?} ----");
            for is_install in [true, false] {
                let raw = if is_install {
                    check_install(&env, ch)
                } else {
                    check_uninstall(&env, ch)
                };
                let label = if is_install { "安装" } else { "卸载" };
                match apply_target_version(raw, &ver) {
                    GuardDecision::Allow => println!("{label}: Allow"),
                    GuardDecision::Refuse {
                        reason,
                        commands,
                        note,
                    } => {
                        println!("{label}: Refuse");
                        println!("  reason   = {reason}");
                        for c in &commands {
                            println!("  command  = {c}");
                        }
                        println!("  note     = {:?}", note.as_deref().unwrap_or(""));
                    }
                }
            }
        }
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
