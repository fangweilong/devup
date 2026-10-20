use std::time::Duration;
use reqwest::Client;

/// 内置 GitHub 加速镜像前缀列表（仅自动探测使用；展示名由前端维护）
pub const BUILTIN_GITHUB_MIRRORS: &[&str] = &[
    "https://ghproxy.net/",
    "https://mirror.ghproxy.com/",
    "https://ghps.cc/",
    "https://gh.ddlc.top/",
];

pub const NPM_MIRROR_REGISTRY: &str = "https://registry.npmmirror.com";
pub const NODEJS_MIRROR_BASE: &str = "https://npmmirror.com/mirrors/node";

/// 为 GitHub 下载链接注入加速前缀
pub fn get_accelerated_github_url(raw_url: &str, prefix: &str) -> String {
    let clean_prefix = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };
    format!("{}{}", clean_prefix, raw_url)
}

/// 根据用户配置策略解析最终的 GitHub 下载 URL
pub async fn resolve_github_download_url(
    client: &Client,
    original_url: &str,
    github_mode: &str,
    github_mirror: &str,
) -> String {
    match github_mode {
        "direct" => original_url.to_string(),
        "selected" => {
            let mirror = github_mirror.trim();
            if mirror.is_empty() {
                original_url.to_string()
            } else {
                get_accelerated_github_url(original_url, mirror)
            }
        }
        _ => {
            // 默认 "auto"：并发探测最快可用内置镜像
            find_fastest_github_url(client, original_url).await
        }
    }
}

/// 快速并发探测最快的 GitHub 加速源
pub async fn find_fastest_github_url(client: &Client, original_url: &str) -> String {
    let mut tasks = Vec::new();

    // 测试内置加速节点
    for mirror_url in BUILTIN_GITHUB_MIRRORS {
        let candidate_url = get_accelerated_github_url(original_url, mirror_url);
        let client_clone = client.clone();
        tasks.push(tokio::spawn(async move {
            let start = std::time::Instant::now();
            let res = client_clone
                .head(&candidate_url)
                .timeout(Duration::from_millis(1800))
                .send()
                .await;

            match res {
                Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                    Some((candidate_url, start.elapsed()))
                }
                _ => None,
            }
        }));
    }

    let mut fastest: Option<(String, Duration)> = None;
    for t in tasks {
        if let Ok(Some((url, elapsed))) = t.await {
            match &fastest {
                None => fastest = Some((url, elapsed)),
                Some((_, best_elapsed)) if elapsed < *best_elapsed => {
                    fastest = Some((url, elapsed));
                }
                _ => {}
            }
        }
    }

    if let Some((best_url, _)) = fastest {
        best_url
    } else {
        // 如果加速节点都超时，回退到第一个内置加速源（防止直接直连死锁）
        get_accelerated_github_url(original_url, BUILTIN_GITHUB_MIRRORS[0])
    }
}
