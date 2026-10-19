use serde::{Deserialize, Serialize};
use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
#[cfg(windows)]
use winreg::RegKey;

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoftwareStatus {
    pub id: String,
    pub name: String,
    pub category: String, // "env" 或 "agent"
    pub description: String,
    pub repo_url: String,
    pub installed: bool,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub install_method: Option<String>,
    pub default_channel: String, // 安装方式说明，例如 "WinGet / 微软官方MSIXBundle"、"国内官方npmmirror镜像 / 原生安装器" 等
}

pub struct Detector;

impl Detector {
    /// 清洗与格式化从 CLI 输出中读取到的版本字符串
    /// 例如: "2.1.303 (Claude Code)" -> "2.1.303"
    ///       "codex-cli 0.163.0" -> "0.163.0"
    ///       "omp/18.12.2" -> "18.12.2"
    ///       "opencode v2.0.32" -> "2.0.32"
    ///       "git version 2.55.0.windows.3" -> "2.55.0"
    ///       "PowerShell 7.4.20" -> "7.4.20"
    ///       "v24.21.0" -> "24.21.0"
    pub fn clean_version_string(raw: &str) -> String {
        let trimmed = raw.trim();
        for part in trimmed.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '/' || c == ',') {
            let p = part.trim().trim_start_matches(|c| c == 'v' || c == 'V');
            if p.is_empty() {
                continue;
            }
            let dots = p.chars().filter(|&c| c == '.').count();
            let digits_start = p.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
            if digits_start && dots >= 1 {
                let clean_p = p.split(|c: char| c.is_alphabetic()).next().unwrap_or(p).trim_end_matches('.');
                if !clean_p.is_empty() {
                    return clean_p.to_string();
                }
            }
        }
        trimmed.to_string()
    }

    /// 探测命令是否存在并读取版本输出（跨平台入口）
    pub fn check_command(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        #[cfg(windows)]
        {
            Self::check_command_windows(cmd, version_arg)
        }
        #[cfg(not(windows))]
        {
            Self::check_command_unix(cmd, version_arg)
        }
    }

    /// Unix/macOS：which 风格查找 + 直接执行取版本（GUI 进程需扩展 PATH）
    #[cfg(not(windows))]
    fn check_command_unix(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        let base_cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);

        let path = crate::platform::which(base_cmd)
            .or_else(|| crate::platform::which(cmd))
            .map(|p| p.to_string_lossy().to_string());
        let Some(found) = path else {
            return (false, None, None);
        };

        let mut command = Command::new(&found);
        command.arg(version_arg);
        command.env("PATH", crate::platform::child_path());

        let version = command.output().ok().and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() {
                    let err_s = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if !err_s.is_empty() {
                        return Some(err_s.lines().next().unwrap_or("").to_string());
                    }
                    None
                } else {
                    Some(s.lines().next().unwrap_or("").to_string())
                }
            } else {
                None
            }
        }).map(|raw| Self::clean_version_string(&raw));

        (true, version, Some(found))
    }

    /// Windows: where.exe 探测 + cmd /c 兜底执行（支持 npm 全局 .cmd shim）
    #[cfg(windows)]
    fn check_command_windows(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        let base_cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);

        // 1. 先用 where.exe 探测（同时支持 .exe, .cmd, .bat 等扩展名）
        let mut where1 = Command::new("where.exe");
        where1.arg(base_cmd);
        #[cfg(windows)]
        where1.creation_flags(CREATE_NO_WINDOW);

        let mut where2 = Command::new("where.exe");
        where2.arg(cmd);
        #[cfg(windows)]
        where2.creation_flags(CREATE_NO_WINDOW);

        let out1 = where1.output().ok();
        let out2 = where2.output().ok();

        let exists = out1.as_ref().map(|o| o.status.success()).unwrap_or(false)
            || out2.as_ref().map(|o| o.status.success()).unwrap_or(false);

        if !exists {
            return (false, None, None);
        }

        let cmd_path = out1
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                s.lines().next().map(|l| l.trim().to_string())
            })
            .or_else(|| {
                out2.and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    s.lines().next().map(|l| l.trim().to_string())
                })
            });

        // 2. 尝试获取版本信息：优先直接执行，失败则回退到 cmd /c（支持 npm 全局安装的 .cmd / .bat 脚本）
        let try_run = |use_shell: bool| -> Option<String> {
            let mut command = if use_shell {
                let mut c = Command::new("cmd.exe");
                c.args(["/c", base_cmd, version_arg]);
                c
            } else {
                let mut c = Command::new(cmd);
                c.arg(version_arg);
                c
            };

            #[cfg(windows)]
            command.creation_flags(CREATE_NO_WINDOW);

            command.output().ok().and_then(|out| {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if s.is_empty() {
                        let err_s = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        if !err_s.is_empty() {
                            return Some(err_s.lines().next().unwrap_or("").to_string());
                        }
                        None
                    } else {
                        Some(s.lines().next().unwrap_or("").to_string())
                    }
                } else {
                    None
                }
            })
        };

        let version = try_run(false)
            .or_else(|| try_run(true))
            .map(|raw| Self::clean_version_string(&raw));
        (true, version, cmd_path)
    }

    /// 检测 Windows Terminal 状态与版本（仅 Windows）
    /// 避免直接运行 `wt.exe -v` / `--version` 触发 GUI 弹窗
    #[cfg(windows)]
    pub fn check_windows_terminal() -> (bool, Option<String>, Option<String>) {
        {
            // 优先从注册表 App Paths 读取安装路径并提取版本号
            let query_reg = |hkey| -> Option<String> {
                let root = RegKey::predef(hkey);
                let subkey = root
                    .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\wt.exe")
                    .ok()?;
                let path: String = subkey.get_value("").ok()?;
                // 路径通常为: C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.24.12741.0_x64__8wekyb3d8bbwe\wt.exe
                if let Some(start) = path.find("Microsoft.WindowsTerminal_") {
                    let sub = &path[start + "Microsoft.WindowsTerminal_".len()..];
                    if let Some(end) = sub.find('_') {
                        return Some(sub[..end].to_string());
                    }
                }
                Some("已安装".to_string())
            };

            if let Some(ver) = query_reg(HKEY_CURRENT_USER).or_else(|| query_reg(HKEY_LOCAL_MACHINE)) {
                return (true, Some(ver), Some("winget/msix".to_string()));
            }
        }

        // 回退检查 where.exe 是否能找到 wt
        let mut cmd = Command::new("where.exe");
        cmd.arg("wt");
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
        (ok, if ok { Some("已安装".to_string()) } else { None }, if ok { Some("winget/msix".to_string()) } else { None })
    }

    /// 分析可执行文件路径，判定其安装途径
    pub fn infer_install_method(path_opt: Option<&str>, default_method: &str) -> Option<String> {
        let p = path_opt?.to_lowercase();
        #[cfg(windows)]
        {
            if p.contains("windowsapps") {
                Some("winget".to_string())
            } else if p.contains("node_modules") || p.contains("npm") || p.contains("nvm") {
                Some("npm".to_string())
            } else if p.contains(".local\\bin") || p.contains("appdata\\local\\omp") || p.contains("appdata\\local\\pi") {
                Some("native_cli".to_string())
            } else {
                Some(default_method.to_string())
            }
        }
        #[cfg(not(windows))]
        {
            if p.contains("/homebrew/") || p.contains("/cellar") {
                Some("brew".to_string())
            } else if p.contains("node_modules") || p.contains("/npm/") || p.contains("nvm") {
                Some("npm".to_string())
            } else if p.contains(".local/bin") {
                Some("native_cli".to_string())
            } else if p.starts_with("/usr/bin") || p.starts_with("/bin") || p.starts_with("/sbin") {
                Some("system".to_string())
            } else {
                Some(default_method.to_string())
            }
        }
    }

    /// 检测一个 CLI 软件并组装状态卡片数据
    fn probe_software(
        id: &str,
        name: &str,
        category: &str,
        description: &str,
        repo_url: &str,
        cli: &str,
        default_method: &str,
        latest_version: Option<&str>,
        default_channel: &str,
    ) -> SoftwareStatus {
        let (ok, ver, path) = Self::check_command(cli, "--version");
        let method = if ok {
            Self::infer_install_method(path.as_deref(), default_method)
        } else {
            None
        };
        SoftwareStatus {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            description: description.into(),
            repo_url: repo_url.into(),
            installed: ok,
            version: ver,
            latest_version: latest_version.map(|s| s.to_string()),
            install_method: method,
            default_channel: default_channel.into(),
        }
    }

    /// 综合全量实时检测所有环境与 Agent 状态
    pub fn detect_all() -> Vec<SoftwareStatus> {
        let mut list = Vec::new();

        // 1. 基础开发环境 (Prerequisites) —— 按平台差异化
        #[cfg(windows)]
        {
            let (wt_ok, wt_ver, wt_method) = Self::check_windows_terminal();
            list.push(SoftwareStatus {
                id: "windows_terminal".into(),
                name: "Windows Terminal".into(),
                category: "env".into(),
                description: "微软现代终端，支持多标签页、GPU 加速与 Unicode".into(),
                repo_url: "https://github.com/microsoft/terminal".into(),
                installed: wt_ok,
                version: wt_ver,
                latest_version: Some("v1.25.2733.0".into()),
                install_method: wt_method,
                default_channel: "WinGet / 微软官方MSIXBundle".into(),
            });

            list.push(Self::probe_software(
                "powershell7",
                "PowerShell 7",
                "env",
                "跨平台全新 PowerShell，现代脚本与 Agent 执行基础".into(),
                "https://github.com/PowerShell/PowerShell",
                "pwsh.exe",
                "msi",
                Some("v7.6.7"),
                "GitHub加速镜像 / WinGet",
            ));

            list.push(Self::probe_software(
                "nodejs",
                "Node.js (LTS)",
                "env",
                "Claude Code、Codex、Pi 等 Agent 必备的运行环境".into(),
                "https://nodejs.org",
                "node.exe",
                "msi",
                Some("v24.21.0"),
                "阿里npmmirror镜像官方MSI",
            ));

            list.push(Self::probe_software(
                "git",
                "Git",
                "env",
                "代码版本管理系统，绝大多数 Agent 的依赖项".into(),
                "https://git-scm.com",
                "git.exe",
                "installer",
                Some("v2.56.0"),
                "GitHub加速镜像 / WinGet",
            ));
        }

        #[cfg(target_os = "macos")]
        {
            list.push(Self::probe_software(
                "homebrew",
                "Homebrew",
                "env",
                "macOS 事实标准包管理器，托管 Node.js / Git 等工具链".into(),
                "https://brew.sh",
                "brew",
                "script",
                None,
                "官方脚本 / 清华镜像加速",
            ));

            list.push(Self::probe_software(
                "nodejs",
                "Node.js (LTS)",
                "env",
                "Claude Code、Codex、Pi 等 Agent 必备的运行环境".into(),
                "https://nodejs.org",
                "node",
                "pkg",
                Some("v24.21.0"),
                "Homebrew / 阿里npmmirror官方PKG",
            ));

            list.push(Self::probe_software(
                "git",
                "Git",
                "env",
                "代码版本管理系统，绝大多数 Agent 的依赖项".into(),
                "https://git-scm.com",
                "git",
                "system",
                Some("v2.56.0"),
                "Homebrew / Xcode 命令行工具",
            ));
        }

        // 2. AI Coding Agents（跨平台一致，check_command 自动处理 .exe 后缀）
        list.push(Self::probe_software(
            "oh-my-pi",
            "oh-my-pi (omp)",
            "agent",
            "带 IDE 级工具与高级编辑功能的终端 AI 编程智能体".into(),
            "https://github.com/can1357/oh-my-pi",
            "omp",
            "native_cli",
            Some("v18.12.2"),
            "官方原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "claudecode",
            "Claude Code",
            "agent",
            "Anthropic 官方研究级终端编码智能体 CLI".into(),
            "https://github.com/anthropics/claude-code",
            "claude",
            "native_cli",
            Some("v2.1.303"),
            "Anthropic原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "codex",
            "OpenAI Codex",
            "agent",
            "OpenAI 官方开源轻量级终端 Coding Agent".into(),
            "https://github.com/openai/codex",
            "codex",
            "npm",
            Some("v0.163.0"),
            "官方npm包 (@openai/codex)",
        ));

        list.push(Self::probe_software(
            "pi",
            "Pi Agent",
            "agent",
            "Earendil Works 统一 LLM 循环与终端交互编程框架".into(),
            "https://github.com/earendil-works/pi",
            "pi",
            "native_cli",
            Some("v1.3.0"),
            "官方原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "opencode",
            "OpenCode",
            "agent",
            "Anomaly 团队开源的高性能轻量 Coding Agent".into(),
            "https://github.com/anomalyco/opencode",
            "opencode",
            "npm",
            Some("v2.0.32"),
            "官方CLI自升级 / npmmirror",
        ));

        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_version_string() {
        assert_eq!(Detector::clean_version_string("2.1.303 (Claude Code)"), "2.1.303");
        assert_eq!(Detector::clean_version_string("codex-cli 0.163.0"), "0.163.0");
        assert_eq!(Detector::clean_version_string("omp/18.12.2"), "18.12.2");
        assert_eq!(Detector::clean_version_string("git version 2.55.0.windows.3"), "2.55.0");
        assert_eq!(Detector::clean_version_string("v24.21.0"), "24.21.0");
        assert_eq!(Detector::clean_version_string("Homebrew 4.5.11"), "4.5.11");
    }

    /// 人工核对全量探测结果：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_detect_all() {
        for s in Detector::detect_all() {
            println!(
                "[{}] {} installed={} ver={:?} latest={:?} method={:?} channel={}",
                s.category, s.name, s.installed, s.version, s.latest_version, s.install_method, s.default_channel
            );
        }
    }
}
