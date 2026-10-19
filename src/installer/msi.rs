use anyhow::{bail, Context, Result};
use std::path::Path;
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
}
