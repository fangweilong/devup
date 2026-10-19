use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::network::downloader::{create_http_client, download_file};
use crate::network::mirrors::find_fastest_github_url;
use crate::state::AppEvent;

pub const GITHUB_REPO: &str = "fangweilong/devup";

#[derive(Deserialize, Debug)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub html_url: String,
    pub body: Option<String>,
    pub assets: Vec<GitHubAsset>,
}

#[derive(Deserialize, Debug)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_notes: String,
    pub download_url: String,
    pub release_url: String,
}

pub struct SelfUpdater;

impl SelfUpdater {
    /// 语义化版本对比：remote > current 时返回 true
    pub fn is_newer(current: &str, remote: &str) -> bool {
        let parse_parts = |s: &str| -> Vec<u64> {
            s.trim_start_matches(|c| c == 'v' || c == 'V')
                .split('-')
                .next()
                .unwrap_or("")
                .split('.')
                .filter_map(|p| p.parse::<u64>().ok())
                .collect()
        };

        let mut c_parts = parse_parts(current);
        let mut r_parts = parse_parts(remote);

        while c_parts.len() < r_parts.len() {
            c_parts.push(0);
        }
        while r_parts.len() < c_parts.len() {
            r_parts.push(0);
        }

        r_parts > c_parts
    }

    /// 检查 GitHub Releases 是否有新版本
    pub async fn check_for_update(proxy_url: Option<String>) -> Result<Option<UpdateInfo>> {
        let client = create_http_client(proxy_url.as_deref())?;
        let current_version = env!("CARGO_PKG_VERSION").to_string();

        let api_url = format!("https://api.github.com/repos/{}/releases/latest", GITHUB_REPO);

        let resp = client
            .get(&api_url)
            .timeout(Duration::from_secs(8))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .with_context(|| format!("请求 GitHub Releases API 失败: {}", api_url))?;

        if !resp.status().is_success() {
            bail!("GitHub API 返回非成功状态码: {}", resp.status());
        }

        let release: GitHubRelease = resp
            .json()
            .await
            .context("解析 GitHub Release 数据失败")?;

        let remote_version = release
            .tag_name
            .trim_start_matches(|c| c == 'v' || c == 'V')
            .to_string();

        if Self::is_newer(&current_version, &remote_version) {
            // 按当前平台匹配 Release 资产
            #[cfg(windows)]
            let platform_asset = release
                .assets
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case("devup.exe"))
                .or_else(|| release.assets.iter().find(|a| a.name.ends_with(".exe")));

            #[cfg(target_os = "macos")]
            let platform_asset = {
                let expect = format!("devup-macos-{}", crate::platform::mac_arch_label());
                release
                    .assets
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(&expect))
                    .or_else(|| {
                        release.assets.iter().find(|a| {
                            let n = a.name.to_lowercase();
                            (n.contains("macos") || n.contains("darwin")) && !n.ends_with(".sha256")
                        })
                    })
            };

            #[cfg(not(any(windows, target_os = "macos")))]
            let platform_asset = Option::<&GitHubAsset>::None;

            if let Some(asset) = platform_asset {
                return Ok(Some(UpdateInfo {
                    current_version,
                    latest_version: remote_version,
                    release_notes: release.body.unwrap_or_else(|| "暂无更新说明。".to_string()),
                    download_url: asset.browser_download_url.clone(),
                    release_url: release.html_url,
                }));
            }
        }

        Ok(None)
    }

    /// 执行原地下载、重命名替换与自重启
    pub async fn execute_update(
        download_url: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let client = create_http_client(proxy_url.as_deref())?;
        let current_exe = std::env::current_exe().context("无法获取当前程序绝对路径")?;
        let exe_dir = current_exe.parent().context("无法获取程序安装所在目录")?;

        // 以当前可执行文件名为基准确定 .new/.old 路径（Windows: devup.exe.new；macOS: devup.new）
        let exe_name = current_exe
            .file_name()
            .context("无法获取当前可执行文件名")?
            .to_string_lossy()
            .to_string();
        let new_exe = exe_dir.join(format!("{}.new", exe_name));
        let old_exe = exe_dir.join(format!("{}.old", exe_name));

        let _ = event_sender.send(AppEvent::Log {
            target_id: "self_update".to_string(),
            message: "正在根据网络与镜像配置解析最优下载源...".to_string(),
        });

        // 利用配置的镜像规则或自动测速加速下载
        let target_url = crate::network::mirrors::resolve_github_download_url(
            &client,
            download_url,
            &mirror_config.github_mode,
            &mirror_config.github_mirror,
        ).await;
        let _ = event_sender.send(AppEvent::Log {
            target_id: "self_update".to_string(),
            message: format!("已选定下载源: {}", target_url),
        });
        let event_tx_clone = event_sender.clone();
        download_file(
            &client,
            &target_url,
            &new_exe,
            "self_update",
            move |progress| {
                let _ = event_tx_clone.send(AppEvent::Progress(progress));
            },
        )
        .await
        .context("下载新版本安装包失败")?;

        // 校验文件完整性（必须大于 1MB 且存在）
        let metadata = tokio::fs::metadata(&new_exe)
            .await
            .context("校验新版本文件失败")?;
        if metadata.len() < 1_000_000 {
            let _ = tokio::fs::remove_file(&new_exe).await;
            bail!(
                "下载文件异常，体积过小 ({} 字节)，已中断替换",
                metadata.len()
            );
        }

        // Unix 系需要补上可执行权限（Windows 以扩展名区分，无需处理）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&new_exe, std::fs::Permissions::from_mode(0o755))
                .await
                .context("设置新版本文件可执行权限失败")?;
        }

        let _ = event_sender.send(AppEvent::Log {
            target_id: "self_update".to_string(),
            message: "下载完成，正在进行原子重命名替换并准备重启...".to_string(),
        });

        // 1. 如果此前存在残留的 .old，尝试清理
        if old_exe.exists() {
            let _ = std::fs::remove_file(&old_exe);
        }

        // 2. 将运行中的当前进程镜像重命名为 .old（NTFS 与 APFS 均允许重命名正在执行的可执行文件）
        std::fs::rename(&current_exe, &old_exe)
            .context("重命名当前可执行文件失败 (请确认当前目录具备写权限)")?;

        // 3. 将新下载的 .new 替换为当前运行程序的真实路径
        if let Err(e) = std::fs::rename(&new_exe, &current_exe) {
            // 回滚恢复原 exe
            let _ = std::fs::rename(&old_exe, &current_exe);
            bail!("安装新版本文件失败: {:#}", e);
        }

        let _ = event_sender.send(AppEvent::Log {
            target_id: "self_update".to_string(),
            message: "替换成功！正在唤起新版本...".to_string(),
        });

        // 4. 唤起新版本进程
        #[allow(unused_mut)]
        let mut cmd = std::process::Command::new(&current_exe);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x00000008); // DETACHED_PROCESS
        }
        cmd.spawn().context("拉起新版本进程失败")?;

        // 5. 等待 200ms 让新进程接管，然后安全退出当前旧版本进程
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    }
}
