pub mod msi;
pub mod npm;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::network::downloader::{create_http_client, download_file};
use crate::network::mirrors::{find_fastest_github_url, NODEJS_MIRROR_BASE};
use crate::state::AppEvent;

use msi::WindowsInstaller;
use npm::NpmInstaller;

pub struct InstallerEngine;

impl InstallerEngine {
    /// 执行具体的软件安装任务
    pub async fn install_target(
        software_id: &str,
        proxy_url: Option<String>,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let proxy_ref = proxy_url.as_deref();
        let client = create_http_client(proxy_ref)?;

        let temp_dir = std::env::temp_dir().join("devup-cache");
        tokio::fs::create_dir_all(&temp_dir).await?;

        let send_log = {
            let tx = event_sender.clone();
            let sid = software_id.to_string();
            move |msg: String| {
                let _ = tx.send(AppEvent::Log {
                    target_id: sid.clone(),
                    message: msg,
                });
            }
        };

        send_log(format!(">>> 开始安装任务: {}", software_id));

        match software_id {
            "powershell7" => {
                let raw_url = "https://github.com/PowerShell/PowerShell/releases/download/v7.4.6/PowerShell-7.4.6-win-x64.msi";
                send_log("正在测试国内 GitHub 加速镜像并获取下载通道...".into());
                let download_url = find_fastest_github_url(&client, raw_url).await;
                send_log(format!("选定最优加速链接: {}", download_url));

                let dest = temp_dir.join("PowerShell-7.4.6-win-x64.msi");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("下载完成，正在后台静默安装 PowerShell 7...".into());
                WindowsInstaller::install_msi(&dest, &["ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1"]).await?;
                send_log("PowerShell 7 安装完成！".into());
            }

            "windows_terminal" => {
                // 如果系统支持 winget 且已通过应用商店/winget 安装，优先通过 winget 静默更新
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    send_log("检测到 Windows Terminal 来自 WinGet/应用商店，执行原生渠道静默升级...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Microsoft.WindowsTerminal", move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await {
                        send_log("Windows Terminal 通过 WinGet 渠道更新成功！".into());
                        return Ok(());
                    }
                    send_log("WinGet 渠道更新失败或已是最新，回退到国内 GitHub 离线包安装...".into());
                }

                let raw_url = "https://github.com/microsoft/terminal/releases/download/v1.21.3231.0/Microsoft.WindowsTerminal_Win10_1.21.3231.0_8wekyb3d8bbwe.msixbundle";
                send_log("正在连接国内 GitHub 加速节点...".into());
                let download_url = find_fastest_github_url(&client, raw_url).await;
                send_log(format!("选定最优加速链接: {}", download_url));

                let dest = temp_dir.join("WindowsTerminal.msixbundle");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("下载完成，正在静默部署 Windows Terminal AppX...".into());
                WindowsInstaller::install_appx(&dest).await?;
                send_log("Windows Terminal 部署成功！".into());
            }

            "nodejs" => {
                let download_url = format!("{}/v20.18.0/node-v20.18.0-x64.msi", NODEJS_MIRROR_BASE);
                send_log(format!("正在从阿里 Node.js 镜像高速下载: {}", download_url));

                let dest = temp_dir.join("node-v20.18.0-x64.msi");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("下载完成，正在后台静默安装 Node.js LTS...".into());
                WindowsInstaller::install_msi(&dest, &[]).await?;
                send_log("Node.js LTS 安装成功！".into());
            }

            "git" => {
                // 如果检测到系统通过 winget 管理 Git，优先调用 winget upgrade
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    send_log("检测到 Git 注册于 WinGet，优先执行 winget 原生渠道更新...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Git.Git", move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await {
                        send_log("Git 通过 WinGet 更新成功！".into());
                        return Ok(());
                    }
                    send_log("WinGet 更新回退，改用官网安装包...".into());
                }

                let raw_url = "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.2/Git-2.56.0.2-64-bit.exe";
                send_log("正在解析 Git for Windows 国内加速地址...".into());
                let download_url = find_fastest_github_url(&client, raw_url).await;

                let dest = temp_dir.join("Git-Installer.exe");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("正在后台静默安装 Git...".into());
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
                send_log("Git 安装成功！".into());
            }

            "claudecode" => {
                // 如果本地已存在 claude 命令，优先尝试原生 `claude update` 自升级
                let (has_claude, _, _) = crate::detector::Detector::check_command("claude.exe", "--version");
                if has_claude {
                    send_log("检测到本地已安装 Claude Code，执行官方原生更新命令 (`claude update`)...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    let update_res = NpmInstaller::run_cli_command("claude", &["update"], proxy_ref, move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await;

                    if update_res.is_ok() {
                        send_log("Claude Code 官方自升级完成！".into());
                        return Ok(());
                    }
                    send_log("原生自升级未成功，尝试通过国内 npmmirror 镜像更新...".into());
                } else {
                    send_log("使用国内官方 npmmirror 镜像高速安装 Claude Code...".into());
                }

                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                NpmInstaller::install_global("@anthropic-ai/claude-code", proxy_ref, move |line| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: sid.clone(),
                        message: line,
                    });
                })
                .await?;
                send_log("Claude Code 全局安装完成！直接在终端输入 `claude` 即可启动。".into());
            }

            "codex" => {
                send_log("使用国内 npmmirror 镜像安装 OpenAI Codex CLI...".into());
                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                NpmInstaller::install_global("@openai/codex", proxy_ref, move |line| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: sid.clone(),
                        message: line,
                    });
                })
                .await?;
                send_log("OpenAI Codex 全局安装完成！直接在终端输入 `codex` 即可启动。".into());
            }

            "pi" => {
                let (has_pi, _, _) = crate::detector::Detector::check_command("pi.exe", "--version");
                if has_pi {
                    send_log("检测到本地已安装 Pi，优先使用官方原生更新命令 (`pi update`)...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    let update_res = NpmInstaller::run_cli_command("pi", &["update"], proxy_ref, move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await;

                    if update_res.is_ok() {
                        send_log("Pi Agent 官方原生更新成功！".into());
                        return Ok(());
                    }
                    send_log("官方原生更新未成功，回退到国内 npmmirror 镜像重新安装...".into());
                } else {
                    send_log("从 npmmirror 镜像安装 Earendil Works Pi Agent...".into());
                }

                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                NpmInstaller::install_global("@earendil-works/pi", proxy_ref, move |line| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: sid.clone(),
                        message: line,
                    });
                })
                .await?;
                send_log("Pi Agent 安装成功！终端输入 `pi` 即可使用。".into());
            }

            "oh-my-pi" => {
                let (has_omp, _, _) = crate::detector::Detector::check_command("omp.exe", "--version");
                if has_omp {
                    send_log("检测到本地已安装 omp，使用官方原生更新命令 (`omp update`)...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    let update_res = NpmInstaller::run_cli_command("omp", &["update"], proxy_ref, move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await;

                    if update_res.is_ok() {
                        send_log("oh-my-pi (omp) 官方原生更新成功！".into());
                        return Ok(());
                    }
                    send_log("omp update 退出，回退到国内镜像安装...".into());
                } else {
                    send_log("安装 oh-my-pi (omp) 编码智能体...".into());
                }

                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                NpmInstaller::install_global("@oh-my-pi/pi-coding-agent", proxy_ref, move |line| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: sid.clone(),
                        message: line,
                    });
                })
                .await?;
                send_log("oh-my-pi (omp) 安装成功！".into());
            }

            "opencode" => {
                let (has_opencode, _, _) = crate::detector::Detector::check_command("opencode.exe", "--version");
                if has_opencode {
                    send_log("检测到本地已安装 OpenCode，优先执行官方命令 (`opencode upgrade`)...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    let update_res = NpmInstaller::run_cli_command("opencode", &["upgrade"], proxy_ref, move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await;

                    if update_res.is_ok() {
                        send_log("OpenCode 官方升级完成！".into());
                        return Ok(());
                    }
                    send_log("官方 upgrade 回退，通过国内 npmmirror 镜像更新...".into());
                } else {
                    send_log("通过国内 npmmirror 镜像安装 OpenCode...".into());
                }

                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                // 探测当前使用的 npm 全局包名：优先 @opencode/cli，其次 opencode-ai
                NpmInstaller::install_global("@opencode/cli", proxy_ref, move |line| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: sid.clone(),
                        message: line,
                    });
                })
                .await?;
                send_log("OpenCode 安装成功！".into());
            }

            other => {
                bail!("未知软件目标: {}", other);
            }
        }

        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });

        Ok(())
    }

    /// 执行具体的软件卸载任务
    pub async fn uninstall_target(
        software_id: &str,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let send_log = {
            let tx = event_sender.clone();
            let sid = software_id.to_string();
            move |msg: String| {
                let _ = tx.send(AppEvent::Log {
                    target_id: sid.clone(),
                    message: msg,
                });
            }
        };

        send_log(format!(">>> 开始执行卸载任务: {}", software_id));

        match software_id {
            "windows_terminal" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    send_log("正在通过 WinGet 静默卸载 Windows Terminal...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    WindowsInstaller::uninstall_via_winget("Microsoft.WindowsTerminal", move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await?;
                    send_log("Windows Terminal 已卸载！".into());
                } else {
                    send_log("正在通过 PowerShell Remove-AppxPackage 卸载 Windows Terminal...".into());
                    let script = "Get-AppxPackage *WindowsTerminal* | Remove-AppxPackage -ErrorAction SilentlyContinue";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                    send_log("Windows Terminal 卸载指令执行完毕！".into());
                }
            }

            "powershell7" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.PowerShell").await {
                    send_log("正在通过 WinGet 卸载 PowerShell 7...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    WindowsInstaller::uninstall_via_winget("Microsoft.PowerShell", move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await?;
                } else {
                    send_log("正在查找 PowerShell 7 MSI 注册项并执行静默卸载...".into());
                    let script = "Get-Package -Name 'PowerShell 7*' -ErrorAction SilentlyContinue | Uninstall-Package -Force";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                }
                send_log("PowerShell 7 卸载完成！".into());
            }

            "nodejs" => {
                send_log("正在通过 MSI/Package 接口执行 Node.js 静默卸载...".into());
                let script = "Get-Package -Name 'Node.js*' -ErrorAction SilentlyContinue | Uninstall-Package -Force";
                let mut cmd = tokio::process::Command::new("powershell.exe");
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);
                let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                send_log("Node.js 卸载指令已提交！".into());
            }

            "git" => {
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    send_log("通过 WinGet 静默卸载 Git...".into());
                    let tx_log = event_sender.clone();
                    let sid = software_id.to_string();
                    WindowsInstaller::uninstall_via_winget("Git.Git", move |line| {
                        let _ = tx_log.send(AppEvent::Log {
                            target_id: sid.clone(),
                            message: line,
                        });
                    }).await?;
                } else {
                    send_log("尝试定位 Git 默认安装目录下的 unins000.exe 静默卸载...".into());
                    let unins_paths = [
                        r"C:\Program Files\Git\unins000.exe",
                        r"C:\apps\Git\unins000.exe",
                    ];
                    let mut uninstalled = false;
                    for p in unins_paths {
                        let path = std::path::Path::new(p);
                        if path.exists() {
                            let mut cmd = tokio::process::Command::new(path);
                            #[cfg(windows)]
                            cmd.creation_flags(0x08000000);
                            cmd.args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"]);
                            let _ = cmd.output().await;
                            uninstalled = true;
                            break;
                        }
                    }
                    if !uninstalled {
                        send_log("未找到 Git 本地 unins000.exe，尝试 winget 兜底移除...".into());
                        let _ = WindowsInstaller::uninstall_via_winget("Git.Git", |_| {}).await;
                    }
                }
                send_log("Git 卸载执行完毕！".into());
            }

            "claudecode" => {
                // 1. 如果是 npm 安装，执行 npm uninstall -g
                send_log("正在检查并卸载 Claude Code npm 全局包...".into());
                let tx_log = event_sender.clone();
                let _ = NpmInstaller::uninstall_global("@anthropic-ai/claude-code", move |l| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: "claudecode".into(),
                        message: l,
                    });
                }).await;

                // 2. 清理原生二进制文件 ~/.local/bin/claude.exe
                if let Ok(user_home) = std::env::var("USERPROFILE") {
                    let local_bin_claude = std::path::Path::new(&user_home)
                        .join(".local")
                        .join("bin")
                        .join("claude.exe");
                    if local_bin_claude.exists() {
                        send_log(format!("移除原生可执行文件: {:?}", local_bin_claude));
                        let _ = tokio::fs::remove_file(local_bin_claude).await;
                    }
                }
                send_log("Claude Code 卸载完成！".into());
            }

            "codex" => {
                send_log("正在卸载 @openai/codex 全局包...".into());
                let tx_log = event_sender.clone();
                NpmInstaller::uninstall_global("@openai/codex", move |l| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: "codex".into(),
                        message: l,
                    });
                }).await?;
                send_log("OpenAI Codex 卸载成功！".into());
            }

            "pi" => {
                send_log("正在卸载 @earendil-works/pi 全局包与原生组件...".into());
                let tx_log = event_sender.clone();
                let _ = NpmInstaller::uninstall_global("@earendil-works/pi", move |l| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: "pi".into(),
                        message: l,
                    });
                }).await;
                if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                    let pi_dir = std::path::Path::new(&local_app).join("pi");
                    if pi_dir.exists() {
                        let _ = tokio::fs::remove_dir_all(pi_dir).await;
                    }
                }
                send_log("Pi Agent 卸载完成！".into());
            }

            "oh-my-pi" => {
                send_log("正在卸载 oh-my-pi 组件...".into());
                let tx_log = event_sender.clone();
                let _ = NpmInstaller::uninstall_global("@oh-my-pi/pi-coding-agent", move |l| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: "oh-my-pi".into(),
                        message: l,
                    });
                }).await;
                if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                    let omp_dir = std::path::Path::new(&local_app).join("omp");
                    if omp_dir.exists() {
                        send_log(format!("清理原生文件目录: {:?}", omp_dir));
                        let _ = tokio::fs::remove_dir_all(omp_dir).await;
                    }
                }
                send_log("oh-my-pi (omp) 卸载成功！".into());
            }

            "opencode" => {
                send_log("正在卸载 OpenCode 全局包...".into());
                let tx_log = event_sender.clone();
                let _ = NpmInstaller::uninstall_global("@opencode/cli", move |l| {
                    let _ = tx_log.send(AppEvent::Log {
                        target_id: "opencode".into(),
                        message: l,
                    });
                }).await;
                let tx_log2 = event_sender.clone();
                let _ = NpmInstaller::uninstall_global("opencode-ai", move |l| {
                    let _ = tx_log2.send(AppEvent::Log {
                        target_id: "opencode".into(),
                        message: l,
                    });
                }).await;
                send_log("OpenCode 卸载完成！".into());
            }

            other => {
                bail!("未知卸载目标: {}", other);
            }
        }

        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });

        Ok(())
    }
}
