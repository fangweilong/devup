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

/// macOS: 在真实的左键按下事件上启动系统级整窗拖拽。
///
/// 必须在前端 mousedown 阶段调用。此时 currentEvent 仍保留用户实际按下
/// 的位置，macOS 才能正确计算窗口与鼠标之间的偏移量。
#[cfg(target_os = "macos")]
unsafe fn native_drag_window(ns_window: *mut std::ffi::c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let window = ns_window as *mut AnyObject;
    let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
    let event: *mut AnyObject = msg_send![app, currentEvent];
    if event.is_null() {
        return;
    }

    let _: () = msg_send![window, performWindowDragWithEvent: event];
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
    // 0. 自更新启动自检：清理上次原地更新遗留的 .old（裸二进制 / .app 目录都覆盖）
    SelfUpdater::cleanup_update_leftovers();

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
        .with_inner_size(LogicalSize::new(1200.0, 820.0))
        .with_min_inner_size(LogicalSize::new(1000.0, 680.0));

    if let Some(icon) = window_icon {
        builder = builder.with_window_icon(Some(icon));
    }

    // macOS: 隐藏原生标题栏视觉样式，让 WebView 内容延伸到红绿灯按钮区域，
    // 与应用自身的深色头部融为一体。
    // 注意：不要开启 movable_by_window_background —— tao 的实现会在每一次
    // NSLeftMouseDown 上无条件调用 performWindowDragWithEvent:，会把边缘
    // 拉拽伸缩（以及内容区文字选中）劫持成整窗移动。窗口拖动改为：头部
    // 空白区按下时经 IPC 触发 DragWindow，由 drag_window() 定向发起整窗拖拽。
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder = builder
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true);
    }

    let window = builder.build(&event_loop)?;

    // macOS: IPC 回调需在头部拖拽达到阈值的中段直接对 NSWindow 发起拖拽，
    // 提前取裸指针（窗口存活周期等同进程，指针不会失效）。
    #[cfg(target_os = "macos")]
    let ns_window_ptr = {
        use tao::platform::macos::WindowExtMacOS;
        window.ns_window()
    };

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
                    rt_for_ipc.spawn(async move {
                        // 1. 动态刷新进程 PATH 环境变量（读取系统注册表最新 PATH）
                        crate::platform::refresh_process_path();

                        // 2. system 模式重新嗅探；custom/direct 按配置重算
                        let (proxy, mode, url, gh_mode, gh_mirror, npm_reg) = {
                            let cfg = config_for_refresh.lock().unwrap();
                            (
                                cfg.effective_proxy(),
                                cfg.proxy.mode.clone(),
                                cfg.proxy.url.clone(),
                                cfg.mirror.github_mode.clone(),
                                cfg.mirror.github_mirror.clone(),
                                cfg.mirror.npm_registry.clone(),
                            )
                        };
                        *active_for_refresh.lock().unwrap() = proxy.clone();
                        let _ = tx.send(AppEvent::ProxyStatus {
                            proxy: proxy.clone(),
                            mode,
                            url,
                            github_mode: gh_mode,
                            github_mirror: gh_mirror,
                            npm_registry: npm_reg.clone(),
                        });

                        // 3. 立即下发本地已装软件探测结果
                        let statuses = Detector::detect_all();
                        let count = statuses.len();
                        let _ = tx.send(AppEvent::StatusList(statuses));

                        // 4. 并发联网拉取各组件官方最新版本并同步
                        if let Ok(client) = crate::network::downloader::create_http_client(proxy.as_deref()) {
                            let (updated, fetched_list) = Detector::fetch_latest_versions_online(&client, &npm_reg).await;
                            if !fetched_list.is_empty() {
                                let summary = fetched_list
                                    .iter()
                                    .map(|(id, v)| format!("{}: {}", id, v))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let _ = tx.send(AppEvent::Log {
                                    target_id: "system".to_string(),
                                    message: format!("🌐 官方最新版本已从远程实时同步: {}", summary),
                                });
                            }
                            if updated {
                                let refreshed = Detector::detect_all();
                                let _ = tx.send(AppEvent::StatusList(refreshed));
                            }
                        }

                        let _ = tx.send(AppEvent::Log {
                            target_id: "system".to_string(),
                            message: format!("✅ 本地环境与组件检测完成 (共扫描 {} 项组件)", count),
                        });
                    });
                }
                Ok(IpcCommand::Install { target_id }) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    let mirror_cfg = config_for_ipc.lock().unwrap().mirror.clone();
                    rt_for_ipc.spawn(async move {
                        if let Err(e) = InstallerEngine::install_target(&target_id, proxy_str, mirror_cfg, tx.clone()).await {
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
                    let (proxy, mode, url, gh_mode, gh_mirror, npm_reg) = {
                        let mut cfg = config_for_ipc.lock().unwrap();
                        cfg.proxy = ProxyConfig { mode, url };
                        if let Err(e) = cfg.save() {
                            eprintln!("持久化代理配置失败: {:#}", e);
                        }
                        let p = cfg.effective_proxy();
                        (
                            p,
                            cfg.proxy.mode.clone(),
                            cfg.proxy.url.clone(),
                            cfg.mirror.github_mode.clone(),
                            cfg.mirror.github_mirror.clone(),
                            cfg.mirror.npm_registry.clone(),
                        )
                    };
                    *active_proxy_for_ipc.lock().unwrap() = proxy.clone();
                    let _ = tx_for_ipc.send(AppEvent::ProxyStatus {
                        proxy,
                        mode,
                        url,
                        github_mode: gh_mode,
                        github_mirror: gh_mirror,
                        npm_registry: npm_reg,
                    });
                }
                Ok(IpcCommand::SetNetworkConfig {
                    proxy_mode,
                    proxy_url,
                    github_mode,
                    github_mirror,
                    npm_registry,
                }) => {
                    let (proxy, mode, url, gh_mode, gh_mirror, npm_reg) = {
                        let mut cfg = config_for_ipc.lock().unwrap();
                        cfg.proxy = ProxyConfig {
                            mode: proxy_mode,
                            url: proxy_url,
                        };
                        cfg.mirror = crate::config::MirrorConfig {
                            github_mode,
                            github_mirror,
                            npm_registry,
                        };
                        if let Err(e) = cfg.save() {
                            eprintln!("持久化网络与镜像配置失败: {:#}", e);
                        }
                        let p = cfg.effective_proxy();
                        (
                            p,
                            cfg.proxy.mode.clone(),
                            cfg.proxy.url.clone(),
                            cfg.mirror.github_mode.clone(),
                            cfg.mirror.github_mirror.clone(),
                            cfg.mirror.npm_registry.clone(),
                        )
                    };
                    *active_proxy_for_ipc.lock().unwrap() = proxy.clone();
                    let _ = tx_for_ipc.send(AppEvent::ProxyStatus {
                        proxy,
                        mode,
                        url,
                        github_mode: gh_mode,
                        github_mirror: gh_mirror,
                        npm_registry: npm_reg,
                    });
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
                Ok(IpcCommand::DragWindow) => {
                    // macOS: 前端已按位移阈值过滤，此处发起系统级整窗拖拽
                    #[cfg(target_os = "macos")]
                    unsafe {
                        native_drag_window(ns_window_ptr);
                    }
                }
                Ok(IpcCommand::PerformSelfUpdate { download_url }) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    let mirror_cfg = config_for_ipc.lock().unwrap().mirror.clone();
                    rt_for_ipc.spawn(async move {
                        if let Err(e) = SelfUpdater::execute_update(&download_url, proxy_str, mirror_cfg, tx.clone()).await {
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
        crate::platform::refresh_process_path();
        let initial_proxy = active_proxy.lock().unwrap().clone();
        let (mode, url, gh_mode, gh_mirror, npm_reg) = {
            let cfg = app_config.lock().unwrap();
            (
                cfg.proxy.mode.clone(),
                cfg.proxy.url.clone(),
                cfg.mirror.github_mode.clone(),
                cfg.mirror.github_mirror.clone(),
                cfg.mirror.npm_registry.clone(),
            )
        };
        let _ = proxy.send_event(UserEvent::PushToWebview(AppEvent::ProxyStatus {
            proxy: initial_proxy.clone(),
            mode,
            url,
            github_mode: gh_mode,
            github_mirror: gh_mirror,
            npm_registry: npm_reg.clone(),
        }));
        let statuses = Detector::detect_all();
        let _ = proxy.send_event(UserEvent::PushToWebview(AppEvent::StatusList(statuses)));

        // 启动时在后台静默拉取一次最新版本并更新
        let tx_for_init = tokio_event_tx.clone();
        rt.spawn(async move {
            if let Ok(client) = crate::network::downloader::create_http_client(initial_proxy.as_deref()) {
                let (updated, fetched_list) = Detector::fetch_latest_versions_online(&client, &npm_reg).await;
                if !fetched_list.is_empty() {
                    let summary = fetched_list
                        .iter()
                        .map(|(id, v)| format!("{}: {}", id, v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = tx_for_init.send(AppEvent::Log {
                        target_id: "system".to_string(),
                        message: format!("🌐 官方最新版本已从远程实时同步: {}", summary),
                    });
                }
                if updated {
                    let refreshed = Detector::detect_all();
                    let _ = tx_for_init.send(AppEvent::StatusList(refreshed));
                }
            }
        });
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
