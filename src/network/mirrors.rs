use std::time::Duration;
use reqwest::Client;

pub const GITHUB_PROXY_PREFIXES: &[&str] = &[
    "https://ghproxy.net/",
    "https://mirror.ghproxy.com/",
    "https://ghps.cc/",
];

pub const NPM_MIRROR_REGISTRY: &str = "https://registry.npmmirror.com";
pub const NODEJS_MIRROR_BASE: &str = "https://npmmirror.com/mirrors/node";

/// 为 GitHub 下载链接注入加速前缀
pub fn get_accelerated_github_url(raw_url: &str, prefix: &str) -> String {
    format!("{}{}", prefix, raw_url)
}

/// 快速并发探测最快的 GitHub 加速源
pub async fn find_fastest_github_url(client: &Client, original_url: &str) -> String {
    let mut tasks = Vec::new();

    // 1. 测试内置加速节点
    for &prefix in GITHUB_PROXY_PREFIXES {
        let candidate_url = get_accelerated_github_url(original_url, prefix);
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
        // 如果加速节点都超时，回退到第一个加速源（防止直接直连死锁）
        get_accelerated_github_url(original_url, GITHUB_PROXY_PREFIXES[0])
    }
}
