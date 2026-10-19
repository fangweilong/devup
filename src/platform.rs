//! 跨平台基础设施适配层：命令查找、PATH 扩展、外链打开、架构标识等。
//!
//! macOS 通过 Finder/LaunchServices 启动的 GUI 进程 PATH 非常短
//! (通常只有 /usr/bin:/bin:/usr/sbin:/sbin)，无法直接找到 brew、
//! node、npm 等用户级命令，因此需要显式扩展搜索目录与子进程 PATH。

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
    std::env::set_var("PATH", &joined);
    joined
}

#[cfg(not(windows))]
pub fn refresh_process_path() -> String {
    child_path()
}

/// 为派生子进程构造的 PATH：继承并追加常用安装目录，
/// 保证各平台命令及 npm 全局 shim 均可被解析。
pub fn child_path() -> String {
    #[cfg(windows)]
    {
        refresh_process_path()
    }
    #[cfg(unix)]
    {
        let current = std::env::var("PATH").unwrap_or_default();
        let mut parts: Vec<String> = Vec::new();
        for extra in extra_search_dirs() {
            let s = extra.to_string_lossy().to_string();
            if !current.split(std::path::MAIN_SEPARATOR_STR).any(|p| p == s) {
                parts.push(s);
            }
        }
        parts.push(current);
        let joined = parts.join(":");
        let _ = std::env::set_var("PATH", &joined);
        joined
    }
}

/// 在 PATH 与补充目录中查找可执行命令（类 which），返回绝对路径
#[cfg(unix)]
pub fn which(name: &str) -> Option<PathBuf> {
    let bases: Vec<PathBuf> = std::env::split_paths(&std::env::var("PATH").unwrap_or_default())
        .chain(extra_search_dirs())
        .collect();
    bases.iter().map(|dir| dir.join(name)).find(|p| is_executable(p))
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
