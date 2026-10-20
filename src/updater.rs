use anyhow::{bail, Context, Result};
use serde::Deserialize;
#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::network::downloader::{create_http_client, download_file};
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
            let platform_asset =
                select_macos_asset(&release.assets, crate::platform::mac_arch_label());

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

        // macOS 整包更新：下载的是 DevUp.app.tar.gz 时走 bundle 替换，
        // 兼容仍在用裸二进制的历史版本（那条路径下载下来的是裸二进制）。
        #[cfg(target_os = "macos")]
        if is_bundle_archive(download_url) {
            return Self::execute_bundle_update(
                &client,
                download_url,
                &mirror_config,
                event_sender,
            )
            .await;
        }

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

    /// 启动自检：清理上一次更新遗留的 .old / 半成品 .new（Windows 裸文件与 macOS bundle 目录都覆盖）
    pub fn cleanup_update_leftovers() {
        let Ok(current_exe) = std::env::current_exe() else {
            return;
        };
        let old_exe = current_exe.with_file_name(format!("{}.old", file_name(&current_exe)));
        let _ = std::fs::remove_file(&old_exe);

        #[cfg(target_os = "macos")]
        if let Some(bundle) = enclosing_bundle(&current_exe) {
            for suffix in ["old", "new"] {
                let leftover = bundle.with_file_name(format!("{}.{}", file_name(&bundle), suffix));
                let _ = std::fs::remove_dir_all(&leftover);
            }
            // 由裸二进制原地升级为 .app 时，旁边可能还留着当年的裸二进制
            if let Some(host) = bundle.parent() {
                for legacy in ["devup", "devup-macos-arm64", "devup-macos-x64"] {
                    let _ = std::fs::remove_file(host.join(format!("{legacy}.old")));
                }
            }
        }
    }

    /// macOS: 整包替换 DevUp.app —— 下载 tar.gz → 解包 → 校验签名 → 原子换目录 → 唤起新版本
    #[cfg(target_os = "macos")]
    async fn execute_bundle_update(
        client: &reqwest::Client,
        download_url: &str,
        mirror_config: &crate::config::MirrorConfig,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let current_exe = std::env::current_exe().context("无法获取当前程序绝对路径")?;
        let bundle = enclosing_bundle(&current_exe);
        // 安装位置：优先当前 bundle 所在目录；裸二进制用户则就地升级为同目录的 DevUp.app
        let host_dir = bundle
            .as_ref()
            .and_then(|b| b.parent())
            .or_else(|| current_exe.parent())
            .context("无法获取应用安装所在目录")?
            .to_path_buf();
        let app_path = bundle.clone().unwrap_or_else(|| host_dir.join("DevUp.app"));
        let app_name = file_name(&app_path);
        let staged_slot = app_path.with_file_name(format!("{app_name}.new"));
        let old_slot = app_path.with_file_name(format!("{app_name}.old"));

        let log = |message: String| {
            let _ = event_sender.send(AppEvent::Log {
                target_id: "self_update".to_string(),
                message,
            });
        };

        // 1. 归档下到临时目录，避免在安装目录留下中间文件
        let work_dir = std::env::temp_dir().join(format!("devup-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work_dir);
        std::fs::create_dir_all(&work_dir).context("创建临时更新目录失败")?;
        let ext = if download_url.to_lowercase().ends_with(".zip") {
            "zip"
        } else {
            "tar.gz"
        };
        let archive = work_dir.join(format!("DevUp.app.{ext}"));

        log("正在根据网络与镜像配置解析最优下载源...".to_string());
        let target_url = crate::network::mirrors::resolve_github_download_url(
            client,
            download_url,
            &mirror_config.github_mode,
            &mirror_config.github_mirror,
        )
        .await;
        log(format!("已选定下载源: {}", target_url));

        let tx_progress = event_sender.clone();
        download_file(
            client,
            &target_url,
            &archive,
            "self_update",
            move |progress| {
                let _ = tx_progress.send(AppEvent::Progress(progress));
            },
        )
        .await
        .context("下载新版本安装包失败")?;

        let metadata = tokio::fs::metadata(&archive)
            .await
            .context("校验新版本归档失败")?;
        if metadata.len() < 1_000_000 {
            let _ = std::fs::remove_dir_all(&work_dir);
            bail!(
                "下载文件异常，体积过小 ({} 字节)，已中断更新",
                metadata.len()
            );
        }

        // 2. 解包并确认关键文件齐全
        let extract_dir = work_dir.join("unpacked");
        std::fs::create_dir_all(&extract_dir).context("创建解包目录失败")?;
        let unpacked = match extract_archive(&archive, &extract_dir)
            .and_then(|_| find_extracted_app(&extract_dir).context("归档里没有找到 DevUp.app"))
        {
            Ok(app) => app,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&work_dir);
                return Err(e);
            }
        };
        let staged_exe = unpacked.join("Contents/MacOS/devup");
        if !staged_exe.is_file() || !unpacked.join("Contents/Info.plist").is_file() {
            let _ = std::fs::remove_dir_all(&work_dir);
            bail!("新版本应用包结构不完整，已中断更新");
        }
        if let Err(e) =
            std::fs::set_permissions(&staged_exe, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(e).context("设置新版本可执行权限失败");
        }
        // tar 已连同签名一起保留；只有在签名确实失效时才补一次 ad-hoc，
        // 以免把 Developer ID 签名降级掉（那会让 TCC 授权重新申请）。
        ensure_signed(&unpacked);

        // 3. 先整体挪到安装目录的 .new 槽位（同目录 => 同卷，后面的 rename 才是原子的）
        log("下载完成，正在安装新版本应用包...".to_string());
        let _ = std::fs::remove_dir_all(&staged_slot);
        let moved = move_into(&unpacked, &staged_slot);
        // staged 已搬走（或根本没动），临时目录可以放心清掉
        let _ = std::fs::remove_dir_all(&work_dir);
        moved.with_context(|| {
            format!(
                "写入 {} 失败：请把 DevUp.app 拖到 ~/Applications 后再更新，或重新下载 DMG 安装",
                host_dir.display()
            )
        })?;

        // 4. 原子换入：旧 bundle 改名让位（运行中的进程持有 inode，改名安全），再把 .new 顶上
        let _ = std::fs::remove_dir_all(&old_slot);
        if let Some(current_bundle) = &bundle {
            if let Err(e) = std::fs::rename(current_bundle, &old_slot) {
                let _ = std::fs::remove_dir_all(&staged_slot);
                return Err(e).context("换出旧版本失败（请确认对本程序所在目录有写权限）");
            }
        }
        if let Err(e) = std::fs::rename(&staged_slot, &app_path) {
            if bundle.is_some() {
                let _ = std::fs::rename(&old_slot, app_path);
            }
            let _ = std::fs::remove_dir_all(&staged_slot);
            return Err(e).context("安装新版本应用包失败");
        }
        // 裸二进制原地升级：让位给 .app，旧文件留给下次启动自检清理
        if bundle.is_none() {
            let _ = std::fs::rename(
                &current_exe,
                host_dir.join(format!("{}.old", file_name(&current_exe))),
            );
        }

        log("替换成功！正在唤起新版本...".to_string());
        launch_bundle(&app_path);
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 在 Release 资产里挑出 macOS 能用的更新包，优先级：
/// 1. 与当前架构匹配的整包 `DevUp_<ver>_<arch>.app.tar.gz`（图标/Info.plist 一起更新）
/// 2. 历史版本只发的裸二进制 `devup-macos-<arch>`
/// 3. 泛匹配 macos/darwin 命名的产物（排除 DMG、校验文件和其他架构整包）
///
/// DMG 是给人手动拖到 Applications 的安装镜像，tar 解不开，绝不能自升级用。
#[cfg(target_os = "macos")]
fn select_macos_asset<'a>(assets: &'a [GitHubAsset], arch: &str) -> Option<&'a GitHubAsset> {
    let bundle_suffix = format!("_{}.app.tar.gz", arch);
    let legacy = format!("devup-macos-{}", arch);
    assets
        .iter()
        .find(|a| a.name.to_lowercase().ends_with(&bundle_suffix))
        .or_else(|| assets.iter().find(|a| a.name.eq_ignore_ascii_case(&legacy)))
        .or_else(|| {
            assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                (n.contains("macos") || n.contains("darwin"))
                    && !n.ends_with(".sha256")
                    && !n.ends_with(".dmg")
                    && !(n.ends_with(".app.tar.gz") || n.ends_with(".app.zip"))
            })
        })
}

/// 判断 Release 资产是否为「整包 .app」归档（这类要走 bundle 替换）
#[cfg(target_os = "macos")]
fn is_bundle_archive(url: &str) -> bool {
    let n = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    n.ends_with(".app.tar.gz") || n.ends_with(".app.tgz") || n.ends_with(".app.zip")
}

/// 找到可执行文件所属的 .app bundle 根目录；裸二进制返回 None
#[cfg(target_os = "macos")]
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| {
            p.extension().map(|e| e == "app").unwrap_or(false)
                && p.join("Contents").join("Info.plist").is_file()
        })
        .map(PathBuf::from)
}

/// 用系统自带命令解包（tar / ditto），不引入额外依赖
#[cfg(target_os = "macos")]
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let status = if file_name(archive).to_lowercase().ends_with(".zip") {
        std::process::Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(archive)
            .arg(dest)
            .status()
    } else {
        std::process::Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .args(["-C"])
            .arg(dest)
            .status()
    }
    .context("调用系统解压命令失败")?;

    if !status.success() {
        bail!("解压新版本归档失败 (退出码 {:?})", status.code());
    }
    Ok(())
}

/// 解包结果里的 DevUp.app（兼容归档顶层目录名不一致的情况）
#[cfg(target_os = "macos")]
fn find_extracted_app(dir: &Path) -> Option<PathBuf> {
    let named = dir.join("DevUp.app");
    if named.is_dir() {
        return Some(named);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.path().is_dir() && e.path().extension().map(|x| x == "app").unwrap_or(false))
        .map(|e| e.path())
}

/// 签名失效时补一个 ad-hoc 签名：没有签名的 bundle 在 macOS 上会被判定「已损坏」
#[cfg(target_os = "macos")]
fn ensure_signed(app: &Path) {
    let verify_ok = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(app)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(true);
    if !verify_ok {
        let _ = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(app)
            .output();
    }
}

/// 目录搬家：同卷 rename，跨卷（临时目录与安装目录不同卷）退化为递归复制
#[cfg(target_os = "macos")]
fn move_into(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_dir_all(src, dst)?;
    let _ = std::fs::remove_dir_all(src);
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 通过 LaunchServices 唤起新 bundle（拿到正确的 Dock 图标与前台激活），失败则直接 exec
#[cfg(target_os = "macos")]
fn launch_bundle(app: &Path) {
    let opened = std::process::Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app)
        .spawn()
        .is_ok();
    if !opened {
        let _ = std::process::Command::new(app.join("Contents/MacOS/devup")).spawn();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{
        GitHubAsset, enclosing_bundle, is_bundle_archive, move_into, select_macos_asset,
    };

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            browser_download_url: format!(
                "https://github.com/fangweilong/devup/releases/download/v0.2.0/{name}"
            ),
        }
    }

    fn pick(names: &[&str], arch: &str) -> Option<String> {
        let assets: Vec<GitHubAsset> = names.iter().map(|n| asset(n)).collect();
        select_macos_asset(&assets, arch).map(|a| a.name.clone())
    }

    /// 资产命名由 packaging/macos/bundle.sh 决定，这两处必须同步
    #[test]
    fn prefers_bundle_for_matching_arch() {
        let names = [
            "devup.exe",
            "DevUp_0.2.0_x64.dmg",
            "DevUp_0.2.0_x64.app.tar.gz",
            "DevUp_0.2.0_arm64.dmg",
            "DevUp_0.2.0_arm64.app.tar.gz",
        ];
        assert_eq!(
            pick(&names, "arm64").as_deref(),
            Some("DevUp_0.2.0_arm64.app.tar.gz")
        );
        assert_eq!(
            pick(&names, "x64").as_deref(),
            Some("DevUp_0.2.0_x64.app.tar.gz")
        );
    }

    #[test]
    fn skips_dmg_and_other_arch_bundles() {
        let names = ["DevUp_0.2.0_x64.dmg", "DevUp_0.2.0_x64.app.tar.gz"];
        assert_eq!(pick(&names, "arm64"), None);
    }

    #[test]
    fn falls_back_to_legacy_bare_binary() {
        let names = ["devup.exe", "devup-macos-arm64", "devup-macos-x64"];
        assert_eq!(pick(&names, "arm64").as_deref(), Some("devup-macos-arm64"));

        // 命名不规范的兜底：仍要求排除 DMG 与校验文件
        let loose = ["devup-macos-arm64.sha256", "DevUp-macOS.dmg"];
        assert_eq!(pick(&loose, "arm64"), None);
    }

    #[test]
    fn bundle_archive_detection_only_matches_bundles() {
        assert!(is_bundle_archive("https://x/DevUp_0.2.0_arm64.app.tar.gz"));
        assert!(!is_bundle_archive("https://x/DevUp_0.2.0_arm64.dmg"));
        assert!(!is_bundle_archive("https://x/devup-macos-arm64"));
    }

    /// 整包替换依赖的两个原语：定位所属 bundle、把解好的 .app 搬进安装目录
    #[test]
    fn locates_bundle_and_moves_it() {
        let root = std::env::temp_dir().join(format!("devup-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let app = root.join("DevUp.app");
        let macos_dir = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos_dir).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), b"plist").unwrap();
        let exe = macos_dir.join("devup");
        std::fs::write(&exe, b"binary").unwrap();

        assert_eq!(enclosing_bundle(&exe).as_deref(), Some(app.as_path()));

        // 裸二进制（历史版本）没有 bundle，更新时就地在同目录建 .app
        let bare = root.join("devup-macos-arm64");
        std::fs::write(&bare, b"binary").unwrap();
        assert_eq!(enclosing_bundle(&bare), None);

        let staged = root.join("DevUp.app.new");
        move_into(&app, &staged).unwrap();
        assert!(staged.join("Contents/MacOS/devup").is_file());
        assert!(!app.exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
