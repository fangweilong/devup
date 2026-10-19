/// 规范化代理地址为完整 URL：无协议前缀时补 `http://`，
/// 已有 http/https/socks5 前缀则原样保留（reqwest 的 `Proxy::all` 均支持）
pub fn normalize_proxy_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if ["http://", "https://", "socks5://", "socks5h://"]
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    }
}

/// 解析 Windows 注册表 `ProxyServer` 字符串值
/// 格式可能为：
/// 1. "127.0.0.1:7890"
/// 2. "http=127.0.0.1:7890;https=127.0.0.1:7890"
/// 返回规范化的代理 URL，例如 "http://127.0.0.1:7890"
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_proxy_server(server: &str) -> Option<String> {
    let target = if server.contains(';') {
        server
            .split(';')
            .find(|part| part.starts_with("http=") || part.starts_with("https="))
            .and_then(|part| part.split('=').nth(1))
            .unwrap_or_else(|| server.split(';').next().unwrap_or(""))
    } else {
        server
    };

    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(normalize_proxy_url(trimmed))
}

/// 检测当前 Windows 系统是否启用了系统代理（如 Clash, v2rayN, Sing-box 等开启的系统代理）
/// 返回规范化的代理 URL，例如 "http://127.0.0.1:7890"
#[cfg(windows)]
pub fn detect_system_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings = hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;

    let proxy_enable: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if proxy_enable == 0 {
        return None;
    }

    let proxy_server: String = settings.get_value("ProxyServer").ok()?;
    parse_windows_proxy_server(&proxy_server)
}

/// 解析 macOS `scutil --proxy` 字典输出：优先 HTTPS 代理，其次 HTTP 代理。
/// SOCKS 未启用 reqwest socks feature 暂不透传。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_scutil_proxy(text: &str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = text
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();

    let enabled = |flag: &str| map.get(flag).map(|v| v == "1").unwrap_or(false);
    let compose = |host_key: &str, port_key: &str| -> Option<String> {
        let host = map.get(host_key)?.trim();
        let port = map.get(port_key)?.trim();
        if host.is_empty() || port.is_empty() {
            return None;
        }
        Some(format!("http://{}:{}", host, port))
    };

    if enabled("HTTPSEnable") {
        if let Some(url) = compose("HTTPSProxy", "HTTPSPort") {
            return Some(url);
        }
    }
    if enabled("HTTPEnable") {
        if let Some(url) = compose("HTTPProxy", "HTTPPort") {
            return Some(url);
        }
    }
    None
}

/// macOS：执行 `scutil --proxy` 读取系统设置中的代理；无系统代理时回退环境变量
#[cfg(target_os = "macos")]
pub fn detect_system_proxy() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    if !output.status.success() {
        return env_proxy_fallback();
    }

    parse_scutil_proxy(&String::from_utf8_lossy(&output.stdout)).or_else(env_proxy_fallback)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn detect_system_proxy() -> Option<String> {
    env_proxy_fallback()
}

/// 环境变量兜底：部分代理工具/终端会话仅导出 HTTP(S)_PROXY
#[cfg(not(windows))]
fn env_proxy_fallback() -> Option<String> {
    for key in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(normalize_proxy_url(v));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_proxy_url() {
        assert_eq!(normalize_proxy_url("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(normalize_proxy_url(" https://a.b:8443 "), "https://a.b:8443");
        assert_eq!(
            normalize_proxy_url("socks5://user:pass@host:1080"),
            "socks5://user:pass@host:1080"
        );
    }

    #[test]
    fn test_parse_windows_proxy_server() {
        // 裸 host:port（Clash 系统代理最常见形态）
        assert_eq!(
            parse_windows_proxy_server("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // 分号协议映射格式
        assert_eq!(
            parse_windows_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // 混合协议段：优先取 http=/https= 段
        assert_eq!(
            parse_windows_proxy_server("socks=127.0.0.1:7891;http=127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // 已带 scheme 保持原样
        assert_eq!(
            parse_windows_proxy_server("http://proxy.example.com:8080").as_deref(),
            Some("http://proxy.example.com:8080")
        );
        // 空值
        assert_eq!(parse_windows_proxy_server("   ").as_deref(), None);
    }

    #[test]
    fn test_parse_scutil_proxy() {
        // 取自真实 macOS `scutil --proxy` 输出（Clash 全开模式）
        let full = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : 127.0.0.1
    1 : <local>
  }
  HTTPEnable : 1
  HTTPPort : 7890
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7890
  HTTPSProxy : 127.0.0.1
  ProxyAutoConfigEnable : 0
  SOCKSEnable : 1
  SOCKSPort : 7890
  SOCKSProxy : 127.0.0.1
}"#;
        assert_eq!(parse_scutil_proxy(full).as_deref(), Some("http://127.0.0.1:7890"));

        // 仅 HTTP 开启
        let http_only = "HTTPEnable : 1\nHTTPPort : 8118\nHTTPProxy : 127.0.0.1\n";
        assert_eq!(parse_scutil_proxy(http_only).as_deref(), Some("http://127.0.0.1:8118"));

        // 全部关闭（ExceptionsList 里的 127.0.0.1 行不应被误判为代理）
        let off = "ExceptionsList : <array> {\n    0 : 127.0.0.1\n}\nHTTPEnable : 0\n";
        assert_eq!(parse_scutil_proxy(off).as_deref(), None);

        // 开启但缺端口
        let no_port = "HTTPSEnable : 1\nHTTPSProxy : 127.0.0.1\n";
        assert_eq!(parse_scutil_proxy(no_port).as_deref(), None);
    }
}
