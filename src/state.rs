use serde::{Deserialize, Serialize};
use crate::config::ProxyMode;
use crate::detector::SoftwareStatus;
use crate::network::downloader::DownloadProgress;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload")]
pub enum IpcCommand {
    Refresh,
    Install { target_id: String },
    Uninstall { target_id: String },
    /// 设置代理模式与自定义地址，并持久化到本地配置文件
    SetProxy {
        mode: ProxyMode,
        url: Option<String>,
    },
    /// 设置网络与镜像源配置（代理 + GitHub 镜像 + npm 源）
    SetNetworkConfig {
        proxy_mode: ProxyMode,
        proxy_url: Option<String>,
        github_mode: String,
        github_mirror: String,
        npm_registry: String,
    },
    OpenUrl { url: String },
    CheckUpdate,
    PerformSelfUpdate { download_url: String },
    /// macOS 头部空白区按下时，请求原生整窗拖拽（替代 movableByWindowBackground 全局劫持）
    DragWindow,
}

/// 仅由后端序列化推送到 WebView（前端 window.onEvent 消费），无反向解析需求
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum AppEvent {
    StatusList(Vec<SoftwareStatus>),
    /// 内置 GitHub / npm 加速镜像清单（前端选项面板由后端数据源动态渲染）
    MirrorOptions {
        github: Vec<crate::network::mirrors::BuiltinMirror>,
        npm: Vec<crate::network::mirrors::BuiltinMirror>,
    },
    /// 当前网络、代理配置与镜像信息
    ProxyStatus {
        proxy: Option<String>,
        mode: ProxyMode,
        url: Option<String>,
        github_mode: String,
        github_mirror: String,
        npm_registry: String,
    },
    Log { target_id: String, message: String },
    Progress(DownloadProgress),
    InstallSuccess { target_id: String },
    InstallError { target_id: String, error: String },
    UpdateAvailable {
        current_version: String,
        latest_version: String,
        release_notes: String,
        download_url: String,
        release_url: String,
    },
    UpdateNotAvailable {
        current_version: String,
    },
    UpdateError {
        error: String,
    },
}

impl Serialize for DownloadProgress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("DownloadProgress", 6)?;
        s.serialize_field("taskId", &self.task_id)?;
        s.serialize_field("downloaded", &self.downloaded_bytes)?;
        s.serialize_field("total", &self.total_bytes)?;
        s.serialize_field("percentage", &self.percentage)?;
        s.serialize_field("speedMb", &self.speed_mb_s)?;
        s.serialize_field("finished", &self.finished)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for DownloadProgress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Helper {
            task_id: Option<String>,
            downloaded: Option<u64>,
            total: Option<u64>,
            percentage: Option<f32>,
            speed_mb: Option<f64>,
            finished: Option<bool>,
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(DownloadProgress {
            task_id: helper.task_id.unwrap_or_default(),
            downloaded_bytes: helper.downloaded.unwrap_or(0),
            total_bytes: helper.total.unwrap_or(0),
            percentage: helper.percentage.unwrap_or(0.0),
            speed_mb_s: helper.speed_mb.unwrap_or(0.0),
            finished: helper.finished.unwrap_or(false),
        })
    }
}
