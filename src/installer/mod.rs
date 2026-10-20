#[cfg(windows)]
pub mod msi;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod npm;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::detector::Detector;
use crate::network::downloader::{create_http_client, download_file};
use crate::network::mirrors::NODEJS_MIRROR_BASE;
use crate::state::AppEvent;

#[cfg(windows)]
use msi::WindowsInstaller;
#[cfg(target_os = "macos")]
use macos::MacInstaller;
use npm::NpmInstaller;

pub struct InstallerEngine;

/// AI Coding Agent 的安装规格（跨平台一致：原生自升级优先，npm 镜像兜底）
struct AgentSpec {
    label: &'static str,
    /// 已安装时优先调用的原生更新命令（None 表示仅 npm 渠道）
    cli: Option<&'static str>,
    update_args: &'static [&'static str],
    npm_pkg: &'static str,
    done_msg: &'static str,
}

fn agent_spec(software_id: &str) -> Option<AgentSpec> {
    let spec = match software_id {
        "claudecode" => AgentSpec {
            label: "Claude Code",
            cli: Some("claude"),
            update_args: &["update"],
            npm_pkg: "@anthropic-ai/claude-code",
            done_msg: "Claude Code 全局安装完成！直接在终端输入 `claude` 即可启动。",
        },
        "codex" => AgentSpec {
            label: "OpenAI Codex",
            cli: None,
            update_args: &[],
            npm_pkg: "@openai/codex",
            done_msg: "OpenAI Codex 全局安装完成！直接在终端输入 `codex` 即可启动。",
        },
        "pi" => AgentSpec {
            label: "Pi",
            cli: Some("pi"),
            update_args: &["update"],
            npm_pkg: "@earendil-works/pi",
            done_msg: "Pi Agent 安装成功！终端输入 `pi` 即可使用。",
        },
        "oh-my-pi" => AgentSpec {
            label: "omp",
            cli: Some("omp"),
            update_args: &["update"],
            npm_pkg: "@oh-my-pi/pi-coding-agent",
            done_msg: "oh-my-pi (omp) 安装成功！",
        },
        "opencode" => AgentSpec {
            label: "OpenCode",
            cli: Some("opencode"),
            update_args: &["upgrade"],
            npm_pkg: "@opencode/cli",
            done_msg: "OpenCode 安装成功！",
        },
        _ => return None,
    };
    Some(spec)
}

/// 卸载时 npm 全局包名（opencode 存在历史双包名需全部清理）
fn agent_uninstall_pkgs(software_id: &str) -> Option<&'static [&'static str]> {
    match software_id {
        "claudecode" => Some(&["@anthropic-ai/claude-code"]),
        "codex" => Some(&["@openai/codex"]),
        "pi" => Some(&["@earendil-works/pi"]),
        "oh-my-pi" => Some(&["@oh-my-pi/pi-coding-agent"]),
        "opencode" => Some(&["@opencode/cli", "opencode-ai"]),
        _ => None,
    }
}

impl InstallerEngine {
    /// 执行具体的软件安装任务（平台分发入口）
    pub async fn install_target(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        #[cfg(windows)]
        {
            Self::install_target_windows(software_id, proxy_url, mirror_config, &event_sender).await?;
        }
        #[cfg(target_os = "macos")]
        {
            Self::install_target_macos(software_id, proxy_url, mirror_config, &event_sender).await?;
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (&software_id, &proxy_url, &mirror_config, &event_sender);
            bail!("当前平台暂不支持自动安装: {}", software_id);
        }
        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });
        Ok(())
    }

    /// 执行具体的软件卸载任务（平台分发入口）
    pub async fn uninstall_target(
        software_id: &str,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        #[cfg(windows)]
        {
            Self::uninstall_target_windows(software_id, &event_sender).await?;
        }
        #[cfg(target_os = "macos")]
        {
            Self::uninstall_target_macos(software_id, &event_sender).await?;
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (&software_id, &event_sender);
            bail!("当前平台暂不支持自动卸载: {}", software_id);
        }
        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });
        Ok(())
    }

    fn make_log(event_sender: &UnboundedSender<AppEvent>, software_id: &str) -> impl Fn(String) + Send + 'static + Clone {
        let tx = event_sender.clone();
        let sid = software_id.to_string();
        move |msg: String| {
            let _ = tx.send(AppEvent::Log {
                target_id: sid.clone(),
                message: msg,
            });
        }
    }

    /// 跨平台共享的 Agent 安装：原生自升级优先，npmmirror 全局安装兜底
    async fn install_agent(
        software_id: &str,
        proxy_ref: Option<&str>,
        npm_registry: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let Some(spec) = agent_spec(software_id) else {
            bail!("未知 Agent 目标: {}", software_id);
        };
        let log = Self::make_log(event_sender, software_id);

        if let Some(cli) = spec.cli {
            let (has_cli, _, _) = Detector::check_command(cli, "--version");
            if has_cli {
                log(format!(
                    "检测到本地已安装 {}，优先执行官方原生更新命令 (`{} {}`)...",
                    spec.label,
                    cli,
                    spec.update_args.join(" ")
                ));
                let update_res =
                    NpmInstaller::run_cli_command(cli, spec.update_args, proxy_ref, log.clone()).await;
                if update_res.is_ok() {
                    log(format!("{} 官方原生更新成功！", spec.label));
                    return Ok(());
                }
                log("原生自升级未成功，回退到国内 npmmirror 镜像重新安装...".into());
            }
        }

        log(format!(
            "使用 npm 镜像 ({}) 安装 {} ({})...",
            npm_registry, spec.label, spec.npm_pkg
        ));
        NpmInstaller::install_global(spec.npm_pkg, proxy_ref, npm_registry, log.clone()).await?;
        log(spec.done_msg.to_string());
        Ok(())
    }

    /// 跨平台共享的 Agent 卸载：npm 全局包移除 + 原生二进制清理
    async fn uninstall_agent(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let log = Self::make_log(event_sender, software_id);
        let Some(pkgs) = agent_uninstall_pkgs(software_id) else {
            return Ok(());
        };

        let mut any_ok = false;
        let mut last_err = None;
        for pkg in pkgs {
            log(format!("正在卸载 npm 全局包 {}...", pkg));
            match NpmInstaller::uninstall_global(pkg, log.clone()).await {
                Ok(()) => any_ok = true,
                Err(e) => last_err = Some(e),
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
                bail!("npm 全局包卸载失败: {:#}", e);
            }
        }
        Ok(())
    }

    /// 清理 Agent 原生安装的可执行文件/数据目录（平台差异部分）
    async fn cleanup_native_artifacts(software_id: &str, event_sender: &UnboundedSender<AppEvent>) {
        let log = Self::make_log(event_sender, software_id);
        let native_bin = match software_id {
            "claudecode" => Some("claude"),
            "pi" => Some("pi"),
            "oh-my-pi" => Some("omp"),
            _ => None,
        };

        if let Some(bin) = native_bin {
            #[cfg(windows)]
            {
                if let Some(home) = crate::platform::home_dir() {
                    let exe = home.join(".local").join("bin").join(format!("{}.exe", bin));
                    if exe.exists() {
                        log(format!("移除原生可执行文件: {:?}", exe));
                        let _ = tokio::fs::remove_file(exe).await;
                    }
                }
                if software_id == "pi" {
                    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                        let dir = std::path::Path::new(&local_app).join("pi");
                        if dir.exists() {
                            let _ = tokio::fs::remove_dir_all(dir).await;
                        }
                    }
                }
                if software_id == "oh-my-pi" {
                    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                        let dir = std::path::Path::new(&local_app).join("omp");
                        if dir.exists() {
                            log(format!("清理原生文件目录: {:?}", dir));
                            let _ = tokio::fs::remove_dir_all(dir).await;
                        }
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                if let Some(home) = crate::platform::home_dir() {
                    let exe = home.join(".local").join("bin").join(bin);
                    if exe.exists() {
                        log(format!("移除原生可执行文件: {:?}", exe));
                        let _ = tokio::fs::remove_file(exe).await;
                    }
                }
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                let _ = (bin, log);
            }
        }
    }

    // ==================== Windows ====================

    #[cfg(windows)]
    async fn install_target_windows(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let proxy_ref = proxy_url.as_deref();
        let client = create_http_client(proxy_ref)?;

        let temp_dir = std::env::temp_dir().join("devup-cache");
        tokio::fs::create_dir_all(&temp_dir).await?;

        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始安装任务: {}", software_id));

        match software_id {
            "powershell7" => {
                let raw_url = "https://github.com/PowerShell/PowerShell/releases/download/v7.4.6/PowerShell-7.4.6-win-x64.msi";
                log(format!("正在根据配置解析 GitHub 下载源 (模式: {})...", mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;
                log(format!("选定加速下载链接: {}", download_url));

                let dest = temp_dir.join("PowerShell-7.4.6-win-x64.msi");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log("下载完成，正在后台静默安装 PowerShell 7...".into());
                log(format!("$ msiexec.exe /i \"{}\" /quiet /norestart ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1", dest.display()));
                WindowsInstaller::install_msi(&dest, &["ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1"]).await?;
                log("PowerShell 7 安装完成！".into());
            }

            "windows_terminal" => {
                // 如果系统支持 winget 且已通过应用商店/winget 安装，优先通过 winget 静默更新
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    log("检测到 Windows Terminal 来自 WinGet/应用商店，执行原生渠道静默升级...".into());
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Microsoft.WindowsTerminal", log.clone()).await {
                        log("Windows Terminal 通过 WinGet 渠道更新成功！".into());
                        return Ok(());
                    }
                    log("WinGet 渠道更新失败或已是最新，回退到国内 GitHub 离线包安装...".into());
                }

                let raw_url = "https://github.com/microsoft/terminal/releases/download/v1.25.2733.0/Microsoft.WindowsTerminal_1.25.2733.0_8wekyb3d8bbwe.msixbundle";
                log(format!("正在根据配置解析 GitHub 下载源 (模式: {})...", mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;
                log(format!("选定加速下载链接: {}", download_url));

                let dest = temp_dir.join("WindowsTerminal.msixbundle");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log("下载完成，正在静默部署 Windows Terminal AppX...".into());
                log(format!("$ powershell.exe -NoProfile -Command \"Add-AppxPackage -Path '{}'\"", dest.display()));
                WindowsInstaller::install_appx(&dest).await?;
                log("Windows Terminal 部署成功！".into());
            }

            "nodejs" => {
                let download_url = format!("{}/v20.18.0/node-v20.18.0-x64.msi", NODEJS_MIRROR_BASE);
                log(format!("正在从阿里 Node.js 镜像高速下载: {}", download_url));

                let dest = temp_dir.join("node-v20.18.0-x64.msi");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log("下载完成，正在后台静默安装 Node.js LTS...".into());
                log(format!("$ msiexec.exe /i \"{}\" /quiet /norestart", dest.display()));
                WindowsInstaller::install_msi(&dest, &[]).await?;
                log("Node.js LTS 安装成功！".into());
            }

            "git" => {
                // 如果检测到系统通过 winget 管理 Git，优先调用 winget upgrade
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    log("检测到 Git 注册于 WinGet，优先执行 winget 原生渠道更新...".into());
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Git.Git", log.clone()).await {
                        log("Git 通过 WinGet 更新成功！".into());
                        return Ok(());
                    }
                    log("WinGet 更新回退，改用官网安装包...".into());
                }

                let raw_url = "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.2/Git-2.56.0.2-64-bit.exe";
                log(format!("正在根据配置解析 Git for Windows 国内加速地址 (模式: {})...", mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;

                let dest = temp_dir.join("Git-Installer.exe");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log("正在后台静默安装 Git...".into());
                log(format!("$ \"{}\" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL /SP- /CLOSEAPPLICATIONS", dest.display()));
                let mut cmd = tokio::process::Command::new(&dest);
                cmd.args([
                    "/VERYSILENT",
                    "/SUPPRESSMSGBOXES",
                    "/NORESTART",
                    "/NOCANCEL",
                    "/SP-",
                    "/CLOSEAPPLICATIONS",
                ]);
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);

                let out = cmd.output().await?;
                if !out.status.success() {
                    bail!("Git 静默安装异常: {:?}", out.status.code());
                }
                log("Git 安装成功！".into());
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::install_agent(software_id, proxy_ref, &mirror_config.npm_registry, event_sender).await?;
            }

            other => {
                bail!("未知软件目标: {}", other);
            }
        }

        Ok(())
    }

    #[cfg(windows)]
    async fn uninstall_target_windows(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始执行卸载任务: {}", software_id));

        match software_id {
            "windows_terminal" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    log("正在通过 WinGet 静默卸载 Windows Terminal...".into());
                    WindowsInstaller::uninstall_via_winget("Microsoft.WindowsTerminal", log.clone()).await?;
                    log("Windows Terminal 已卸载！".into());
                } else {
                    log("正在通过 PowerShell Remove-AppxPackage 卸载 Windows Terminal...".into());
                    let script = "Get-AppxPackage *WindowsTerminal* | Remove-AppxPackage -ErrorAction SilentlyContinue";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                    log("Windows Terminal 卸载指令执行完毕！".into());
                }
            }

            "powershell7" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.PowerShell").await {
                    log("正在通过 WinGet 卸载 PowerShell 7...".into());
                    WindowsInstaller::uninstall_via_winget("Microsoft.PowerShell", log.clone()).await?;
                } else {
                    log("正在查找 PowerShell 7 MSI 注册项并执行静默卸载...".into());
                    let script = "Get-Package -Name 'PowerShell 7*' -ErrorAction SilentlyContinue | Uninstall-Package -Force";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                }
                log("PowerShell 7 卸载完成！".into());
            }

            "nodejs" => {
                log("正在通过 MSI/Package 接口执行 Node.js 静默卸载...".into());
                let script = "Get-Package -Name 'Node.js*' -ErrorAction SilentlyContinue | Uninstall-Package -Force";
                let mut cmd = tokio::process::Command::new("powershell.exe");
                log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);
                let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                log("Node.js 卸载指令已提交！".into());
            }

            "git" => {
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    log("通过 WinGet 静默卸载 Git...".into());
                    WindowsInstaller::uninstall_via_winget("Git.Git", log.clone()).await?;
                } else {
                    log("尝试定位 Git 默认安装目录下的 unins000.exe 静默卸载...".into());
                    let unins_paths = [
                        r"C:\Program Files\Git\unins000.exe",
                        r"C:\apps\Git\unins000.exe",
                    ];
                    let mut uninstalled = false;
                    for p in unins_paths {
                        let path = std::path::Path::new(p);
                        if path.exists() {
                            let mut cmd = tokio::process::Command::new(path);
                            log(format!("$ \"{}\" /VERYSILENT /NORESTART /SUPPRESSMSGBOXES", path.display()));
                            #[cfg(windows)]
                            cmd.creation_flags(0x08000000);
                            cmd.args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"]);
                            let _ = cmd.output().await;
                            uninstalled = true;
                            break;
                        }
                    }
                    if !uninstalled {
                        log("未找到 Git 本地 unins000.exe，尝试 winget 兜底移除...".into());
                        let _ = WindowsInstaller::uninstall_via_winget("Git.Git", |_: String| {}).await;
                    }
                }
                log("Git 卸载执行完毕！".into());
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::uninstall_agent(software_id, event_sender).await?;
                Self::cleanup_native_artifacts(software_id, event_sender).await;
            }

            other => {
                bail!("未知卸载目标: {}", other);
            }
        }

        Ok(())
    }

    // ==================== macOS ====================

    #[cfg(target_os = "macos")]
    async fn install_target_macos(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let proxy_ref = proxy_url.as_deref();
        let client = create_http_client(proxy_ref)?;

        let temp_dir = std::env::temp_dir().join("devup-cache");
        tokio::fs::create_dir_all(&temp_dir).await?;

        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始安装任务: {}", software_id));

        match software_id {
            "homebrew" => {
                if MacInstaller::brew_available() {
                    log("检测到 Homebrew 已安装，执行 brew update 自更新（已启用清华镜像加速）...".into());
                    MacInstaller::brew_run(&["update"], log.clone()).await?;
                    log("Homebrew 更新完成！".into());
                } else {
                    log("Homebrew 安装脚本需要交互输入登录密码，已为您唤起系统 Terminal...".into());
                    MacInstaller::open_in_terminal(macos::HOMEBREW_INSTALL_SCRIPT)
                        .await?;
                    log("请在弹出的终端窗口中按提示完成安装（已预置国内镜像加速），完成后点击「刷新状态」。".into());
                }
            }

            "nodejs" => {
                if MacInstaller::brew_available() {
                    log("通过 Homebrew 安装/升级 Node.js LTS（清华 bottle 加速）...".into());
                    MacInstaller::brew_install_or_upgrade("node", log.clone()).await?;
                    log("Node.js LTS 安装成功！".into());
                } else {
                    let arch = crate::platform::mac_arch_label();
                    let file_name = if arch == "arm64" {
                        "node-v20.18.0-arm64.pkg".to_string()
                    } else {
                        "node-v20.18.0.pkg".to_string()
                    };
                    let download_url = format!("{}/v20.18.0/{}", NODEJS_MIRROR_BASE, file_name);
                    log(format!("未检测到 Homebrew，正在从阿里 npmmirror 镜像下载官方 PKG: {}", download_url));

                    let dest = temp_dir.join(&file_name);
                    let tx_progress = event_sender.clone();
                    download_file(&client, &download_url, &dest, software_id, move |p| {
                        let _ = tx_progress.send(AppEvent::Progress(p));
                    })
                    .await?;

                    log("下载完成，正在弹出系统授权框以静默安装 Node.js LTS...".into());
                    MacInstaller::install_pkg(&dest, "DevUp 需要管理员权限来安装 Node.js LTS").await?;
                    log("Node.js LTS 安装成功！".into());
                }
            }

            "git" => {
                if MacInstaller::brew_available() {
                    log("通过 Homebrew 安装/升级 Git（清华 bottle 加速）...".into());
                    MacInstaller::brew_install_or_upgrade("git", log.clone()).await?;
                    log("Git 安装成功！".into());
                } else {
                    log("未检测到 Homebrew，正在唤起 Xcode 命令行工具安装对话框...".into());
                    let _ = tokio::process::Command::new("xcode-select")
                        .arg("--install")
                        .spawn();
                    log("请在系统弹窗中确认安装 Command Line Tools（内含系统 Git），完成后点击「刷新状态」。".into());
                }
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::install_agent(software_id, proxy_ref, &mirror_config.npm_registry, event_sender).await?;
            }

            other => {
                bail!("未知软件目标: {}", other);
            }
        }

        Ok(())
    }

    #[cfg(target_os = "macos")]
    async fn uninstall_target_macos(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始执行卸载任务: {}", software_id));

        match software_id {
            "homebrew" => {
                log("Homebrew 官方卸载脚本需要交互确认，已为您唤起系统 Terminal...".into());
                MacInstaller::open_in_terminal(macos::HOMEBREW_UNINSTALL_SCRIPT)
                    .await?;
                log("请在弹出的终端窗口中完成卸载流程。".into());
            }

            "nodejs" => {
                if MacInstaller::brew_installed_formula("node") {
                    log("正在通过 Homebrew 卸载 node formula...".into());
                    MacInstaller::brew_run(&["uninstall", "node"], log.clone()).await?;
                } else {
                    log("正在弹出系统授权框以清理官方 PKG 安装的 Node.js 文件...".into());
                    MacInstaller::remove_pkg_node_files("DevUp 需要管理员权限来卸载 Node.js").await?;
                }
                log("Node.js 卸载完成！".into());
            }

            "git" => {
                if MacInstaller::brew_installed_formula("git") {
                    log("正在通过 Homebrew 卸载 git formula...".into());
                    MacInstaller::brew_run(&["uninstall", "git"], log.clone()).await?;
                    log("Git 卸载完成！".into());
                } else {
                    log("当前 Git 由 Xcode 命令行工具 (/usr/bin/git) 提供，随系统管理，不支持单独卸载。".into());
                }
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::uninstall_agent(software_id, event_sender).await?;
                Self::cleanup_native_artifacts(software_id, event_sender).await;
            }

            other => {
                bail!("未知卸载目标: {}", other);
            }
        }

        Ok(())
    }
}
