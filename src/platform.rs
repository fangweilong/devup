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

/// 为派生子进程构造的 PATH：继承当前进程 PATH 并追加常用安装目录，
/// 保证 npm 全局 shim (claude/codex 等) 与 brew/node 均可被 shebang 解析。
#[cfg(unix)]
pub fn child_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for extra in extra_search_dirs() {
        let s = extra.to_string_lossy().to_string();
        if !current.split(std::path::MAIN_SEPARATOR_STR).any(|p| p == s) {
            parts.push(s);
        }
    }
    parts.push(current);
    parts.join(":")
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
