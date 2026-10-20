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
    /// 护栏拒绝这类「按设计没有执行」的指引。它不是错误，所以与 InstallError 分开：
    /// 前端渲染成中性/黄色横幅 + 可复制命令，复用红字失败样式会让用户以为 DevUp 坏了。
    /// 构造点：`installer::InstallerEngine::node_preflight()`（Node 所有权护栏 G1–G6）。
    Guidance {
        target_id: String,
        /// 为什么不做，一句话讲清归属，例如「Node.js 由 nvm 托管」
        reason: String,
        /// 建议用户在终端里自行执行的命令，逐条展示、可一键复制
        commands: Vec<String>,
        /// 可选补充：逃生口提示 / 影响说明
        note: Option<String>,
    },
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

/// 指引事件的统一出口，供护栏拒绝分支调用：调用方只填语义字段，不必自己拼
/// AppEvent、也不必关心通道类型（安装/卸载链路手上就是 `&UnboundedSender<AppEvent>`）。
///
/// 这里吞掉 SendError 是刻意的：发送失败只会发生在 WebView 已销毁时，而「指引没发出去」
/// 不该改变拒绝的返回结果 —— 拒绝仍须按设计返回 `Ok(())`，不能因此翻成错误路径。
pub fn send_guidance(
    event_sender: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    target_id: &str,
    reason: impl Into<String>,
    commands: Vec<String>,
    note: Option<String>,
) {
    let _ = event_sender.send(AppEvent::Guidance {
        target_id: target_id.to_string(),
        reason: reason.into(),
        commands,
        note,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_guidance() -> AppEvent {
        AppEvent::Guidance {
            target_id: "nodejs".to_string(),
            reason: "Node.js 由 nvm 托管（~/.nvm，当前 v26.12.0）".to_string(),
            commands: vec!["nvm install 24.22.0".to_string(), "nvm use 24.22.0".to_string()],
            note: Some("确需 DevUp 接管系统安装时，把 config.json 的 node.mode 设为 system 可跳过全部护栏".to_string()),
        }
    }

    /// 事件形状由 ui/app.js 的 switch 字面量与 handleGuidance 里的字段读取共同约定，
    /// 这里用序列化后的 JSON 快照把形状钉死。
    #[test]
    fn test_guidance_event_json_snapshot() {
        let json = serde_json::to_string(&sample_guidance()).unwrap();
        assert_eq!(
            json,
            r#"{"type":"Guidance","data":{"target_id":"nodejs","reason":"Node.js 由 nvm 托管（~/.nvm，当前 v26.12.0）","commands":["nvm install 24.22.0","nvm use 24.22.0"],"note":"确需 DevUp 接管系统安装时，把 config.json 的 node.mode 设为 system 可跳过全部护栏"}}"#
        );
    }

    /// 任务 D 可能先只给 reason，且逃生口提示未必有：空命令与缺省 note 也要有稳定形状
    /// （`note` 无 skip_serializing_if，缺省是显式 null 而非字段消失，前端据此判空）。
    #[test]
    fn test_guidance_event_json_snapshot_minimal() {
        let json = serde_json::to_string(&AppEvent::Guidance {
            target_id: "nodejs".to_string(),
            reason: "Node.js 由 nvm 托管".to_string(),
            commands: vec![],
            note: None,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"Guidance","data":{"target_id":"nodejs","reason":"Node.js 由 nvm 托管","commands":[],"note":null}}"#
        );
    }

    /// 前后端同源校验：事件名不写死，从真实序列化结果里取（改 rename_all 或变体名会立刻
    /// 反映到这里），再拿它去前端派发点找分支；字段名逐一对 handleGuidance 的读取处。
    /// 意义在于后端改了形状、前端没跟上时测试挂住，而不是线上静默丢消息
    /// （window.onEvent 的 switch 没有 default 分支）。
    #[test]
    fn test_guidance_event_contract_matches_ui() {
        let json = serde_json::to_string(&sample_guidance()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let event_type = value["type"].as_str().unwrap();

        let ui = include_str!("../ui/app.js");
        assert!(
            ui.contains(&format!("case \"{event_type}\":")),
            "ui/app.js 的 window.onEvent 缺少 {event_type} 分支，指引会被静默丢弃"
        );

        // 截取 handleGuidance 函数体：内部各块都以缩进收尾，首个顶格 "\n}" 即函数结尾
        let start = ui
            .find("function handleGuidance(")
            .expect("ui/app.js 缺少 handleGuidance 派发函数");
        let body = &ui[start..];
        let body = &body[..body.find("\n}").expect("handleGuidance 函数体边界异常")];

        for field in ["target_id", "reason", "commands", "note"] {
            assert!(value["data"].get(field).is_some(), "快照缺少字段 {field}");
            let access = format!("data.{field}");
            assert!(
                body.contains(&access),
                "handleGuidance 未读取 {access}：后端字段名与前端已漂移"
            );
        }
    }
}
