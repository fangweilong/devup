use winreg::enums::*;
use winreg::RegKey;

/// 检测当前 Windows 系统是否启用了系统代理（如 Clash, v2rayN, Sing-box 等开启的系统代理）
/// 返回规范化的代理 URL，例如 "http://127.0.0.1:7890"
pub fn detect_system_proxy() -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings = hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;

    let proxy_enable: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if proxy_enable == 0 {
        return None;
    }

    let proxy_server: String = settings.get_value("ProxyServer").ok()?;
    if proxy_server.trim().is_empty() {
        return None;
    }

    // 格式可能为：
    // 1. "127.0.0.1:7890"
    // 2. "http=127.0.0.1:7890;https=127.0.0.1:7890"
    let target = if proxy_server.contains(';') {
        proxy_server
            .split(';')
            .find(|part| part.starts_with("http=") || part.starts_with("https="))
            .and_then(|part| part.split('=').nth(1))
            .unwrap_or_else(|| proxy_server.split(';').next().unwrap_or(""))
    } else {
        &proxy_server
    };

    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        Some(format!("http://{}", trimmed))
    }
}
