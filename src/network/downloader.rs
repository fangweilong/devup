use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Proxy};
use std::path::Path;
use std::time::Instant;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub task_id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percentage: f32,
    pub speed_mb_s: f64,
    pub finished: bool,
}

pub fn create_http_client(proxy_url: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) DevUp/0.1.0")
        .timeout(std::time::Duration::from_secs(600));

    if let Some(p) = proxy_url {
        if !p.trim().is_empty() {
            builder = builder.proxy(Proxy::all(p).context("无效的代理配置")?);
        }
    }

    builder.build().context("构建 HTTP 客户端失败")
}

pub async fn download_file<F>(
    client: &Client,
    url: &str,
    dest_path: &Path,
    task_id: &str,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(DownloadProgress) + Send + 'static,
{
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("发起下载请求失败: {}", url))?;

    let total_bytes = response.content_length().unwrap_or(0);
    let mut downloaded_bytes: u64 = 0;

    let mut file = File::create(dest_path)
        .await
        .with_context(|| format!("创建目标文件失败: {:?}", dest_path))?;

    let mut stream = response.bytes_stream();
    let start_time = Instant::now();
    let mut last_emit = Instant::now();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.context("下载数据块读取失败")?;
        file.write_all(&chunk)
            .await
            .context("写入磁盘文件失败")?;

        downloaded_bytes += chunk.len() as u64;

        // 每隔 150ms 推送一次进度，避免高频刷屏
        if last_emit.elapsed().as_millis() >= 150 || (total_bytes > 0 && downloaded_bytes >= total_bytes) {
            let elapsed_secs = start_time.elapsed().as_secs_f64().max(0.001);
            let speed_mb_s = (downloaded_bytes as f64 / (1024.0 * 1024.0)) / elapsed_secs;
            let percentage = if total_bytes > 0 {
                (downloaded_bytes as f32 / total_bytes as f32) * 100.0
            } else {
                0.0
            };

            on_progress(DownloadProgress {
                task_id: task_id.to_string(),
                downloaded_bytes,
                total_bytes,
                percentage,
                speed_mb_s,
                finished: false,
            });

            last_emit = Instant::now();
        }
    }

    file.flush().await?;

    on_progress(DownloadProgress {
        task_id: task_id.to_string(),
        downloaded_bytes,
        total_bytes: downloaded_bytes,
        percentage: 100.0,
        speed_mb_s: 0.0,
        finished: true,
    });

    Ok(())
}
