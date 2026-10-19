use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use crate::network::mirrors::NPM_MIRROR_REGISTRY;

/// 构造跨平台的命令包装器：Windows 经由 cmd.exe /c（解析 npm 的 .cmd shim），
/// Unix/macOS 直接执行命令并注入扩展后的 PATH
#[cfg(windows)]
fn wrap_command(name: &str) -> Command {
    let mut cmd = Command::new("cmd.exe");
    cmd.creation_flags(0x08000000);
    cmd.arg("/c").arg(name);
    cmd
}

#[cfg(not(windows))]
fn wrap_command(name: &str) -> Command {
    let resolved = crate::platform::which(name)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let mut cmd = Command::new(resolved);
    cmd.env("PATH", crate::platform::child_path());
    cmd
}

/// 排空子进程 stdout/stderr 并逐行回调，防止管道写满导致死锁
async fn pump_output<F>(child: &mut Child, mut on_log: F)
where
    F: FnMut(String),
{
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
}

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
        let mut cmd = wrap_command("npm");
        cmd.arg("install")
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

        on_log(format!(
            "$ npm install -g {} --registry={}{}",
            package_name,
            NPM_MIRROR_REGISTRY,
            proxy_url
                .filter(|p| !p.trim().is_empty())
                .map(|p| format!(" --proxy={}", p))
                .unwrap_or_default()
        ));

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 npm 安装进程失败，请确认系统已安装 Node.js/npm: {}", package_name))?;

        pump_output(&mut child, on_log).await;

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("npm 安装 '{}' 失败，返回码: {:?}", package_name, status.code());
        }
    }

    /// 执行原生 CLI 更新命令（如 `claude update`, `omp update`, `opencode upgrade` 等）
    pub async fn run_cli_command<F>(
        cmd_name: &str,
        args: &[&str],
        proxy_url: Option<&str>,
        mut on_log: F,
    ) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let mut cmd = wrap_command(cmd_name);
        for a in args {
            cmd.arg(a);
        }

        if let Some(proxy) = proxy_url {
            if !proxy.trim().is_empty() {
                cmd.env("HTTP_PROXY", proxy);
                cmd.env("HTTPS_PROXY", proxy);
                cmd.env("ALL_PROXY", proxy);
            }
        }

        on_log(format!("$ {} {}", cmd_name, args.join(" ")));

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动命令失败: {} {:?}", cmd_name, args))?;

        pump_output(&mut child, on_log).await;

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("执行 '{}' 退出码异常: {:?}", cmd_name, status.code());
        }
    }

    /// 执行 npm 全局包卸载
    pub async fn uninstall_global<F>(package_name: &str, mut on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let mut cmd = wrap_command("npm");
        cmd.args(["uninstall", "-g", package_name]);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        on_log(format!("$ npm uninstall -g {}", package_name));


        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 npm 卸载进程失败: {}", package_name))?;

        pump_output(&mut child, on_log).await;

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("npm 卸载 '{}' 失败，返回码: {:?}", package_name, status.code());
        }
    }
}
