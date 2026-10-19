use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::network::proxy::{detect_system_proxy, normalize_proxy_url};

/// 代理地址来源
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// 自动嗅探系统代理 / 环境变量（默认）
    System,
    /// 使用配置的固定地址（proxy.url）
    Custom,
    /// 显式禁用任何代理
    Direct,
}

impl Default for ProxyMode {
    fn default() -> Self {
        Self::System
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub mode: ProxyMode,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MirrorConfig {
    /// GitHub 镜像策略: "auto" (自动测速) | "selected" (指定镜像) | "direct" (直连不走镜像)
    pub github_mode: String,
    /// 选定的 GitHub 加速镜像前缀，如 "https://ghproxy.net/"
    pub github_mirror: String,
    /// npm 镜像源地址，如 "https://registry.npmmirror.com"
    pub npm_registry: String,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            github_mode: "auto".to_string(),
            github_mirror: "https://ghproxy.net/".to_string(),
            npm_registry: "https://registry.npmmirror.com".to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub mirror: MirrorConfig,
}

impl AppConfig {
    /// 配置文件路径：各平台标准配置目录下的 devup/config.json
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    /// 加载配置；文件缺失视为首次运行，解析失败回退默认值，永不阻断启动
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!(
                    "配置文件无效({})，已回退默认配置: {e}",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// 原子落盘：先写临时文件再 rename，避免写一半损坏配置
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        std::fs::write(&tmp, &text)
            .with_context(|| format!("写入配置失败: {}", tmp.display()))?;
        // Windows 上 rename 遇目标已存在会失败（Unix 则直接替换），先删除旧文件保证跨平台一致
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // 首次保存文件本就不存在：属正常，继续走 rename
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // 旧文件无法删除（如被安全软件短暂锁定）：退化为直接覆写
                std::fs::write(&path, text)
                    .with_context(|| format!("更新配置文件失败: {}", path.display()))?;
                return Ok(());
            }
        }
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("更新配置文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 根据配置计算实际生效的代理地址
    pub fn effective_proxy(&self) -> Option<String> {
        match self.proxy.mode {
            ProxyMode::Custom => self
                .proxy
                .url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(normalize_proxy_url),
            ProxyMode::Direct => None,
            ProxyMode::System => detect_system_proxy(),
        }
    }
}

/// 各平台用户配置目录（不引额外 crate，使用业界标准位置）
fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
            .join("devup")
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("devup")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"))
            .join("devup")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_fields_default_to_system() {
        // 老文件/手工编辑缺字段：整体回退默认（mode=system）
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.proxy.mode, ProxyMode::System);
        assert_eq!(cfg.proxy.url, None);
        assert_eq!(cfg.mirror.github_mode, "auto");
        assert_eq!(cfg.mirror.npm_registry, "https://registry.npmmirror.com");

        let partial: AppConfig =
            serde_json::from_str(r#"{"proxy":{"url":"127.0.0.1:7890"}}"#).unwrap();
        assert_eq!(partial.proxy.mode, ProxyMode::System);
        assert_eq!(partial.mirror.github_mode, "auto");
    }
    #[test]
    fn test_custom_mode_serde() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{"proxy":{"mode":"custom","url":"127.0.0.1:7890"}}"#).unwrap();
        assert_eq!(cfg.proxy.mode, ProxyMode::Custom);
        assert_eq!(cfg.proxy.url.as_deref(), Some("127.0.0.1:7890"));
    }

    #[test]
    fn test_effective_proxy_custom_and_direct() {
        let mut cfg = AppConfig::default();

        cfg.proxy.mode = ProxyMode::Direct;
        assert_eq!(cfg.effective_proxy(), None);

        cfg.proxy.mode = ProxyMode::Custom;
        cfg.proxy.url = Some(" 127.0.0.1:7890 ".into());
        assert_eq!(cfg.effective_proxy().as_deref(), Some("http://127.0.0.1:7890"));

        // 自定义模式但地址为空：视为未生效
        cfg.proxy.url = Some("   ".into());
        assert_eq!(cfg.effective_proxy(), None);
    }
}
