pub mod msi;
pub mod npm;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::network::downloader::{create_http_client, download_file};
use crate::network::mirrors::find_fastest_github_url;
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
                let download_url = "https://npmmirror.com/mirrors/node/v20.18.0/node-v20.18.0-x64.msi";
                send_log(format!("正在从阿里 Node.js 镜像高速下载: {}", download_url));

                let dest = temp_dir.join("node-v20.18.0-x64.msi");
                let tx_progress = event_sender.clone();
                download_file(&client, download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("下载完成，正在后台静默安装 Node.js LTS...".into());
                WindowsInstaller::install_msi(&dest, &[]).await?;
                send_log("Node.js LTS 安装成功！".into());
            }

            "git" => {
                let raw_url = "https://github.com/git-for-windows/git/releases/download/v2.47.0.windows.2/Git-2.47.0.2-64-bit.exe";
                send_log("正在解析 Git for Windows 国内加速地址...".into());
                let download_url = find_fastest_github_url(&client, raw_url).await;

                let dest = temp_dir.join("Git-Installer.exe");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                send_log("正在后台静默安装 Git...".into());
                let out = tokio::process::Command::new(&dest)
                    .args(["/VERYSILENT", "/NORESTART", "/NOCANCEL", "/SP-", "/CLOSEAPPLICATIONS"])
                    .output()
                    .await?;
                if !out.status.success() {
                    bail!("Git 静默安装异常: {:?}", out.status.code());
                }
                send_log("Git 安装成功！".into());
            }

            "claudecode" => {
                send_log("使用国内官方 npmmirror 镜像高速安装 Claude Code...".into());
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
                send_log("从 npmmirror 镜像安装 Earendil Works Pi Agent...".into());
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
                send_log("安装 oh-my-pi (omp) 编码智能体...".into());
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
                send_log("安装 OpenCode 编码智能体...".into());
                let tx_log = event_sender.clone();
                let sid = software_id.to_string();
                NpmInstaller::install_global("opencode-ai", proxy_ref, move |line| {
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
}
