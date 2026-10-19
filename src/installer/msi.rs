use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub struct WindowsInstaller;

impl WindowsInstaller {
    /// 静默安装 MSI 安装包
    pub async fn install_msi(msi_path: &Path, extra_args: &[&str]) -> Result<()> {
        let mut cmd = Command::new("msiexec.exe");
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd.arg("/i")
            .arg(msi_path)
            .arg("/quiet")
            .arg("/norestart");

        for arg in extra_args {
            cmd.arg(arg);
        }

        let output = cmd
            .output()
            .await
            .with_context(|| format!("执行 msiexec 失败: {:?}", msi_path))?;

        let code = output.status.code().unwrap_or(-1);
        // 0: 成功, 3010: 成功但提示需要重启
        if code == 0 || code == 3010 {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("MSI 安装退出码异常: {} ({})", code, stderr);
        }
    }

    /// 静默安装 AppX / MSIXBundle (如 Windows Terminal)
    pub async fn install_appx(package_path: &Path) -> Result<()> {
        let script = format!(
            "Add-AppxPackage -Path '{}' -ErrorAction Stop",
            package_path.display()
        );

        let mut cmd = Command::new("powershell.exe");
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let output = cmd
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
            .await
            .with_context(|| "调用 powershell Add-AppxPackage 失败")?;

        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("安装 AppX 失败: {}", err);
        }
    }

    /// 检查指定包名是否在 winget 中已安装
    pub async fn is_installed_via_winget(package_id: &str) -> bool {
        let mut cmd = Command::new("winget.exe");
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd.args(["list", "--id", package_id, "--exact", "--accept-source-agreements"]);

        cmd.output()
            .await
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// 通过 winget 静默升级包
    pub async fn upgrade_via_winget<F>(package_id: &str, mut on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let mut cmd = Command::new("winget.exe");
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd.args([
            "upgrade",
            "--id",
            package_id,
            "--exact",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity",
        ]);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 winget upgrade 失败: {}", package_id))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mut tasks = Vec::new();

        if let Some(out) = stdout {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                let mut reader = BufReader::new(out).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        let _ = tx.send(line);
                    }
                }
            });
            tasks.push(tokio::spawn(async move {
                let mut lines = Vec::new();
                while let Some(line) = rx.recv().await {
                    lines.push(line);
                }
                lines
            }));
        }

        if let Some(err) = stderr {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                let mut reader = BufReader::new(err).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        let _ = tx.send(line);
                    }
                }
            });
            tasks.push(tokio::spawn(async move {
                let mut lines = Vec::new();
                while let Some(line) = rx.recv().await {
                    lines.push(line);
                }
                lines
            }));
        }

        let status = child.wait().await?;
        for task in tasks {
            if let Ok(lines) = task.await {
                for line in lines {
                    on_log(line);
                }
            }
        }

        if status.success() {
            Ok(())
        } else {
            bail!("winget upgrade 退出码异常: {:?}", status.code())
        }
    }

    /// 通过 winget 静默卸载包
    pub async fn uninstall_via_winget<F>(package_id: &str, mut on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let mut cmd = Command::new("winget.exe");
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd.args([
            "uninstall",
            "--id",
            package_id,
            "--exact",
            "--silent",
            "--accept-source-agreements",
            "--disable-interactivity",
        ]);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 winget uninstall 失败: {}", package_id))?;

        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    on_log(trimmed.to_string());
                }
            }
        }

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("winget 卸载 '{}' 退出码异常: {:?}", package_id, status.code())
        }
    }
}
