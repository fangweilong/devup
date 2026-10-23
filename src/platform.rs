//! 跨平台基础设施适配层：命令查找、PATH 扩展、外链打开、架构标识等。
//!
//! macOS 通过 Finder/LaunchServices 启动的 GUI 进程 PATH 非常短
//! (通常只有 /usr/bin:/bin:/usr/sbin:/sbin)，无法直接找到 brew、
//! node、npm 等用户级命令，因此需要显式扩展搜索目录与子进程 PATH。
//!
//! PATH 合并规则：用户现有 PATH 的相对顺序原样保留，DevUp 的补充目录插在
//! **系统兜底目录（/usr/bin 等）之前**。两条各治一个真实回归：
//! - 不前置补充目录：装了 nvm/volta/fnm 的机器上，用户终端里生效的那份 node
//!   不能被 Homebrew 压掉，否则探测结果与 `npm -g` 落点都偏离用户终端的现实；
//! - 不无脑追加到末尾：Finder 启动时 current 就是 `/usr/bin:/bin:/usr/sbin:/sbin`，
//!   若 brew 排到其后，装了 brew git 的用户会被 /usr/bin/git（CLT）遮蔽。

use std::path::PathBuf;
#[cfg(unix)]
use std::path::Path;

#[cfg(windows)]
use std::process::Command;

/// 判断 unix 文件是否存在且可执行
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 用户主目录
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Windows 高频安装目录（用户本地 bin、npm、WinGet、cargo、bun 等）
#[cfg(windows)]
pub fn extra_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join("AppData").join("Roaming").join("npm"));
        dirs.push(home.join("AppData").join("Local").join("omp"));
        dirs.push(home.join("AppData").join("Local").join("Microsoft").join("WinGet").join("Links"));
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join(".opencode").join("bin"));
        dirs.push(home.join("AppData").join("Local").join("Programs").join("OpenAI").join("Codex").join("bin"));
        // Antigravity CLI 官方落点：%LOCALAPPDATA%\agy\bin
        dirs.push(home.join("AppData").join("Local").join("agy").join("bin"));
    }
    dirs.push(PathBuf::from("C:\\Program Files\\PowerShell\\7"));
    dirs.push(PathBuf::from("C:\\Program Files\\Git\\cmd"));
    dirs.push(PathBuf::from("C:\\Program Files\\nodejs"));
    dirs
}

/// macOS GUI 启动场景下额外补充的高频安装目录（Homebrew、npm 全局、用户本地 bin）
#[cfg(target_os = "macos")]
pub fn extra_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".npm-global/bin"));
    }
    dirs
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn extra_search_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// 重新读取系统注册表与环境配置，动态刷新进程 PATH
#[cfg(windows)]
pub fn refresh_process_path() -> String {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let mut parts: Vec<String> = Vec::new();

    let query_reg_path = |hkey, subkey_path| -> Option<String> {
        let root = RegKey::predef(hkey);
        let sub = root.open_subkey(subkey_path).ok()?;
        sub.get_value("Path").or_else(|_| sub.get_value("PATH")).ok()
    };

    // 1. 读取注册表系统 PATH
    if let Some(sys_path) = query_reg_path(HKEY_LOCAL_MACHINE, "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment") {
        for p in sys_path.split(';') {
            let trimmed = p.trim();
            if !trimmed.is_empty() && !parts.iter().any(|x| x.eq_ignore_ascii_case(trimmed)) {
                parts.push(trimmed.to_string());
            }
        }
    }

    // 2. 读取注册表用户 PATH
    if let Some(user_path) = query_reg_path(HKEY_CURRENT_USER, "Environment") {
        for p in user_path.split(';') {
            let trimmed = p.trim();
            if !trimmed.is_empty() && !parts.iter().any(|x| x.eq_ignore_ascii_case(trimmed)) {
                parts.push(trimmed.to_string());
            }
        }
    }

    // 3. 追加高频工具目录（如目录存在）
    for extra in extra_search_dirs() {
        if extra.is_dir() {
            let s = extra.to_string_lossy().to_string();
            if !parts.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
                parts.push(s);
            }
        }
    }

    // 4. 合并当前进程可能包含的会话专有 PATH
    if let Ok(curr) = std::env::var("PATH") {
        for p in curr.split(';') {
            let trimmed = p.trim();
            if !trimmed.is_empty() && !parts.iter().any(|x| x.eq_ignore_ascii_case(trimmed)) {
                parts.push(trimmed.to_string());
            }
        }
    }

    let joined = parts.join(";");
    // SAFETY: 在单进程或主事件驱动中更新当前进程的 PATH 环境变量
    unsafe {
        std::env::set_var("PATH", &joined);
    }
    joined
}

#[cfg(not(windows))]
pub fn refresh_process_path() -> String {
    child_path()
}

/// Unix 系统兜底目录：登录环境自带，不代表用户主动配置的优先级
#[cfg(unix)]
const SYSTEM_PATH_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/System/Cryptexes/App/usr/bin",
];

/// 合并 Unix PATH：原样保留 current 的相对顺序，把 extras 中尚未出现的目录
/// 插在**第一个系统兜底目录之前**；current 里没有系统兜底目录时追加到末尾。
///
/// 纯函数：不读写环境变量、不执行命令，便于表驱动单测（大小写敏感的精确比较）。
/// 空分段（`::` / 首尾 `:`）会被丢弃，重复项只保留首次出现的位置。
#[cfg(unix)]
pub fn merge_path(current: &str, extras: &[PathBuf]) -> String {
    let mut segs: Vec<String> = Vec::new();
    for seg in current.split(':') {
        if seg.is_empty() || segs.iter().any(|p| p == seg) {
            continue;
        }
        segs.push(seg.to_string());
    }

    // 补充目录若一路排到系统目录之后，Finder 启动场景（current 恰好是
    // /usr/bin:/bin:/usr/sbin:/sbin）会让 /usr/bin/git 遮蔽 brew 的 git
    let insert_at = segs
        .iter()
        .position(|s| SYSTEM_PATH_DIRS.contains(&s.as_str()))
        .unwrap_or(segs.len());

    let mut offset = 0usize;
    for extra in extras {
        let s = extra.to_string_lossy().into_owned();
        if s.is_empty() || segs.iter().any(|p| p == &s) {
            continue;
        }
        segs.insert(insert_at + offset, s);
        offset += 1;
    }

    segs.join(":")
}

/// 为派生子进程构造的 PATH：继承用户现有 PATH 并补齐常用安装目录，
/// 保证各平台命令及 npm 全局 shim 均可被解析（合并顺序见 `merge_path`）。
pub fn child_path() -> String {
    #[cfg(windows)]
    {
        refresh_process_path()
    }
    #[cfg(unix)]
    {
        let current = std::env::var("PATH").unwrap_or_default();
        let joined = merge_path(&current, &extra_search_dirs());
        // SAFETY: 在单进程或主事件驱动中更新当前进程的 PATH 环境变量
        unsafe {
            std::env::set_var("PATH", &joined);
        }
        joined
    }
}

/// 在 PATH 与补充目录中查找可执行命令（类 which），返回绝对路径。
/// 顺序与 `child_path()` 共用 `merge_path`，因此不依赖调用方是否先刷新过进程 PATH。
#[cfg(unix)]
pub fn which(name: &str) -> Option<PathBuf> {
    let current = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&merge_path(&current, &extra_search_dirs()))
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

/// Apple Silicon 输出 arm64，Intel 输出 x64（与 GitHub Release 资产命名一致）
#[cfg(target_os = "macos")]
pub fn mac_arch_label() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    }
}

/// 使用系统默认浏览器/协议处理器打开 URL
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "start", "", url]);
        cmd.creation_flags(0x08000000);
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;

    fn dirs(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    /// 回归主案：用户终端里生效的 nvm/volta node，优先级不能被 Homebrew 抢走。
    /// 旧实现把补充目录前置，导致探测结果与 `npm -g` 落点都偏离用户终端的现实。
    #[test]
    fn merge_path_keeps_manager_priority_over_homebrew() {
        let current = "/Users/x/.nvm/versions/node/v26.12.0/bin:/usr/bin:/bin";
        let merged = merge_path(current, &dirs(&["/opt/homebrew/bin", "/usr/local/bin"]));

        let segs: Vec<&str> = merged.split(':').collect();
        let nvm = segs
            .iter()
            .position(|p| p.contains(".nvm"))
            .expect("用户自身的 nvm 目录必须保留");
        let brew = segs
            .iter()
            .position(|p| *p == "/opt/homebrew/bin")
            .expect("Homebrew 目录仍需补齐，否则 GUI 启动时找不到 brew");
        assert!(nvm < brew, "用户 PATH 必须排在补充目录之前: {merged}");
    }

    #[test]
    fn merge_path_table() {
        struct Case {
            name: &'static str,
            current: &'static str,
            extras: &'static [&'static str],
            want: &'static str,
        }
        let cases = [
            Case {
                name: "空 PATH 时只用补充目录",
                current: "",
                extras: &["/opt/homebrew/bin", "/usr/local/bin"],
                want: "/opt/homebrew/bin:/usr/local/bin",
            },
            Case {
                name: "已存在的补充目录不重复追加，且保留首次位置",
                current: "/opt/homebrew/bin:/usr/bin",
                extras: &["/opt/homebrew/bin", "/usr/local/bin"],
                want: "/opt/homebrew/bin:/usr/local/bin:/usr/bin",
            },
            Case {
                name: "空分段（首尾冒号与连续冒号）被丢弃",
                current: ":/usr/bin::/bin:",
                extras: &[],
                want: "/usr/bin:/bin",
            },
            Case {
                name: "用户 PATH 内部重复只保留首次出现",
                current: "/usr/bin:/usr/bin:/bin",
                extras: &[],
                want: "/usr/bin:/bin",
            },
            Case {
                name: "大小写不同视为不同目录（精确比较，与旧实现一致）",
                current: "/Opt/Homebrew/bin",
                extras: &["/opt/homebrew/bin"],
                want: "/Opt/Homebrew/bin:/opt/homebrew/bin",
            },
            Case {
                name: "原样保留用户写下的相对目录",
                current: ".:bin:/usr/bin",
                extras: &[],
                want: ".:bin:/usr/bin",
            },
            Case {
                name: "两者皆空得到空串",
                current: "",
                extras: &[],
                want: "",
            },
            Case {
                // Finder/LaunchServices 启动的真实形态：补充目录一旦排到系统目录之后，
                // /usr/bin/git（CLT）就会遮蔽 brew 的 git，卡片版本与托管方式双双误报
                name: "GUI 启动：补充目录插在系统兜底目录之前",
                current: "/usr/bin:/bin:/usr/sbin:/sbin",
                extras: &["/opt/homebrew/bin", "/usr/local/bin"],
                want: "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            },
            Case {
                name: "用户 PATH 自身的相对顺序不被改写（系统目录之后的用户目录仍留在其后）",
                current: "/usr/bin:/Users/x/.volta/bin",
                extras: &["/opt/homebrew/bin"],
                want: "/opt/homebrew/bin:/usr/bin:/Users/x/.volta/bin",
            },
        ];

        for c in cases {
            assert_eq!(
                merge_path(c.current, &dirs(c.extras)),
                c.want,
                "用例失败: {}",
                c.name
            );
        }
    }

    /// 人工核对全量 PATH 优先级与 node 解析结果：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_path_resolution() {
        let current = std::env::var("PATH").unwrap_or_default();
        let extras = extra_search_dirs();

        println!("原始 PATH:");
        for seg in current.split(':') {
            println!("  {seg}");
        }
        println!("合并后（用户顺序保留，补充目录插在系统兜底目录之前）:");
        for seg in merge_path(&current, &extras).split(':') {
            println!("  {seg}");
        }
        println!("which(node) = {:?}", which("node"));
        println!("which(npm)  = {:?}", which("npm"));
        println!("which(brew) = {:?}", which("brew"));
    }
}
