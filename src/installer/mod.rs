#[cfg(windows)]
pub mod msi;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod npm;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::AppConfig;
use crate::detector::{
    apply_target_version, check_install, check_uninstall, detect, node_channel, Detector, Channel,
    GuardDecision, NodeEnv, NodeOwner,
};
use crate::network::downloader::{create_http_client, download_file};
use crate::network::mirrors::NODEJS_MIRROR_BASE;
use crate::state::{send_guidance, AppEvent};

#[cfg(windows)]
use msi::WindowsInstaller;
#[cfg(target_os = "macos")]
use macos::MacInstaller;
use npm::NpmInstaller;

pub struct InstallerEngine;

/// AI Coding Agent 的安装规格（跨平台一致：原生自升级优先，npm 镜像兜底）
struct AgentSpec {
    label: &'static str,
    /// 已安装时优先调用的原生更新命令（None 表示仅 npm 渠道）
    cli: Option<&'static str>,
    update_args: &'static [&'static str],
    npm_pkg: &'static str,
    done_msg: &'static str,
}

fn agent_spec(software_id: &str) -> Option<AgentSpec> {
    let spec = match software_id {
        "claudecode" => AgentSpec {
            label: "Claude Code",
            cli: Some("claude"),
            update_args: &["update"],
            npm_pkg: "@anthropic-ai/claude-code",
            done_msg: "Claude Code 全局安装完成！直接在终端输入 `claude` 即可启动。",
        },
        "codex" => AgentSpec {
            label: "OpenAI Codex",
            cli: None,
            update_args: &[],
            npm_pkg: "@openai/codex",
            done_msg: "OpenAI Codex 全局安装完成！直接在终端输入 `codex` 即可启动。",
        },
        "pi" => AgentSpec {
            label: "Pi",
            cli: Some("pi"),
            update_args: &["update"],
            npm_pkg: "@earendil-works/pi",
            done_msg: "Pi Agent 安装成功！终端输入 `pi` 即可使用。",
        },
        "oh-my-pi" => AgentSpec {
            label: "omp",
            cli: Some("omp"),
            update_args: &["update"],
            npm_pkg: "@oh-my-pi/pi-coding-agent",
            done_msg: "oh-my-pi (omp) 安装成功！",
        },
        "opencode" => AgentSpec {
            label: "OpenCode",
            cli: Some("opencode"),
            update_args: &["upgrade"],
            npm_pkg: "@opencode/cli",
            done_msg: "OpenCode 安装成功！",
        },
        _ => return None,
    };
    Some(spec)
}

/// 卸载时 npm 全局包名（opencode 存在历史双包名需全部清理）
fn agent_uninstall_pkgs(software_id: &str) -> Option<&'static [&'static str]> {
    match software_id {
        "claudecode" => Some(&["@anthropic-ai/claude-code"]),
        "codex" => Some(&["@openai/codex"]),
        "pi" => Some(&["@earendil-works/pi"]),
        "oh-my-pi" => Some(&["@oh-my-pi/pi-coding-agent"]),
        "opencode" => Some(&["@opencode/cli", "opencode-ai"]),
        _ => None,
    }
}

/// G6 卸载脚本模板：`__ROOTS__` 由 `InstallerEngine::node_uninstall_script` 填成 PowerShell
/// 数组字面量（管理器数据目录）。刻意写成一行，原因见该方法注释。
/// **Windows 分支本机未验证。**
#[cfg(windows)]
const NODE_UNINSTALL_SCRIPT_TEMPLATE: &str = r#"$ErrorActionPreference='SilentlyContinue'; function DevupKey($p){ if([string]::IsNullOrWhiteSpace($p)){ return '' }; $s=($p -replace '[\\/]+','/').Trim(); if($s.Length -gt 1){ $s=$s.TrimEnd('/') }; return $s.ToLower() }; $managerRoots=@(__ROOTS__); $targets=@(); $owned=@(); foreach($pkg in @(Get-Package -Name 'Node.js*' -ErrorAction SilentlyContinue)){ $loc=DevupKey $pkg.InstallLocation; $hit=$false; foreach($r in $managerRoots){ $rk=DevupKey $r; if($rk -and $loc -and (([string]::Equals($loc,$rk,[System.StringComparison]::Ordinal) -or $loc.StartsWith($rk + '/',[System.StringComparison]::Ordinal)))){ $hit=$true; break } }; if($hit){ $owned+=('{0} [{1}]' -f $pkg.Name,$pkg.InstallLocation) } else { $targets+=$pkg } }; foreach($t in $targets){ $t | Uninstall-Package -Force -ErrorAction SilentlyContinue | Out-Null }; Write-Output ('DEVUP_RESULT targets={0} owned={1}' -f $targets.Count,$owned.Count); foreach($o in $owned){ Write-Output ('DEVUP_OWNED ' + $o) }"#;

impl InstallerEngine {
    /// 执行具体的软件安装任务（平台分发入口）
    pub async fn install_target(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        // 平台函数返回 `false` = 护栏已出指引、本轮什么都没执行。这种收尾**不能**再发
        // `InstallSuccess`：前端收到它会 `delete state.guidance[target_id]`（`ui/app.js:92-94`），
        // 刚挂上的黄色横幅会被自己抹掉，还把「未代为执行」渲染成「安装完成」。
        #[cfg(windows)]
        {
            if !Self::install_target_windows(software_id, proxy_url, mirror_config, &event_sender).await? {
                return Ok(());
            }
        }
        #[cfg(target_os = "macos")]
        {
            if !Self::install_target_macos(software_id, proxy_url, mirror_config, &event_sender).await? {
                return Ok(());
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (&software_id, &proxy_url, &mirror_config, &event_sender);
            bail!("当前平台暂不支持自动安装: {}", software_id);
        }
        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });
        Ok(())
    }

    /// 执行具体的软件卸载任务（平台分发入口）
    pub async fn uninstall_target(
        software_id: &str,
        event_sender: UnboundedSender<AppEvent>,
    ) -> Result<()> {
        // 同 `install_target`：护栏拒绝的收尾不发 `InstallSuccess`，否则前端会清掉指引横幅
        #[cfg(windows)]
        {
            if !Self::uninstall_target_windows(software_id, &event_sender).await? {
                return Ok(());
            }
        }
        #[cfg(target_os = "macos")]
        {
            if !Self::uninstall_target_macos(software_id, &event_sender).await? {
                return Ok(());
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (&software_id, &event_sender);
            bail!("当前平台暂不支持自动卸载: {}", software_id);
        }
        let _ = event_sender.send(AppEvent::InstallSuccess {
            target_id: software_id.to_string(),
        });
        Ok(())
    }

    fn make_log(event_sender: &UnboundedSender<AppEvent>, software_id: &str) -> impl Fn(String) + Send + 'static + Clone {
        let tx = event_sender.clone();
        let sid = software_id.to_string();
        move |msg: String| {
            let _ = tx.send(AppEvent::Log {
                target_id: sid.clone(),
                message: msg,
            });
        }
    }

    /// Node 所有权护栏：安装/卸载四条路径共用的前置判定（G1/G3/G4/G5/G6 的规则本体在
    /// `detector::node_source`，这里只做取证调度、`node.mode` 开关、日志与事件呈现）。
    ///
    /// 返回 `None` 表示护栏已出指引，调用方**只需 `return Ok(false)`**：这不是错误，不能走
    /// `bail!` / `InstallError`；任务态收尾（清 `activeTaskId`、收起进度、恢复按钮）由前端
    /// `handleGuidance` 代管（设计文档 D3）。返回 `Some(node_env)` 表示放行，env 带回来是给
    /// Windows 卸载侧用的（G6 的精确判据「`InstallLocation` 落在管理器目录内」只能在脚本侧做，
    /// 而管理器根目录清单只有探测结果里有）。
    ///
    /// 为什么拒绝要回 `Ok(false)` 而不是 `Ok(())`：外层 `install_target` / `uninstall_target`
    /// 会在平台函数正常返回后发 `InstallSuccess`，而前端收到它第一件事就是
    /// `delete state.guidance[target_id]`（`ui/app.js:92-94`）——刚挂上的横幅会被自己抹掉，
    /// 还把「未代为执行」渲染成「安装完成」。`false` 就是让外层跳过那次发送。
    ///
    /// **线程结论**：`detect()` 背着最长约 2s 的子进程等待（`npm config get prefix` 2s /
    /// `pkgutil --pkgs` 2s / `node --version` 3s 上限），所以整段包进 `spawn_blocking`。
    /// 安装/卸载本身并不在 UI 事件循环线程上跑——`main.rs` 的 IPC 分支是
    /// `rt_for_ipc.spawn(async move { InstallerEngine::install_target(...) })`，落在 tokio
    /// worker 线程；但同一个 multi-thread runtime 上还挂着事件桥接任务（`main.rs` 里转发
    /// `tokio_event_rx` 的那条循环），阻塞 worker 就是在挤占它，`spawn_blocking` 是阻塞调用的正解。
    async fn node_preflight(
        software_id: &str,
        ch: Channel,
        uninstall: bool,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Option<NodeEnv> {
        // 非 Node 目标零成本短路：护栏只管 nodejs，不能给 Git / PowerShell / Agent 的安装
        // 连带加一次磁盘读 + 子进程探测
        if software_id != "nodejs" {
            return Some(NodeEnv::default());
        }
        let log = Self::make_log(event_sender, software_id);

        // 开关与探测在同一次 blocking 任务里（`AppConfig::load()` 同样是磁盘动作）。
        // 为什么从磁盘读、而不是用 main.rs 里那份 `Arc<Mutex<AppConfig>>`：`install_target`
        // 的签名只接 `MirrorConfig`，而 `node.mode` 在 M0 没有 UI（D1）—— 唯一的改法是用户
        // 手改 config.json，磁盘就是这件事的真相源，也免为一个开关去改跨层签名。
        let (guards_enabled, env) = match tokio::task::spawn_blocking(|| {
            let enabled = AppConfig::load().node_guards_enabled();
            // 跳过护栏时连取证也不做：`mode = "system"` 的语义就是恢复旧行为，
            // 不该为一次不会采用的判定付掉 detect() 的那 2s（Windows 卸载侧的脚本
            // 因此拿到空的管理器清单，也就是「什么都不排除」，与旧行为一致）
            (enabled, if enabled { detect() } else { NodeEnv::default() })
        })
        .await
        {
            Ok(pair) => pair,
                // `JoinError::is_panic()` 是 `pub fn`（tokio 1.x），用 `is_panic` 而不是变体
                // 匹配：这样不依赖 Panic 变体上是否带值，将来 tokio 改了字段也不会误判。
                // 走到这里说明 detect 自己崩了（它内部所有失败路径都是 None/false 降级），
                // 按「无归属证据」继续：安装侧各条判据都不命中即放行，mac 卸载侧因没有 receipt
                // 而拒绝 —— 方向与护栏自身的保守性一致，不因为我们的故障就谎报成功。
                Err(e) => {
                    log(format!("Node 所有权探测未完成({e:?})，本次按「无归属证据」处理"));
                    (true, NodeEnv::default())
                }
            };

        if !guards_enabled {
            // 逃生口生效时必须看得见（D1 不做 UI，日志首行是唯一提示位）
            log("⚠ node.mode = \"system\"：本次已绕过 Node 所有权检查，DevUp 将直接向系统位置写入。".to_string());
            return Some(env);
        }

        // 每次判定都留一行结论：排障时能直接看到护栏读到了什么，而不是只能猜
        let managers = env
            .managers
            .iter()
            .map(|m| format!("{}({})", m.kind.label(), m.root.display()))
            .collect::<Vec<_>>()
            .join(", ");
        log(format!(
            "Node 所有权判定: owner={:?} managers=[{}]（{} / {ch:?}）",
            env.owner,
            managers,
            if uninstall { "卸载" } else { "安装" }
        ));
        if env.owner == NodeOwner::Unknown {
            // D2：Unknown 不拦安装，但要在日志留痕（卡片侧提示归任务 E）
            log(format!(
                "Node 来源未识别：{:?}（安装侧按 D2 不拦，卸载侧按保守处理）",
                env.active_node
            ));
        }

        let raw = if uninstall {
            check_uninstall(&env, ch)
        } else {
            check_install(&env, ch)
        };
        // 护栏建议的版本必须与 DevUp 本来要装的那一个一致（内存缓存，离线时退回内置基线）
        let ver = Detector::get_latest_version("nodejs").unwrap_or_default();
        match apply_target_version(raw, &ver) {
            GuardDecision::Allow => Some(env),
            GuardDecision::Refuse {
                reason,
                commands,
                note,
            } => {
                send_guidance(event_sender, software_id, reason, commands, note);
                None
            }
        }
    }

    /// G6 的脚本侧过滤（**Windows 分支本机未验证**，无 Windows 环境）。
    ///
    /// 为什么必须改写旧脚本：`Get-Package -Name 'Node.js*' | Uninstall-Package -Force` 是按
    /// **名称通配**删的，而 nvm-windows / scoop 装的 node 在注册表里同样登记成 `Node.js ...`，
    /// 于是「卸载 DevUp 认为的 node」会变成「删掉用户托管方的 runtime」。现在的判据是
    /// `InstallLocation` 归一化后是否落在某个 `manager.root` 内 —— 与 `node_source` 的
    /// `path_key`/`is_within` 同一套规则（分隔符同视、大小写折叠、去尾分隔符、边界必须落在
    /// `/` 上），只是那份逻辑在 PowerShell 里重写了一遍：脚本在目标机上跑，Rust 侧够不到
    /// `Get-Package` 的结果。
    ///
    /// 整段写成一行（`;` 分隔）：`powershell.exe -Command` 传多行文本时，换行会被它的
    /// 命令行再解析当成语句边界截掉，单行没有这个问题，也就不必临时落一个 .ps1 文件或
    /// 依赖 `-EncodedCommand`。
    ///
    /// 刻意的保守：`InstallLocation` 取不到（空）时按「非管理器」处理，交给 Uninstall-Package。
    /// 反过来会把裸 MSI 机器（A6 验收项）也一起拦掉，那是功能回归而非安全收益。
    ///
    /// **任务 F 的口径复核**（对照 mac 侧 `macos::pkg_removal_targets`，Windows 未验证）：
    /// `DevupKey` 与 `path_key` 逐条对齐 —— 分隔符同视且连续折叠、大小写折叠（PS `ToLower`
    /// 对 ASCII 路径与 Rust `to_lowercase` 同结果）、去尾分隔符且仅长度 >1 时去（`/` 自身
    /// 保留）、边界必须落在 `/` 上（`-replace` + 显式 `+ '/'` 与 `is_within` 的
    /// 「下一字节必须是 `/`」等价，`C:\Program Files\nodejs2` 不会被当成孩子）。两处刻意留着
    /// 的差异，方向都是 PS 更保守、不会多删：① `DevupKey` 多做整串 `.Trim()`（注册表值带
    /// 空白是真实脏数据，Rust 侧的根路径在采集期已 trim，等价）；② `rk == '/'` 的特例 Rust 有
    /// PS 无，但盘符路径归一化后形如 `c:/...` 不以 `/` 开头，两侧同样判不命中，属不可达输入。
    /// 已修的一处：原先用 `-eq` / `StartsWith(string)`，PowerShell 默认是 culture-sensitive
    /// 比较（撇号、连字符在某些 culture 下按可忽略处理），会把「只差标点」的无关包判成托管方，
    /// 全部判中时还会谎报「注册项全部指向管理器目录」；现改 `[StringComparison]::Ordinal`，
    /// 与 Rust 的逐字节比较同口径。
    #[cfg(windows)]
    fn node_uninstall_script(managers: &[crate::detector::Manager]) -> String {
        // 注册表里没有管理器证据时传空数组：脚本照跑，只是没什么可排除的
        // （`mode = "system"` 跳过护栏时也是这一形态 —— 逃生口的语义就是恢复旧行为）
        let roots = managers
            .iter()
            .map(|m| format!("'{}'", m.root.to_string_lossy().replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        NODE_UNINSTALL_SCRIPT_TEMPLATE.replace("__ROOTS__", &roots)
    }

    /// 只认脚本自己打的 `DEVUP_RESULT targets=.. owned=..` 与 `DEVUP_OWNED <明细>` 两类行；
    /// PowerShell 的其它输出（provider 噪音、进度）不参与判定。解析不到一律按 0 处理：
    /// 宁可再要一次指引，也不谎报「卸载成功」。
    #[cfg(windows)]
    fn parse_node_uninstall_result(text: &str) -> (usize, usize, Vec<String>) {
        let mut targets = 0usize;
        let mut owned = 0usize;
        let mut items: Vec<String> = Vec::new();
        for line in text.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("DEVUP_RESULT") {
                for kv in rest.split_whitespace() {
                    if let Some(v) = kv.strip_prefix("targets=") {
                        targets = v.parse().unwrap_or(0);
                    } else if let Some(v) = kv.strip_prefix("owned=") {
                        owned = v.parse().unwrap_or(0);
                    }
                }
            } else if let Some(item) = l.strip_prefix("DEVUP_OWNED") {
                let item = item.trim();
                if !item.is_empty() {
                    items.push(item.to_string());
                }
            }
        }
        (targets, owned, items)
    }

    /// 跨平台共享的 Agent 安装：原生自升级优先，npmmirror 全局安装兜底
    async fn install_agent(
        software_id: &str,
        proxy_ref: Option<&str>,
        npm_registry: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let Some(spec) = agent_spec(software_id) else {
            bail!("未知 Agent 目标: {}", software_id);
        };
        let log = Self::make_log(event_sender, software_id);

        if let Some(cli) = spec.cli {
            let (has_cli, _, _) = Detector::check_command(cli, "--version");
            if has_cli {
                log(format!(
                    "检测到本地已安装 {}，优先执行官方原生更新命令 (`{} {}`)...",
                    spec.label,
                    cli,
                    spec.update_args.join(" ")
                ));
                let update_res =
                    NpmInstaller::run_cli_command(cli, spec.update_args, proxy_ref, log.clone()).await;
                if update_res.is_ok() {
                    log(format!("{} 官方原生更新成功！", spec.label));
                    return Ok(());
                }
                log("原生自升级未成功，回退到国内 npmmirror 镜像重新安装...".into());
            }
        }

        log(format!(
            "使用 npm 镜像 ({}) 安装 {} ({})...",
            npm_registry, spec.label, spec.npm_pkg
        ));
        NpmInstaller::install_global(spec.npm_pkg, proxy_ref, npm_registry, log.clone()).await?;
        log(spec.done_msg.to_string());
        Ok(())
    }

    /// 跨平台共享的 Agent 卸载：npm 全局包移除 + 原生二进制清理
    async fn uninstall_agent(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<()> {
        let log = Self::make_log(event_sender, software_id);
        let Some(pkgs) = agent_uninstall_pkgs(software_id) else {
            return Ok(());
        };

        let mut any_ok = false;
        let mut last_err = None;
        for pkg in pkgs {
            log(format!("正在卸载 npm 全局包 {}...", pkg));
            match NpmInstaller::uninstall_global(pkg, log.clone()).await {
                Ok(()) => any_ok = true,
                Err(e) => last_err = Some(e),
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
                bail!("npm 全局包卸载失败: {:#}", e);
            }
        }
        Ok(())
    }

    /// 清理 Agent 原生安装的可执行文件/数据目录（平台差异部分）
    async fn cleanup_native_artifacts(software_id: &str, event_sender: &UnboundedSender<AppEvent>) {
        let log = Self::make_log(event_sender, software_id);
        let native_bin = match software_id {
            "claudecode" => Some("claude"),
            "pi" => Some("pi"),
            "oh-my-pi" => Some("omp"),
            _ => None,
        };

        if let Some(bin) = native_bin {
            #[cfg(windows)]
            {
                if let Some(home) = crate::platform::home_dir() {
                    let exe = home.join(".local").join("bin").join(format!("{}.exe", bin));
                    if exe.exists() {
                        log(format!("移除原生可执行文件: {:?}", exe));
                        let _ = tokio::fs::remove_file(exe).await;
                    }
                }
                if software_id == "pi" {
                    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                        let dir = std::path::Path::new(&local_app).join("pi");
                        if dir.exists() {
                            let _ = tokio::fs::remove_dir_all(dir).await;
                        }
                    }
                }
                if software_id == "oh-my-pi" {
                    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
                        let dir = std::path::Path::new(&local_app).join("omp");
                        if dir.exists() {
                            log(format!("清理原生文件目录: {:?}", dir));
                            let _ = tokio::fs::remove_dir_all(dir).await;
                        }
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                if let Some(home) = crate::platform::home_dir() {
                    let exe = home.join(".local").join("bin").join(bin);
                    if exe.exists() {
                        log(format!("移除原生可执行文件: {:?}", exe));
                        let _ = tokio::fs::remove_file(exe).await;
                    }
                }
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                let _ = (bin, log);
            }
        }
    }

    // ==================== Windows ====================

    /// Windows 安装。返回 `false` 表示「护栏已出指引、本轮未执行」（见 `install_target`）。
    /// **本函数内的 Node/卸载分支本机未验证（无 Windows 环境）。**
    #[cfg(windows)]
    async fn install_target_windows(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        // G1 护栏：必须早于 create_http_client / 下载 / 临时目录创建——下载几十 MB 之后才被拒
        // 是最差体验。非 nodejs 目标在 `node_preflight` 内零成本短路，所以其他卡片不受影响。
        // Windows 只有 MSI 一条路，`node_channel` 的第一个参数在这里被忽略。
        // **Windows 分支本机未验证。**
        if Self::node_preflight(software_id, node_channel(false), false, event_sender)
            .await
            .is_none()
        {
            return Ok(false);
        }

        let proxy_ref = proxy_url.as_deref();
        let client = create_http_client(proxy_ref)?;

        let temp_dir = std::env::temp_dir().join("devup-cache");
        tokio::fs::create_dir_all(&temp_dir).await?;

        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始安装任务: {}", software_id));

        match software_id {
            "powershell7" => {
                let ver = Detector::get_latest_version("powershell7").unwrap_or_else(|| "v7.4.7".to_string());
                let raw_ver = ver.trim_start_matches('v');
                let default_ps_url = format!("https://github.com/PowerShell/PowerShell/releases/download/{}/PowerShell-{}-win-x64.msi", ver, raw_ver);
                let raw_url = Detector::get_latest_version("powershell_download_url").unwrap_or(default_ps_url);
                log(format!("正在根据配置解析 PowerShell 7 ({}) GitHub 下载源 (模式: {})...", ver, mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, &raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;
                log(format!("选定加速下载链接: {}", download_url));

                let dest = temp_dir.join(format!("PowerShell-{}-win-x64.msi", raw_ver));
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log(format!("下载完成，正在后台静默安装 PowerShell 7 ({})...", ver));
                log(format!("$ msiexec.exe /i \"{}\" /quiet /norestart ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1", dest.display()));
                WindowsInstaller::install_msi(&dest, &["ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1"]).await?;
                log(format!("PowerShell 7 ({}) 安装完成！", ver));
            }

            "windows_terminal" => {
                // 如果系统支持 winget 且已通过应用商店/winget 安装，优先通过 winget 静默更新
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    log("检测到 Windows Terminal 来自 WinGet/应用商店，执行原生渠道静默升级...".into());
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Microsoft.WindowsTerminal", log.clone()).await {
                        log("Windows Terminal 通过 WinGet 渠道更新成功！".into());
                        return Ok(true);
                    }
                    log("WinGet 渠道更新失败或已是最新，回退到国内 GitHub 离线包安装...".into());
                }

                let ver = Detector::get_latest_version("windows_terminal").unwrap_or_else(|| "v1.25.2733.0".to_string());
                let raw_ver = ver.trim_start_matches('v');
                let default_wt_url = format!("https://github.com/microsoft/terminal/releases/download/{}/Microsoft.WindowsTerminal_{}_8wekyb3d8bbwe.msixbundle", ver, raw_ver);
                let raw_url = Detector::get_latest_version("windows_terminal_download_url").unwrap_or(default_wt_url);
                log(format!("正在根据配置解析 Windows Terminal ({}) GitHub 下载源 (模式: {})...", ver, mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, &raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;
                log(format!("选定加速下载链接: {}", download_url));

                let dest = temp_dir.join(format!("WindowsTerminal_{}.msixbundle", raw_ver));
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log(format!("下载完成，正在静默部署 Windows Terminal ({}) AppX...", ver));
                log(format!("$ powershell.exe -NoProfile -Command \"Add-AppxPackage -Path '{}'\"", dest.display()));
                WindowsInstaller::install_appx(&dest).await?;
                log(format!("Windows Terminal ({}) 部署成功！", ver));
            }
            "nodejs" => {
                let ver = Detector::get_latest_version("nodejs").unwrap_or_else(|| "v24.22.0".to_string());
                let download_url = format!("{}/{}/node-{}-x64.msi", NODEJS_MIRROR_BASE, ver, ver);
                log(format!("正在从阿里 Node.js 镜像高速下载 {} LTS: {}", ver, download_url));

                let dest = temp_dir.join(format!("node-{}-x64.msi", ver));
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log(format!("下载完成，正在后台静默安装 Node.js LTS ({})...", ver));
                log(format!("$ msiexec.exe /i \"{}\" /quiet /norestart", dest.display()));
                WindowsInstaller::install_msi(&dest, &[]).await?;
                log(format!("Node.js LTS ({}) 安装成功！", ver));
            }

            "git" => {
                // 如果检测到系统通过 winget 管理 Git，优先调用 winget upgrade
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    log("检测到 Git 注册于 WinGet，优先执行 winget 原生渠道更新...".into());
                    if let Ok(()) = WindowsInstaller::upgrade_via_winget("Git.Git", log.clone()).await {
                        log("Git 通过 WinGet 更新成功！".into());
                        return Ok(true);
                    }
                    log("WinGet 更新回退，改用官网安装包...".into());
                }

                let default_git_url = "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.2/Git-2.56.0.2-64-bit.exe".to_string();
                let raw_url = Detector::get_latest_version("git_download_url").unwrap_or(default_git_url);
                log(format!("正在根据配置解析 Git for Windows 国内加速地址 (模式: {})...", mirror_config.github_mode));
                let download_url = crate::network::mirrors::resolve_github_download_url(&client, &raw_url, &mirror_config.github_mode, &mirror_config.github_mirror).await;

                let dest = temp_dir.join("Git-Installer.exe");
                let tx_progress = event_sender.clone();
                download_file(&client, &download_url, &dest, software_id, move |p| {
                    let _ = tx_progress.send(AppEvent::Progress(p));
                })
                .await?;

                log("正在后台静默安装 Git...".into());
                log(format!("$ \"{}\" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL /SP- /CLOSEAPPLICATIONS", dest.display()));
                let mut cmd = tokio::process::Command::new(&dest);
                cmd.args([
                    "/VERYSILENT",
                    "/SUPPRESSMSGBOXES",
                    "/NORESTART",
                    "/NOCANCEL",
                    "/SP-",
                    "/CLOSEAPPLICATIONS",
                ]);
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);

                let out = cmd.output().await?;
                if !out.status.success() {
                    bail!("Git 静默安装异常: {:?}", out.status.code());
                }
                log("Git 安装成功！".into());
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::install_agent(software_id, proxy_ref, &mirror_config.npm_registry, event_sender).await?;
            }

            other => {
                bail!("未知软件目标: {}", other);
            }
        }

        Ok(true)
    }

    /// Windows 卸载。返回 `false` 表示「护栏已出指引、本轮未删除任何东西」。
    /// **G6 分支本机未验证（无 Windows 环境）**：脚本与结果解析按 PowerShell 语义写就，
    /// 真机需补测（见交付报告与 `docs/plan-m0-node-guardrails.md` §4.2 A5/A6）。
    #[cfg(windows)]
    async fn uninstall_target_windows(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始执行卸载任务: {}", software_id));

        match software_id {
            "windows_terminal" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.WindowsTerminal").await {
                    log("正在通过 WinGet 静默卸载 Windows Terminal...".into());
                    WindowsInstaller::uninstall_via_winget("Microsoft.WindowsTerminal", log.clone()).await?;
                    log("Windows Terminal 已卸载！".into());
                } else {
                    log("正在通过 PowerShell Remove-AppxPackage 卸载 Windows Terminal...".into());
                    let script = "Get-AppxPackage *WindowsTerminal* | Remove-AppxPackage -ErrorAction SilentlyContinue";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                    log("Windows Terminal 卸载指令执行完毕！".into());
                }
            }

            "powershell7" => {
                if WindowsInstaller::is_installed_via_winget("Microsoft.PowerShell").await {
                    log("正在通过 WinGet 卸载 PowerShell 7...".into());
                    WindowsInstaller::uninstall_via_winget("Microsoft.PowerShell", log.clone()).await?;
                } else {
                    log("正在查找 PowerShell 7 MSI 注册项并执行静默卸载...".into());
                    let script = "Get-Package -Name 'PowerShell 7*' -ErrorAction SilentlyContinue | Uninstall-Package -Force";
                    let mut cmd = tokio::process::Command::new("powershell.exe");
                    log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000);
                    let _ = cmd.args(["-NoProfile", "-Command", script]).output().await;
                }
                log("PowerShell 7 卸载完成！".into());
            }

            "nodejs" => {
                // G6 第一步（静态证据）：`C:\Program Files\nodejs` 本身已被版本管理器接管时，
                // 连卸载脚本都不必跑
                // Windows 只有 MSI 一条路（`node_channel` 的参数被忽略）；这里要接住 env，
                // 因为 G6 的精确过滤需要管理器根目录清单
                let env = match Self::node_preflight(software_id, node_channel(false), true, event_sender)
                    .await
                {
                    None => return Ok(false),
                    Some(env) => env,
                };

                log("正在通过 MSI/Package 接口执行 Node.js 静默卸载（按 InstallLocation 排除版本管理器自有目录）...".into());
                let script = Self::node_uninstall_script(&env.managers);
                let mut cmd = tokio::process::Command::new("powershell.exe");
                log(format!("$ powershell.exe -NoProfile -Command \"{}\"", script));
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);
                let out = cmd.args(["-NoProfile", "-Command", &script]).output().await?;
                // 只认脚本自己打的那一行机器可读结论，PowerShell 的其它输出（进度、
                // Provider 噪音）不参与判定
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                let (targets, owned_count, owned_items) = Self::parse_node_uninstall_result(&text);
                // 明细行比计数行更具体，但计数行更早打印；取两者的大值，避免 stdout 被截
                // 时把「挡住了一些」误报成「什么都没找到」
                let owned = owned_count.max(owned_items.len());

                if targets > 0 {
                    if owned > 0 {
                        log(format!(
                            "已跳过 {owned} 项指向版本管理器自己目录的注册项（DevUp 不代删托管方的 runtime）。"
                        ));
                    }
                    log(format!("Node.js 卸载指令已提交（{targets} 项）！"));
                    return Ok(true);
                }

                // 「一个都没命中」不是成功：要么本就没装，要么全被护栏挡在管理器目录里，
                // 两种情况用户需要知道差在哪，所以文案分开写。
                let (reason, commands) = if owned == 0 {
                    (
                        "没有发现 Node.js 的 MSI / 程序注册项，DevUp 没有执行任何删除。".to_string(),
                        Vec::new(),
                    )
                } else {
                    (
                        format!(
                            "找到的 {owned} 项 Node.js 注册项全部指向版本管理器自己的目录，DevUp 不代删：\n{}",
                            owned_items.join("\n")
                        ),
                        crate::detector::hint_commands(&env)
                            .iter()
                            .map(|c| {
                                crate::detector::fill_version_token(
                                    c,
                                    &Detector::get_latest_version("nodejs").unwrap_or_default(),
                                )
                            })
                            .collect(),
                    )
                };
                send_guidance(
                    event_sender,
                    software_id,
                    reason,
                    commands,
                    Some(crate::detector::GUARD_ESCAPE_NOTE.to_string()),
                );
                Ok(false)
            }

            "git" => {
                if WindowsInstaller::is_installed_via_winget("Git.Git").await {
                    log("通过 WinGet 静默卸载 Git...".into());
                    WindowsInstaller::uninstall_via_winget("Git.Git", log.clone()).await?;
                } else {
                    log("尝试定位 Git 默认安装目录下的 unins000.exe 静默卸载...".into());
                    let unins_paths = [
                        r"C:\Program Files\Git\unins000.exe",
                        r"C:\apps\Git\unins000.exe",
                    ];
                    let mut uninstalled = false;
                    for p in unins_paths {
                        let path = std::path::Path::new(p);
                        if path.exists() {
                            let mut cmd = tokio::process::Command::new(path);
                            log(format!("$ \"{}\" /VERYSILENT /NORESTART /SUPPRESSMSGBOXES", path.display()));
                            #[cfg(windows)]
                            cmd.creation_flags(0x08000000);
                            cmd.args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"]);
                            let _ = cmd.output().await;
                            uninstalled = true;
                            break;
                        }
                    }
                    if !uninstalled {
                        log("未找到 Git 本地 unins000.exe，尝试 winget 兜底移除...".into());
                        let _ = WindowsInstaller::uninstall_via_winget("Git.Git", |_: String| {}).await;
                    }
                }
                log("Git 卸载执行完毕！".into());
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::uninstall_agent(software_id, event_sender).await?;
                Self::cleanup_native_artifacts(software_id, event_sender).await;
            }

            other => {
                bail!("未知卸载目标: {}", other);
            }
        }

        Ok(true)
    }

    // ==================== macOS ====================

    /// macOS 安装。返回 `false` 表示「护栏已出指引、本轮未执行」（见 `install_target`）。
    #[cfg(target_os = "macos")]
    async fn install_target_macos(
        software_id: &str,
        proxy_url: Option<String>,
        mirror_config: crate::config::MirrorConfig,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        // G3/G4 护栏：必须早于 create_http_client / 下载 / 临时目录。渠道按「这次会往哪写」选，
        // 用的就是下面 nodejs 分支那同一次 `brew_available()` 探测（`&&` 短路让非 nodejs 目标
        // 连这次 which() 都不必跑）。
        let brew_ready = software_id == "nodejs" && MacInstaller::brew_available();
        if Self::node_preflight(software_id, node_channel(brew_ready), false, event_sender)
            .await
            .is_none()
        {
            return Ok(false);
        }

        let proxy_ref = proxy_url.as_deref();
        let client = create_http_client(proxy_ref)?;

        let temp_dir = std::env::temp_dir().join("devup-cache");
        tokio::fs::create_dir_all(&temp_dir).await?;

        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始安装任务: {}", software_id));

        match software_id {
            "homebrew" => {
                if MacInstaller::brew_available() {
                    log("检测到 Homebrew 已安装，执行 brew update 自更新（已启用清华镜像加速）...".into());
                    MacInstaller::brew_run(&["update"], log.clone()).await?;
                    log("Homebrew 更新完成！".into());
                } else {
                    log("Homebrew 安装脚本需要交互输入登录密码，已为您唤起系统 Terminal...".into());
                    MacInstaller::open_in_terminal(macos::HOMEBREW_INSTALL_SCRIPT)
                        .await?;
                    log("请在弹出的终端窗口中按提示完成安装（已预置国内镜像加速），完成后点击「刷新状态」。".into());
                }
            }

            "nodejs" => {
                // 复用护栏那一次探测：渠道判定与实际写入路径必须同源，否则「按 brew 拦、
                // 按 PKG 装」这种自相矛盾的结论就可能发生
                if brew_ready {
                    log("通过 Homebrew 安装/升级 Node.js LTS（清华 bottle 加速）...".into());
                    MacInstaller::brew_install_or_upgrade("node", log.clone()).await?;
                    log("Node.js LTS 安装成功！".into());
                } else {
                    let ver = Detector::get_latest_version("nodejs").unwrap_or_else(|| "v24.22.0".to_string());
                    let arch = crate::platform::mac_arch_label();
                    let file_name = if arch == "arm64" {
                        format!("node-{}-arm64.pkg", ver)
                    } else {
                        format!("node-{}.pkg", ver)
                    };
                    let download_url = format!("{}/{}/{}", NODEJS_MIRROR_BASE, ver, file_name);
                    log(format!("未检测到 Homebrew，正在从阿里 npmmirror 镜像下载官方 PKG ({}): {}", ver, download_url));
                    let dest = temp_dir.join(&file_name);
                    let tx_progress = event_sender.clone();
                    download_file(&client, &download_url, &dest, software_id, move |p| {
                        let _ = tx_progress.send(AppEvent::Progress(p));
                    })
                    .await?;

                    log("下载完成，正在弹出系统授权框以静默安装 Node.js LTS...".into());
                    MacInstaller::install_pkg(&dest, "DevUp 需要管理员权限来安装 Node.js LTS").await?;
                    log("Node.js LTS 安装成功！".into());
                }
            }

            "git" => {
                if MacInstaller::brew_available() {
                    log("通过 Homebrew 安装/升级 Git（清华 bottle 加速）...".into());
                    MacInstaller::brew_install_or_upgrade("git", log.clone()).await?;
                    log("Git 安装成功！".into());
                } else {
                    log("未检测到 Homebrew，正在唤起 Xcode 命令行工具安装对话框...".into());
                    let _ = tokio::process::Command::new("xcode-select")
                        .arg("--install")
                        .spawn();
                    log("请在系统弹窗中确认安装 Command Line Tools（内含系统 Git），完成后点击「刷新状态」。".into());
                }
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::install_agent(software_id, proxy_ref, &mirror_config.npm_registry, event_sender).await?;
            }

            other => {
                bail!("未知软件目标: {}", other);
            }
        }

        Ok(true)
    }

    /// macOS 卸载。返回 `false` 表示「护栏已出指引、本轮未删除任何东西」（G5）。
    #[cfg(target_os = "macos")]
    async fn uninstall_target_macos(
        software_id: &str,
        event_sender: &UnboundedSender<AppEvent>,
    ) -> Result<bool> {
        let log = Self::make_log(event_sender, software_id);
        log(format!(">>> 开始执行卸载任务: {}", software_id));

        match software_id {
            "homebrew" => {
                log("Homebrew 官方卸载脚本需要交互确认，已为您唤起系统 Terminal...".into());
                MacInstaller::open_in_terminal(macos::HOMEBREW_UNINSTALL_SCRIPT)
                    .await?;
                log("请在弹出的终端窗口中完成卸载流程。".into());
            }

            "nodejs" => {
                // G5 第一层：护栏必须早于任何删除动作（现状是闭眼 `rm -rf /usr/local/bin/node`）。
                // 渠道与下面实际走的分支同源：brew 自管移除不在 G5 射程，只有要动 /usr/local
                // 时才要求 PKG receipt 或 owner=Brew 的证据。
                let brew_node = MacInstaller::brew_installed_formula("node");
                // env 要带下去：receipt 放行 ≠ 清单可全删（§2.5 第 2 步，候选可能是指向
                // 管理器目录的 symlink），PKG 分支的所有权核对靠它
                let env = match Self::node_preflight(software_id, node_channel(brew_node), true, event_sender)
                    .await
                {
                    None => return Ok(false),
                    Some(env) => env,
                };
                if brew_node {
                    log("正在通过 Homebrew 卸载 node formula...".into());
                    MacInstaller::brew_run(&["uninstall", "node"], log.clone()).await?;
                } else {
                    let removal = MacInstaller::remove_pkg_node_files(
                        &env,
                        "DevUp 需要管理员权限来卸载 Node.js",
                        log.clone(),
                    )
                    .await?;
                    // G5 第二层：跳过项逐项呈现。用户必须能看出「少删了什么、为什么」，
                    // 否则「卸载完成」就是个谎 —— 他以为 DevUp 清了 /usr/local，其实有一半
                    // 是别人的东西。
                    for p in &removal.skipped {
                        log(format!("已跳过（所有权指向版本管理器目录，不代删）: {}", p.display()));
                    }
                    if !removal.executed {
                        // 核对后一个可删项都没有：没弹系统授权框、也没删任何东西。这不是错误，
                        // 按护栏契约出指引并 `Ok(false)` —— 发 `InstallSuccess` 会让前端把
                        // 横幅抹掉、把「未执行」渲染成「卸载完成」（§9 台账 R1）。
                        let skipped: Vec<String> = removal
                            .skipped
                            .iter()
                            .map(|p| format!("  {}", p.display()))
                            .collect();
                        let ver = Detector::get_latest_version("nodejs").unwrap_or_default();
                        send_guidance(
                            event_sender,
                            software_id,
                            format!(
                                "官方 PKG 卸载清单里的 {} 个路径经所有权核对后全部落在版本管理器自己的目录内（它们是指向托管方的链接，不是官方 PKG 装入的实体），DevUp 没有执行任何删除，也没有弹出系统授权框：\n{}",
                                skipped.len(),
                                skipped.join("\n")
                            ),
                            crate::detector::hint_commands(&env)
                                .iter()
                                .map(|c| crate::detector::fill_version_token(c, &ver))
                                .collect(),
                            Some(crate::detector::GUARD_ESCAPE_NOTE.to_string()),
                        );
                        return Ok(false);
                    }
                    log(format!("已清理 {} 项官方 PKG 落点。", removal.deleted.len()));
                }
                log("Node.js 卸载完成！".into());
            }

            "git" => {
                if MacInstaller::brew_installed_formula("git") {
                    log("正在通过 Homebrew 卸载 git formula...".into());
                    MacInstaller::brew_run(&["uninstall", "git"], log.clone()).await?;
                    log("Git 卸载完成！".into());
                } else {
                    log("当前 Git 由 Xcode 命令行工具 (/usr/bin/git) 提供，随系统管理，不支持单独卸载。".into());
                }
            }

            "claudecode" | "codex" | "pi" | "oh-my-pi" | "opencode" => {
                Self::uninstall_agent(software_id, event_sender).await?;
                Self::cleanup_native_artifacts(software_id, event_sender).await;
            }

            other => {
                bail!("未知卸载目标: {}", other);
            }
        }

        Ok(true)
    }
}
