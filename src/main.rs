#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod detector;
mod installer;
mod network;
mod state;

use std::sync::Arc;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use tokio::sync::mpsc::unbounded_channel;
use wry::{http::Request, WebViewBuilder};

use detector::Detector;
use installer::InstallerEngine;
use network::proxy::detect_system_proxy;
use state::{AppEvent, IpcCommand};

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
}

fn main() -> anyhow::Result<()> {
    // 1. 初始化异步 Tokio 运行时
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );

    // 2. 初始化 GUI 事件循环与跨线程 Proxy
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("DevUp · 现代化开发环境与 AI 工具链管家")
        .with_min_inner_size(LogicalSize::new(820.0, 600.0))
        .build(&event_loop)?;

    let html_content = build_inlined_html();

    // 内存中记录当前系统代理
    let active_proxy = Arc::new(std::sync::Mutex::new(detect_system_proxy()));

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

    // 3. 构建 WebView
    let webview = WebViewBuilder::new()
        .with_html(&html_content)
        .with_ipc_handler(move |req: Request<String>| {
            let msg = req.body();
            let cmd: Result<IpcCommand, _> = serde_json::from_str(msg);
            match cmd {
                Ok(IpcCommand::Refresh) => {
                    let tx = tx_for_ipc.clone();
                    let current_proxy = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn_blocking(move || {
                        let _ = tx.send(AppEvent::ProxyDetected {
                            proxy: current_proxy,
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
                Ok(IpcCommand::SetCustomProxy { proxy }) => {
                    *active_proxy_for_ipc.lock().unwrap() = proxy.clone();
                    let _ = tx_for_ipc.send(AppEvent::ProxyDetected { proxy });
                }
                Ok(IpcCommand::OpenUrl { url }) => {
                    let _ = std::process::Command::new("cmd")
                        .args(["/c", "start", "", &url])
                        .spawn();
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
        let _ = proxy.send_event(UserEvent::PushToWebview(AppEvent::ProxyDetected {
            proxy: initial_proxy,
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
