//! Git 全局提交身份（user.name / user.email）。
//!
//! 只读写 `--global`，不碰仓库级配置：DevUp 面向的是「这台机器还没配过 Git」的用户，
//! 全局身份是 `git commit` 能直接工作的最小前提。

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

pub fn read_identity() -> GitIdentity {
    read_identity_at(None)
}

pub fn save_identity(name: &str, email: &str) -> Result<GitIdentity> {
    save_identity_at(name, email, None)
}

fn read_identity_at(config_global: Option<&Path>) -> GitIdentity {
    GitIdentity {
        name: config_get(config_global, "user.name"),
        email: config_get(config_global, "user.email"),
    }
}

fn save_identity_at(name: &str, email: &str, config_global: Option<&Path>) -> Result<GitIdentity> {
    let name = name.trim();
    let email = email.trim();
    if name.is_empty() {
        bail!("请填写提交显示的名字（user.name）");
    }
    if email.is_empty() {
        bail!("请填写提交邮箱（user.email）");
    }

    config_set(config_global, "user.name", name)?;
    config_set(config_global, "user.email", email)?;
    Ok(GitIdentity {
        name: Some(name.to_string()),
        email: Some(email.to_string()),
    })
}

fn config_get(config_global: Option<&Path>, key: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["config", "--global", "--get", key]);
    if let Some(path) = config_global {
        cmd.env("GIT_CONFIG_GLOBAL", path);
    }
    crate::platform::hide_console(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn config_set(config_global: Option<&Path>, key: &str, value: &str) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["config", "--global", key, value]);
    if let Some(path) = config_global {
        cmd.env("GIT_CONFIG_GLOBAL", path);
    }
    crate::platform::hide_console(&mut cmd);
    let out = cmd.output().context("无法执行 git，请先安装 Git 后重试")?;
    if !out.status.success() {
        bail!(
            "写入 Git 配置失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip_into_isolated_global_config() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "devup-identity-{}-{}",
            std::process::id(),
            "test"
        ));
        let _ = std::fs::remove_file(&path);

        let saved = save_identity_at("DevUp Tester", "zs@example.com", Some(&path)).unwrap();
        assert_eq!(saved.name.as_deref(), Some("DevUp Tester"));
        assert_eq!(saved.email.as_deref(), Some("zs@example.com"));

        let read = read_identity_at(Some(&path));
        assert_eq!(read, saved);

        // 空白值必须被拒绝，且不能覆盖已写入的身份
        assert!(save_identity_at("  ", "zs@example.com", Some(&path)).is_err());
        assert_eq!(read_identity_at(Some(&path)), saved);

        let _ = std::fs::remove_file(&path);
    }
}