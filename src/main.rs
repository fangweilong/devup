#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod config;
mod detector;
mod installer;
mod network;
mod platform;
mod state;
mod icon_bytes;
mod updater;

use icon_bytes::APP_ICON_RGBA;
use tao::window::Icon;
use std::sync::Arc;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use tokio::sync::mpsc::unbounded_channel;
use wry::{http::Request, WebViewBuilder};

use config::{AppConfig, ProxyConfig};
use detector::Detector;
use installer::InstallerEngine;
use state::{AppEvent, IpcCommand};
use updater::SelfUpdater;

#[derive(Debug)]
enum UserEvent {
    PushToWebview(AppEvent),
}

fn build_inlined_html() -> String {
    let html = include_str!("../ui/index.html");
    let css = include_str!("../ui/style.css");
    let js = include_str!("../ui/app.js");

    html.replace("__STYLE_PLACEHOLDER__", css)
        .replace("__SCRIPT_PLACEHOLDER__", js)
        .replace("__VERSION_PLACEHOLDER__", env!("CARGO_PKG_VERSION"))
}

fn main() -> anyhow::Result<()> {
    // 0. 自更新启动自检：静默清理上次原地更新遗留的 .old 旧版本文件
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(name) = current_exe.file_name().map(|n| n.to_string_lossy().to_string()) {
            let old_exe = current_exe.with_file_name(format!("{}.old", name));
            if old_exe.exists() {
                let _ = std::fs::remove_file(old_exe);
            }
        }
    }

    // 1. 初始化异步 Tokio 运行时
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );

    // 2. 初始化 GUI 事件循环与跨线程 Proxy
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window_icon = Icon::from_rgba(APP_ICON_RGBA.to_vec(), 32, 32).ok();

    let mut builder = WindowBuilder::new()
        .with_title("DevUp · 现代化开发环境与 AI 工具链管家")
        .with_inner_size(LogicalSize::new(960.0, 720.0))
        .with_min_inner_size(LogicalSize::new(860.0, 620.0));

    if let Some(icon) = window_icon {
        builder = builder.with_window_icon(Some(icon));
    }

    // macOS: 隐藏原生标题栏视觉样式，让 WebView 内容延伸到红绿灯按钮区域，
    // 与应用自身的深色头部融为一体（tao 内部已处理 WKWebView 下背景拖拽移动窗口）
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder = builder
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            .with_movable_by_window_background(true);
    }

    let window = builder.build(&event_loop)?;

    let html_content = build_inlined_html();

    // 加载持久化配置，计算初始生效代理（自定义 / 直连 / 跟随系统嗅探）
    let app_config = Arc::new(std::sync::Mutex::new(AppConfig::load()));
    let initial_proxy = app_config.lock().unwrap().effective_proxy();
    // 内存中记录当前实际生效的代理，安装/下载/自升级链路统一读取此值
    let active_proxy = Arc::new(std::sync::Mutex::new(initial_proxy));

    let (tokio_event_tx, mut tokio_event_rx) = unbounded_channel::<AppEvent>();

    // 桥接 Tokio 事件通道 -> Tao UserEvent (安全回到 GUI 主线程)
    let proxy_for_tokio = proxy.clone();
    rt.spawn(async move {
        while let Some(event) = tokio_event_rx.recv().await {
            let _ = proxy_for_tokio.send_event(UserEvent::PushToWebview(event));
        }
    });

    let rt_for_ipc = rt.clone();
    let tx_for_ipc = tokio_event_tx.clone();
    let active_proxy_for_ipc = active_proxy.clone();
    let config_for_ipc = app_config.clone();

    // 3. 构建 WebView
    let webview = WebViewBuilder::new()
        .with_html(&html_content)
        .with_ipc_handler(move |req: Request<String>| {
            let msg = req.body();
            let cmd: Result<IpcCommand, _> = serde_json::from_str(msg);
            match cmd {
                Ok(IpcCommand::Refresh) => {
                    let tx = tx_for_ipc.clone();
                    let config_for_refresh = config_for_ipc.clone();
                    let active_for_refresh = active_proxy_for_ipc.clone();
                    rt_for_ipc.spawn_blocking(move || {
                        // system 模式重新嗅探；custom/direct 按配置重算
                        let (proxy, mode, url) = {
                            let cfg = config_for_refresh.lock().unwrap();
                            (
                                cfg.effective_proxy(),
                                cfg.proxy.mode.clone(),
                                cfg.proxy.url.clone(),
                            )
                        };
                        *active_for_refresh.lock().unwrap() = proxy.clone();
                        let _ = tx.send(AppEvent::ProxyStatus {
                            proxy,
                            mode,
                            url,
                        });
                        let statuses = Detector::detect_all();
                        let _ = tx.send(AppEvent::StatusList(statuses));
                    });
                }
                Ok(IpcCommand::Install { target_id }) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        if let Err(e) = InstallerEngine::install_target(&target_id, proxy_str, tx.clone()).await {
                            let _ = tx.send(AppEvent::InstallError {
                                target_id,
                                error: format!("{:#}", e),
                            });
                        }
                    });
                }
                Ok(IpcCommand::Uninstall { target_id }) => {
                    let tx = tx_for_ipc.clone();
                    rt_for_ipc.spawn(async move {
                        if let Err(e) = InstallerEngine::uninstall_target(&target_id, tx.clone()).await {
                            let _ = tx.send(AppEvent::InstallError {
                                target_id,
                                error: format!("{:#}", e),
                            });
                        }
                    });
                }
                Ok(IpcCommand::SetProxy { mode, url }) => {
                    let (proxy, mode, url) = {
                        let mut cfg = config_for_ipc.lock().unwrap();
                        cfg.proxy = ProxyConfig { mode, url };
                        if let Err(e) = cfg.save() {
                            eprintln!("持久化代理配置失败: {:#}", e);
                        }
                        let p = cfg.effective_proxy();
                        (p, cfg.proxy.mode.clone(), cfg.proxy.url.clone())
                    };
                    *active_proxy_for_ipc.lock().unwrap() = proxy.clone();
                    let _ = tx_for_ipc.send(AppEvent::ProxyStatus { proxy, mode, url });
                }
                Ok(IpcCommand::OpenUrl { url }) => {
                    platform::open_url(&url);
                }
                Ok(IpcCommand::CheckUpdate) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        match SelfUpdater::check_for_update(proxy_str).await {
                            Ok(Some(info)) => {
                                let _ = tx.send(AppEvent::UpdateAvailable {
                                    current_version: info.current_version,
                                    latest_version: info.latest_version,
                                    release_notes: info.release_notes,
                                    download_url: info.download_url,
                                    release_url: info.release_url,
                                });
                            }
                            Ok(None) => {
                                let _ = tx.send(AppEvent::UpdateNotAvailable {
                                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                                });
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::UpdateError {
                                    error: format!("{:#}", e),
                                });
                            }
                        }
                    });
                }
                Ok(IpcCommand::PerformSelfUpdate { download_url }) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        if let Err(e) = SelfUpdater::execute_update(&download_url, proxy_str, tx.clone()).await {
                            let _ = tx.send(AppEvent::InstallError {
                                target_id: "self_update".to_string(),
                                error: format!("自升级失败: {:#}", e),
                            });
                        }
                    });
                }
                Err(err) => {
                    eprintln!("解析 IPC 指令失败: {:?}, 原报文: {}", err, msg);
                }
            }
        })
        .build(&window)?;

    // 4. 首次加载时，触发一次初始状态感知
    {
        let initial_proxy = active_proxy.lock().unwrap().clone();
        let (mode, url) = {
            let cfg = app_config.lock().unwrap();
            (cfg.proxy.mode.clone(), cfg.proxy.url.clone())
        };
        let _ = proxy.send_event(UserEvent::PushToWebview(AppEvent::ProxyStatus {
            proxy: initial_proxy,
            mode,
            url,
        }));
        let statuses = Detector::detect_all();
        let _ = proxy.send_event(UserEvent::PushToWebview(AppEvent::StatusList(statuses)));
    }

    // 5. 事件循环驱动
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::PushToWebview(app_event)) => {
                if let Ok(json_str) = serde_json::to_string(&app_event) {
                    let script = format!("window.onEvent({})", json_str);
                    let _ = webview.evaluate_script(&script);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => (),
        }
    });

    #[allow(unreachable_code)]
    Ok(())
}
