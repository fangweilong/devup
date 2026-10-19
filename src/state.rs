use serde::{Deserialize, Serialize};
use crate::detector::SoftwareStatus;
use crate::network::downloader::DownloadProgress;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "payload")]
pub enum IpcCommand {
    Refresh,
    Install { target_id: String },
    SetCustomProxy { proxy: Option<String> },
    OpenUrl { url: String },
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
