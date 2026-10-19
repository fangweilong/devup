use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use crate::network::mirrors::NPM_MIRROR_REGISTRY;

pub struct NpmInstaller;

impl NpmInstaller {
    /// 执行 npm 全局包安装，自动配置国内 npmmirror 镜像源，并透传系统代理环境变量
    pub async fn install_global<F>(
        package_name: &str,
        proxy_url: Option<&str>,
        mut on_log: F,
    ) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/c")
            .arg("npm")
            .arg("install")
            .arg("-g")
            .arg(package_name)
            .arg(format!("--registry={}", NPM_MIRROR_REGISTRY));

        if let Some(proxy) = proxy_url {
            if !proxy.trim().is_empty() {
                cmd.env("HTTP_PROXY", proxy);
                cmd.env("HTTPS_PROXY", proxy);
                cmd.arg(format!("--proxy={}", proxy));
            }
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 npm 安装进程失败，请确认系统已安装 Node.js/npm: {}", package_name))?;

        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                on_log(line);
            }
        }

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("npm 安装 '{}' 失败，返回码: {:?}", package_name, status.code());
        }
    }
}
