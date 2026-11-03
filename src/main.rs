#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod config;
mod detector;
mod git;
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
    /// 首屏 HTML 加载完成后再显示窗口，避免 WebView2 初始化期间的白屏
    ShowWindow,
    /// 自绘标题栏：原生拖拽 / 最小化 / 最大化切换
    WindowDrag,
    WindowMinimize,
    WindowToggleMaximize,
    /// 退出应用（macOS Cmd+Q 等），与 CloseRequested 走同一退出链路
    Quit,
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
    // Git credential helper 是同步、短命的子进程：必须在创建窗口 / Tokio 之前分流，
    // 否则 git clone/pull/push 会顺带弹出一个 DevUp 主界面。
    if let Some(code) = crate::network::auth::run_git_credential_helper() {
        std::process::exit(code);
    }

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
        // 先隐藏，等 WebView 首屏加载完成再显示；背景色兜底避免闪白
        .with_visible(false)
        .with_background_color((9, 13, 22, 255))
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

    // Windows: 去掉系统标题栏，由前端 header 充当标题栏（拖拽 / 最小化 / 最大化 / 关闭）
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder = builder
            .with_decorations(false)
            .with_undecorated_shadow(true);
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
    // 账号元数据进内存快照，所有请求线程据此判断是否带认证头（token 仍留在系统凭据库）
    crate::network::auth::sync_accounts(&app_config.lock().unwrap().git_accounts);
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
    // IPC 线程请求退出时经 EventLoopProxy 投递 Quit（如 macOS Cmd+Q）
    let loop_proxy_for_ipc = proxy.clone();

    // 3. 构建 WebView
    let proxy_for_page_load = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_on_page_load_handler(move |event, _url| {
            if matches!(event, wry::PageLoadEvent::Finished) {
                let _ = proxy_for_page_load.send_event(UserEvent::ShowWindow);
            }
        })
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
                        // 内置镜像清单随刷新下发：前端选项面板据此渲染，且不依赖启动时序
                        let _ = tx.send(AppEvent::MirrorOptions {
                            github: crate::network::mirrors::BUILTIN_GITHUB_MIRRORS.to_vec(),
                            npm: crate::network::mirrors::BUILTIN_NPM_MIRRORS.to_vec(),
                        });
                        // 初始事件可能在 WebView 加载完成前发出：Refresh 再补一次账号与身份。
                        // 系统 Git（GCM / 钥匙串等）已有登录也要识别，读凭据放阻塞线程。
                        let accounts = tokio::task::spawn_blocking(
                            crate::network::auth::account_views_with_system,
                        )
                        .await
                        .unwrap_or_default();
                        let _ = tx.send(AppEvent::AuthAccounts { accounts });
                        let identity = tokio::task::spawn_blocking(crate::git::read_identity)
                            .await
                            .unwrap_or_default();
                        let _ = tx.send(AppEvent::GitIdentity {
                            name: identity.name,
                            email: identity.email,
                            error: None,
                        });

                        // 3. 立即下发本地已装软件探测结果（扫描会起大量子进程，放阻塞线程）
                        let statuses = tokio::task::spawn_blocking(Detector::detect_all)
                            .await
                            .unwrap_or_default();
                        let count = statuses.len();
                        let _ = tx.send(AppEvent::StatusList(statuses));

                        // 4. 并发联网拉取各组件官方最新版本并同步
                        if let Ok(client) = crate::network::downloader::create_http_client(proxy.as_deref()) {
                            let (updated, fetched_list, challenge) =
                                Detector::fetch_latest_versions_online(&client, &npm_reg).await;
                            if let Some(challenge) = challenge {
                                let _ = tx.send(AppEvent::AuthRequired(challenge));
                            }
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
                                let refreshed = tokio::task::spawn_blocking(Detector::detect_all)
                                    .await
                                    .unwrap_or_default();
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
                Ok(IpcCommand::AuthList) => {
                    let tx = tx_for_ipc.clone();
                    let config = config_for_ipc.clone();
                    let proxy = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        // 先回本地凭据存在性，界面不用等网络；再补一次在线校验，
                        // 区分「已保存但过期」与「仍然有效」。
                        let mut views = tokio::task::spawn_blocking(
                            crate::network::auth::account_views_with_system,
                        )
                        .await
                        .unwrap_or_else(|_| crate::network::auth::account_views());
                        let _ = tx.send(AppEvent::AuthAccounts {
                            accounts: views.clone(),
                        });
                        if views.iter().all(|a| !a.has_token) {
                            return;
                        }
                        let Ok(client) = crate::network::downloader::create_http_client(
                            proxy.as_deref(),
                        ) else {
                            return;
                        };

                        let mut username_changed = false;
                        for view in views.iter_mut().filter(|a| a.has_token) {
                            let Some(token) = crate::network::auth::token_for(&view.host) else {
                                continue;
                            };
                            match crate::network::auth::verify_token(
                                &client, &view.host, view.kind, &token,
                            )
                            .await
                            {
                                Ok(username) => {
                                    view.valid = Some(true);
                                    if view.username != username {
                                        view.username = username;
                                        username_changed = true;
                                    }
                                }
                                Err(crate::network::auth::VerifyError::Rejected(_)) => {
                                    view.valid = Some(false);
                                }
                                Err(crate::network::auth::VerifyError::Unavailable(_)) => {
                                    // 断网/服务端异常不能判定 token 失效
                                    view.valid = None;
                                }
                            }
                        }

                        if username_changed {
                            let mut cfg = config.lock().unwrap();
                            for view in &views {
                                if let Some(account) = cfg
                                    .git_accounts
                                    .iter_mut()
                                    .find(|a| a.host.eq_ignore_ascii_case(&view.host))
                                {
                                    account.username = view.username.clone();
                                }
                            }
                            if let Err(e) = cfg.save() {
                                eprintln!("持久化 Git 账号失败: {:#}", e);
                            }
                        }
                        let _ = tx.send(AppEvent::AuthAccounts { accounts: views });
                    });
                }
                Ok(IpcCommand::AuthLogin { host, kind, token }) => {
                    let tx = tx_for_ipc.clone();
                    let config = config_for_ipc.clone();
                    let proxy = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        let normalized = crate::network::auth::normalize_host(&host);
                        let host = match normalized {
                            Ok(h) => h,
                            Err(e) => {
                                let _ = tx.send(AppEvent::AuthLoginResult {
                                    host,
                                    username: None,
                                    error: Some(format!("{:#}", e)),
                                });
                                return;
                            }
                        };
                        let result = match crate::network::downloader::create_http_client(
                            proxy.as_deref(),
                        ) {
                            Ok(client) => {
                                crate::network::auth::verify_token(&client, &host, kind, token.trim())
                                    .await
                            }
                            Err(e) => Err(crate::network::auth::VerifyError::Unavailable(format!(
                                "{:#}",
                                e
                            ))),
                        };
                        let username = match result {
                            Ok(username) => username,
                            Err(e) => {
                                let _ = tx.send(AppEvent::AuthLoginResult {
                                    host,
                                    username: None,
                                    error: Some(format!("{:#}", e)),
                                });
                                return;
                            }
                        };

                        // 验证通过后才写系统凭据库；凭据写入失败不得留下「已登录」的假状态
                        if let Err(e) = crate::network::auth::store_token(&host, &token) {
                            let _ = tx.send(AppEvent::AuthLoginResult {
                                host,
                                username: None,
                                error: Some(format!("{:#}", e)),
                            });
                            return;
                        }

                        // 让该 host 的 git 凭据助手指向 DevUp：终端 clone/pull/push 不再询问账号密码
                        let helper_backup =
                            match crate::network::auth::install_git_helper(&host) {
                                Ok(backup) => backup,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::AuthLoginResult {
                                        host,
                                        username: None,
                                        error: Some(format!(
                                            "Token 已保存，但配置 Git 凭据助手失败：{:#}",
                                            e
                                        )),
                                    });
                                    return;
                                }
                            };

                        {
                            let mut cfg = config.lock().unwrap();
                            let backup = match helper_backup {
                                Some(backup) => backup,
                                // 已指向 DevUp：保留首次接管时存下的原配置
                                None => cfg
                                    .git_accounts
                                    .iter()
                                    .find(|a| a.host.eq_ignore_ascii_case(&host))
                                    .map(|a| a.git_helper_backup.clone())
                                    .unwrap_or_default(),
                            };
                            cfg.git_accounts
                                .retain(|a| !a.host.eq_ignore_ascii_case(&host));
                            cfg.git_accounts.push(crate::network::auth::GitAccount {
                                host: host.clone(),
                                kind,
                                username: username.clone(),
                                git_helper_backup: backup,
                            });
                            if let Err(e) = cfg.save() {
                                eprintln!("持久化 Git 账号失败: {:#}", e);
                            }
                            crate::network::auth::sync_accounts(&cfg.git_accounts);
                        }
                        crate::network::auth::clear_challenge(&host);
                        let _ = tx.send(AppEvent::AuthAccounts {
                            accounts: crate::network::auth::account_views(),
                        });
                        let _ = tx.send(AppEvent::AuthLoginResult {
                            host,
                            username: Some(username),
                            error: None,
                        });
                    });
                }
                Ok(IpcCommand::AuthLogout { host }) => {
                    // 先摘掉 git 助手并恢复原配置，再删 Token：避免 git 在窗口期读到空凭据
                    let backup = {
                        let cfg = config_for_ipc.lock().unwrap();
                        cfg.git_accounts
                            .iter()
                            .find(|a| a.host.eq_ignore_ascii_case(&host))
                            .map(|a| a.git_helper_backup.clone())
                            .unwrap_or_default()
                    };
                    if let Err(e) = crate::network::auth::uninstall_git_helper(&host, &backup) {
                        eprintln!("恢复 Git 凭据助手配置失败: {:#}", e);
                    }
                    crate::network::auth::delete_token(&host);
                    {
                        let mut cfg = config_for_ipc.lock().unwrap();
                        cfg.git_accounts
                            .retain(|a| !a.host.eq_ignore_ascii_case(&host));
                        if let Err(e) = cfg.save() {
                            eprintln!("持久化 Git 账号失败: {:#}", e);
                        }
                        crate::network::auth::sync_accounts(&cfg.git_accounts);
                    }
                    crate::network::auth::clear_challenge(&host);
                    let _ = tx_for_ipc.send(AppEvent::AuthAccounts {
                        accounts: crate::network::auth::account_views(),
                    });
                }
                Ok(IpcCommand::GitIdentityGet) => {
                    let tx = tx_for_ipc.clone();
                    rt_for_ipc.spawn(async move {
                        let identity = crate::git::read_identity();
                        let _ = tx.send(AppEvent::GitIdentity {
                            name: identity.name,
                            email: identity.email,
                            error: None,
                        });
                    });
                }
                Ok(IpcCommand::GitIdentitySet { name, email }) => {
                    let tx = tx_for_ipc.clone();
                    rt_for_ipc.spawn(async move {
                        match crate::git::save_identity(&name, &email) {
                            Ok(identity) => {
                                let _ = tx.send(AppEvent::GitIdentity {
                                    name: identity.name,
                                    email: identity.email,
                                    error: None,
                                });
                            }
                            Err(e) => {
                                // 只回错误，不清空用户已填内容
                                let _ = tx.send(AppEvent::GitIdentity {
                                    name: None,
                                    email: None,
                                    error: Some(format!("{:#}", e)),
                                });
                            }
                        }
                    });
                }
                Ok(IpcCommand::AuthDetect { host }) => {
                    let tx = tx_for_ipc.clone();
                    rt_for_ipc.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            match crate::network::auth::normalize_host(&host) {
                                Ok(host) => {
                                    if crate::network::auth::is_managed(&host) {
                                        return (
                                            host,
                                            None,
                                            Some("该域名已由 DevUp 管理，无需检测系统登录".to_string()),
                                        );
                                    }
                                    let username =
                                        crate::network::auth::detect_system_credential(&host);
                                    if username.is_some() {
                                        crate::network::auth::remember_detected_host(&host);
                                    }
                                    (host, username, None)
                                }
                                Err(e) => (host, None, Some(format!("{:#}", e))),
                            }
                        })
                        .await;

                        let (host, username, error) = match result {
                            Ok(value) => value,
                            Err(e) => (
                                String::new(),
                                None,
                                Some(format!("检测任务执行失败: {e}")),
                            ),
                        };
                        let found = username.is_some();
                        let _ = tx.send(AppEvent::AuthDetected {
                            host,
                            username,
                            error,
                        });

                        if found {
                            let accounts = tokio::task::spawn_blocking(
                                crate::network::auth::account_views_with_system,
                            )
                            .await
                            .unwrap_or_default();
                            let _ = tx.send(AppEvent::AuthAccounts { accounts });
                        }
                    });
                }
                Ok(IpcCommand::CheckUpdate) => {
                    let tx = tx_for_ipc.clone();
                    let proxy_str = active_proxy_for_ipc.lock().unwrap().clone();
                    rt_for_ipc.spawn(async move {
                        match SelfUpdater::check_for_update(proxy_str, Some(&tx)).await {
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
                    // Windows: 交给事件循环，用 tao 的原生 HTCAPTION 拖拽（保留贴边分屏）
                    #[cfg(windows)]
                    {
                        let _ = loop_proxy_for_ipc.send_event(UserEvent::WindowDrag);
                    }
                }
                Ok(IpcCommand::WindowMinimize) => {
                    let _ = loop_proxy_for_ipc.send_event(UserEvent::WindowMinimize);
                }
                Ok(IpcCommand::WindowToggleMaximize) => {
                    let _ = loop_proxy_for_ipc.send_event(UserEvent::WindowToggleMaximize);
                }
                Ok(IpcCommand::WindowClose) => {
                    let _ = loop_proxy_for_ipc.send_event(UserEvent::Quit);
                }
                Ok(IpcCommand::Quit) => {
                    // macOS Cmd+Q 由前端拦截后桥接进来，与关闭窗口同样退出
                    let _ = loop_proxy_for_ipc.send_event(UserEvent::Quit);
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

    // 首屏数据统一由前端 DOMContentLoaded 后的 Refresh 请求驱动：
    // 此时页面已能接收事件，避免启动时扫描与页面加载后的刷新重复跑两轮。
    // 兜底：WebView2 页面加载事件异常时不能让窗口永远隐藏
    let proxy_show_fallback = proxy.clone();
    rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        let _ = proxy_show_fallback.send_event(UserEvent::ShowWindow);
    });

    // 5. 事件循环驱动
    // macOS: tao 0.37 没有应用菜单，Cmd+Q 无系统级归宿。主路径是前端拦截
    // keydown 后经 IPC Quit 投递 UserEvent::Quit；这里额外跟踪修饰键并在
    // webview 尚未接管焦点时（KeyboardInput 直达 tao）原生兜底一次。
    #[cfg(target_os = "macos")]
    use tao::event::ElementState;
    #[cfg(target_os = "macos")]
    use tao::keyboard::{KeyCode, ModifiersState};
    #[cfg(target_os = "macos")]
    let mut mac_modifiers = ModifiersState::empty();
    // 页面加载与兜底定时器都会投递 ShowWindow，只允许第一次显示/抢焦点
    let mut window_shown = false;
    let mut last_maximized = window.is_maximized();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::PushToWebview(app_event)) => {
                if let Ok(json_str) = serde_json::to_string(&app_event) {
                    let script = format!("window.onEvent({})", json_str);
                    let _ = webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(UserEvent::ShowWindow) => {
                // 页面加载完成或兜底超时：只在首次显示时聚焦，避免 4 秒兜底抢走用户焦点
                if !window_shown {
                    window_shown = true;
                    window.set_visible(true);
                    window.set_focus();
                }
            }
            Event::UserEvent(UserEvent::WindowDrag) => {
                #[cfg(windows)]
                {
                    let _ = window.drag_window();
                }
            }
            Event::UserEvent(UserEvent::WindowMinimize) => {
                window.set_minimized(true);
            }
            Event::UserEvent(UserEvent::WindowToggleMaximize) => {
                let next = !window.is_maximized();
                window.set_maximized(next);
            }
            Event::UserEvent(UserEvent::Quit) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                // 按钮、系统贴边、Win+↑ 都会改变最大化状态：变化时同步图标
                let maximized = window.is_maximized();
                if maximized != last_maximized {
                    last_maximized = maximized;
                    if let Ok(json) =
                        serde_json::to_string(&AppEvent::WindowMaximized { maximized })
                    {
                        let _ = webview.evaluate_script(&format!("window.onEvent({})", json));
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            #[cfg(target_os = "macos")]
            Event::WindowEvent {
                event: WindowEvent::ModifiersChanged(mods),
                ..
            } => {
                mac_modifiers = mods;
            }
            #[cfg(target_os = "macos")]
            Event::WindowEvent {
                event: WindowEvent::KeyboardInput {
                    event: key_event, ..
                },
                ..
            } => {
                if key_event.state == ElementState::Pressed
                    && key_event.physical_key == KeyCode::KeyQ
                    && mac_modifiers.super_key()
                    && !mac_modifiers.control_key()
                    && !mac_modifiers.alt_key()
                    && !mac_modifiers.shift_key()
                {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => (),
        }
    });

    #[allow(unreachable_code)]
    Ok(())
}
