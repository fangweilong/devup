use serde::{Deserialize, Serialize};
use crate::detector::SoftwareStatus;
use crate::network::downloader::DownloadProgress;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload")]
pub enum IpcCommand {
    Refresh,
    Install { target_id: String },
    Uninstall { target_id: String },
    SetCustomProxy { proxy: Option<String> },
    OpenUrl { url: String },
    CheckUpdate,
    PerformSelfUpdate { download_url: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AppEvent {
    StatusList(Vec<SoftwareStatus>),
    ProxyDetected { proxy: Option<String> },
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
