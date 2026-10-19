use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::platform;

/// Homebrew 清华镜像环境变量（国内网络加速），仅作用于 DevUp 发起的 brew 调用
fn brew_mirror_env(cmd: &mut Command) {
    cmd.env("HOMEBREW_BOTTLE_DOMAIN", "https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1");
}

pub struct MacInstaller;

impl MacInstaller {
    /// 当前机器是否存在 brew 命令
    pub fn brew_available() -> bool {
        platform::which("brew").is_some()
    }

    /// brew 中是否已安装指定 formula
    pub fn brew_installed_formula(formula: &str) -> bool {
        let Some(brew) = platform::which("brew") else {
            return false;
        };
        std::process::Command::new(brew)
            .args(["list", "--versions", formula])
            .env("PATH", platform::child_path())
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false)
    }

    /// 执行 brew 子命令并逐行透传输出（安装/升级/卸载 formula）
    pub async fn brew_run<F>(args: &[&str], mut on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let brew = platform::which("brew").context("未检测到 Homebrew，请先安装 Homebrew")?;
        let mut cmd = Command::new(brew);
        cmd.args(args);
        cmd.env("PATH", platform::child_path());
        brew_mirror_env(&mut cmd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 brew {:?} 失败", args))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        drop(tx);

        while let Some(line) = rx.recv().await {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                on_log(trimmed.to_string());
            }
        }

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("brew {:?} 退出码异常: {:?}", args, status.code())
        }
    }

    /// 安装或升级 formula：已存在则 upgrade，否则 install
    pub async fn brew_install_or_upgrade<F>(formula: &str, on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        if Self::brew_installed_formula(formula) {
            Self::brew_run(&["upgrade", formula], on_log).await
        } else {
            Self::brew_run(&["install", formula], on_log).await
        }
    }

    /// 以管理员权限执行 shell 片段（弹出 macOS 原生授权对话框）
    pub async fn run_admin_shell(script_body: &str, prompt: &str) -> Result<()> {
        // AppleScript 字符串内的双引号需转义
        let escaped = script_body.replace('\\', "\\\\").replace('"', "\\\"");
        let source = format!(
            r#"do shell script "{}" with administrator privileges with prompt "{}""#,
            escaped, prompt
        );

        let output = Command::new("osascript")
            .args(["-e", &source])
            .output()
            .await
            .context("调用 osascript 管理员授权失败")?;

        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            // 用户取消授权时 osascript 返回 -128
            if err.contains("-128") {
                bail!("已取消管理员授权，安装中止");
            }
            bail!("管理员命令执行失败: {}", err.trim())
        }
    }

    /// 静默安装官方 .pkg 安装包（经原生授权对话框提权）
    pub async fn install_pkg(pkg_path: &Path, prompt: &str) -> Result<()> {
        let script = format!(
            "/usr/sbin/installer -pkg '{}' -target /",
            pkg_path.to_string_lossy().replace('\'', "'\\''")
        );
        Self::run_admin_shell(&script, prompt).await
    }

    /// 在系统终端中打开一个交互式脚本（用于 Homebrew 官方安装/卸载这类
    /// 必须真人输入密码、拒绝 root 直接执行的长流程）
    pub async fn open_in_terminal(script: &str) -> Result<()> {
        let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
        let source = format!(
            r#"tell application "Terminal"
    activate
    do script "{}"
end tell"#,
            escaped
        );
        let output = Command::new("osascript")
            .args(["-e", &source])
            .output()
            .await
            .context("调用 osascript 打开终端失败")?;
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("唤起终端失败: {}", err.trim())
        }
    }

    /// 以 root 权限清理 npmmirror PKG 安装的 Node.js 文件
    pub async fn remove_pkg_node_files(prompt: &str) -> Result<()> {
        let script = "rm -rf /usr/local/bin/node /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack /usr/local/lib/node_modules /usr/local/include/node && pkgutil --forget org.nodejs.node.pkg 2>/dev/null; true";
        Self::run_admin_shell(script, prompt).await
    }
}

/// Homebrew 官方安装脚本（预置清华镜像环境变量，国内网络友好）
pub const HOMEBREW_INSTALL_SCRIPT: &str = r#"export HOMEBREW_INSTALL_FROM_API=1 HOMEBREW_API_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles/api" HOMEBREW_BOTTLE_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles" HOMEBREW_BREW_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/brew.git" HOMEBREW_CORE_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/homebrew-core.git"; /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#;

/// Homebrew 官方卸载脚本
pub const HOMEBREW_UNINSTALL_SCRIPT: &str = r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/uninstall.sh)""#;
