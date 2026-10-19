use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoftwareStatus {
    pub id: String,
    pub name: String,
    pub category: String, // "env" 或 "agent"
    pub description: String,
    pub repo_url: String,
    pub installed: bool,
    pub version: Option<String>,
}

pub struct Detector;

impl Detector {
    /// 实时检测命令是否存在并获取版本号（纯内存执行，零注册表污染）
    pub fn check_command(cmd: &str, version_arg: &str) -> (bool, Option<String>) {
        // 去除可能的 .exe 后缀用于通用探测
        let base_cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);

        // 1. 先用 where.exe 探测（同时支持 .exe, .cmd, .bat 等扩展名）
        let exists = Command::new("where.exe")
            .arg(base_cmd)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
            || Command::new("where.exe")
                .arg(cmd)
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false);

        if !exists {
            return (false, None);
        }

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

        let version = try_run(false).or_else(|| try_run(true));
        (true, version)
    }

    /// 综合全量实时检测所有环境与 Agent 状态
    pub fn detect_all() -> Vec<SoftwareStatus> {
        let mut list = Vec::new();

        // 1. 基础开发环境 (Prerequisites)
        let (wt_ok, wt_ver) = Self::check_command("wt.exe", "--version");
        list.push(SoftwareStatus {
            id: "windows_terminal".into(),
            name: "Windows Terminal".into(),
            category: "env".into(),
            description: "微软现代终端，支持多标签页、GPU 加速与 Unicode".into(),
            repo_url: "https://github.com/microsoft/terminal".into(),
            installed: wt_ok,
            version: wt_ver,
        });

        let (pwsh_ok, pwsh_ver) = Self::check_command("pwsh.exe", "--version");
        list.push(SoftwareStatus {
            id: "powershell7".into(),
            name: "PowerShell 7".into(),
            category: "env".into(),
            description: "跨平台全新 PowerShell，现代脚本与 Agent 执行基础".into(),
            repo_url: "https://github.com/PowerShell/PowerShell".into(),
            installed: pwsh_ok,
            version: pwsh_ver,
        });

        let (node_ok, node_ver) = Self::check_command("node.exe", "--version");
        list.push(SoftwareStatus {
            id: "nodejs".into(),
            name: "Node.js (LTS)".into(),
            category: "env".into(),
            description: "Claude Code、Codex、Pi 等 Agent 必备的运行环境".into(),
            repo_url: "https://nodejs.org".into(),
            installed: node_ok,
            version: node_ver,
        });

        let (git_ok, git_ver) = Self::check_command("git.exe", "--version");
        list.push(SoftwareStatus {
            id: "git".into(),
            name: "Git".into(),
            category: "env".into(),
            description: "代码版本管理系统，绝大多数 Agent 的依赖项".into(),
            repo_url: "https://git-scm.com".into(),
            installed: git_ok,
            version: git_ver,
        });

        // 2. AI Coding Agents
        let (omp_ok, omp_ver) = Self::check_command("omp.exe", "--version");
        list.push(SoftwareStatus {
            id: "oh-my-pi".into(),
            name: "oh-my-pi (omp)".into(),
            category: "agent".into(),
            description: "带 IDE 级工具与高级编辑功能的终端 AI 编程智能体".into(),
            repo_url: "https://github.com/can1357/oh-my-pi".into(),
            installed: omp_ok,
            version: omp_ver,
        });

        let (claude_ok, claude_ver) = Self::check_command("claude.exe", "--version");
        list.push(SoftwareStatus {
            id: "claudecode".into(),
            name: "Claude Code".into(),
            category: "agent".into(),
            description: "Anthropic 官方研究级终端编码智能体 CLI".into(),
            repo_url: "https://github.com/anthropics/claude-code".into(),
            installed: claude_ok,
            version: claude_ver,
        });

        let (codex_ok, codex_ver) = Self::check_command("codex.exe", "--version");
        list.push(SoftwareStatus {
            id: "codex".into(),
            name: "OpenAI Codex".into(),
            category: "agent".into(),
            description: "OpenAI 官方开源轻量级终端 Coding Agent".into(),
            repo_url: "https://github.com/openai/codex".into(),
            installed: codex_ok,
            version: codex_ver,
        });

        let (pi_ok, pi_ver) = Self::check_command("pi.exe", "--version");
        list.push(SoftwareStatus {
            id: "pi".into(),
            name: "Pi Agent".into(),
            category: "agent".into(),
            description: "Earendil Works 统一 LLM 循环与终端交互编程框架".into(),
            repo_url: "https://github.com/earendil-works/pi".into(),
            installed: pi_ok,
            version: pi_ver,
        });

        let (opencode_ok, opencode_ver) = Self::check_command("opencode.exe", "--version");
        list.push(SoftwareStatus {
            id: "opencode".into(),
            name: "OpenCode".into(),
            category: "agent".into(),
            description: "Anomaly 团队开源的高性能轻量 Coding Agent".into(),
            repo_url: "https://github.com/anomalyco/opencode".into(),
            installed: opencode_ok,
            version: opencode_ver,
        });

        list
    }
}
