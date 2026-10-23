use crate::detector::node_source;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
#[cfg(windows)]
use winreg::RegKey;

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 全局组件最新版本内存缓存
static VERSION_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoftwareStatus {
    pub id: String,
    pub name: String,
    pub category: String, // "env" 或 "agent"
    pub description: String,
    pub repo_url: String,
    pub installed: bool,
    pub version: Option<String>,
    pub latest_version: Option<String>,
    pub install_method: Option<String>,
    pub default_channel: String, // 安装方式说明，例如 "WinGet / 微软官方MSIXBundle"、"国内官方npmmirror镜像 / 原生安装器" 等
    /// 按 D4「显示但不代管」：这份 runtime 由哪个版本管理器托管（取 `ManagerKind::label()`，
    /// 如 "nvm"）。GUI 的 PATH 解析不到 node 时，卡片据此显示「由 nvm 托管」而不是谎报「未安装」，
    /// 但 DevUp 不因此认为那份 runtime 可由它操作（`installed` 的判定口径不受这个字段影响）。
    /// 非 Node 卡片恒为 `None`。
    #[serde(default)]
    pub managed_by: Option<String>,
    /// 主托管方自己装了哪些版本，语义降序、形如 "26.12.0"（展示方自行补 `v`）。
    /// 只属于 `managed_by` 那一个管理器：本机 nvm 与 volta 共存，把 volta 的版本混进来就是假信息。
    /// 「本机还有哪些管理器」请遍历 `node_source::NodeEnv::managers`，那里才是唯一真相源。
    #[serde(default)]
    pub managed_versions: Vec<String>,
    /// 任务 H：护栏此刻会拒绝「安装族」动作时，随卡片下发的指引内容
    /// （reason / 已填版本的 commands / note），来源是 `node_source::guidance_preview`
    /// ——与真拒绝时 `Guidance` 横幅**同一条判定函数的输出**，前端不得自造第二份。
    /// `None` = 当前判定放行（裸装机器 / brew 可升级 / mode=system 逃生口生效），
    /// 前端按普通确认弹窗走。
    #[serde(default)]
    pub guard_hint: Option<node_source::Guidance>,
    /// 任务 H（卸载侧）：`check_uninstall` 对本机实际卸载渠道的预览。
    /// 与安装不同，卸载**可能真放行**（mac 有 PKG receipt、或 brew 装着 node 走 brew 自管），
    /// 所以前端只在 `Some` 时换成指引窗；`None` 时保留确认步骤，那一下是真删。
    #[serde(default)]
    pub uninstall_hint: Option<node_source::Guidance>,
}

pub struct Detector;

impl Detector {
    /// 清洗与格式化从 CLI 输出中读取到的版本字符串
    /// 例如: "2.1.303 (Claude Code)" -> "2.1.303"
    ///       "codex-cli 0.163.0" -> "0.163.0"
    ///       "omp/18.12.2" -> "18.12.2"
    ///       "opencode v2.0.32" -> "2.0.32"
    ///       "git version 2.55.0.windows.3" -> "2.55.0"
    ///       "PowerShell 7.4.20" -> "7.4.20"
    ///       "v24.21.0" -> "24.21.0"
    pub fn clean_version_string(raw: &str) -> String {
        let trimmed = raw.trim();
        for part in trimmed.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '/' || c == ',') {
            let p = part.trim().trim_start_matches(|c| c == 'v' || c == 'V');
            if p.is_empty() {
                continue;
            }
            let dots = p.chars().filter(|&c| c == '.').count();
            let digits_start = p.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
            if digits_start && dots >= 1 {
                let clean_p = p.split(|c: char| c.is_alphabetic()).next().unwrap_or(p).trim_end_matches('.');
                if !clean_p.is_empty() {
                    return clean_p.to_string();
                }
            }
        }
        trimmed.to_string()
    }

    /// 探测命令是否存在并读取版本输出（跨平台入口）
    pub fn check_command(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        #[cfg(windows)]
        {
            Self::check_command_windows(cmd, version_arg)
        }
        #[cfg(not(windows))]
        {
            Self::check_command_unix(cmd, version_arg)
        }
    }

    /// Unix/macOS：which 风格查找 + 直接执行取版本（GUI 进程需扩展 PATH）
    #[cfg(not(windows))]
    fn check_command_unix(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        let base_cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);

        let path = crate::platform::which(base_cmd)
            .or_else(|| crate::platform::which(cmd))
            .map(|p| p.to_string_lossy().to_string());
        let Some(found) = path else {
            return (false, None, None);
        };

        let mut command = Command::new(&found);
        command.arg(version_arg);
        command.env("PATH", crate::platform::child_path());

        let version = command.output().ok().and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() {
                    let err_s = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if !err_s.is_empty() {
                        return Some(err_s.lines().next().unwrap_or("").to_string());
                    }
                    None
                } else {
                    Some(s.lines().next().unwrap_or("").to_string())
                }
            } else {
                None
            }
        }).map(|raw| Self::clean_version_string(&raw));

        (true, version, Some(found))
    }

    /// Windows: where.exe 探测 + cmd /c 兜底执行（支持 npm 全局 .cmd shim）
    #[cfg(windows)]
    fn check_command_windows(cmd: &str, version_arg: &str) -> (bool, Option<String>, Option<String>) {
        let child_path = crate::platform::child_path();
        let base_cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);

        // 1. 先用 where.exe 探测（同时支持 .exe, .cmd, .bat 等扩展名）
        let mut where1 = Command::new("where.exe");
        where1.arg(base_cmd);
        where1.env("PATH", &child_path);
        #[cfg(windows)]
        where1.creation_flags(CREATE_NO_WINDOW);

        let mut where2 = Command::new("where.exe");
        where2.arg(cmd);
        where2.env("PATH", &child_path);
        #[cfg(windows)]
        where2.creation_flags(CREATE_NO_WINDOW);

        let out1 = where1.output().ok();
        let out2 = where2.output().ok();

        let exists = out1.as_ref().map(|o| o.status.success()).unwrap_or(false)
            || out2.as_ref().map(|o| o.status.success()).unwrap_or(false);

        if !exists {
            return (false, None, None);
        }

        let cmd_path = out1
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).to_string();
                s.lines().next().map(|l| l.trim().to_string())
            })
            .or_else(|| {
                out2.and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    s.lines().next().map(|l| l.trim().to_string())
                })
            });

        // 2. 尝试获取版本信息：优先直接执行，失败则回退到 cmd /c（支持 npm 全局安装的 .cmd / .bat 脚本）
        let try_run = |use_shell: bool| -> Option<String> {
            let mut command = if use_shell {
                let mut c = Command::new("cmd.exe");
                c.args(["/c", base_cmd, version_arg]);
                c
            } else {
                let mut c = Command::new(cmd);
                c.arg(version_arg);
                c
            };

            command.env("PATH", &child_path);
            #[cfg(windows)]
            command.creation_flags(CREATE_NO_WINDOW);

            command.output().ok().and_then(|out| {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if s.is_empty() {
                        let err_s = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        if !err_s.is_empty() {
                            return Some(err_s.lines().next().unwrap_or("").to_string());
                        }
                        None
                    } else {
                        Some(s.lines().next().unwrap_or("").to_string())
                    }
                } else {
                    None
                }
            })
        };

        let version = try_run(false)
            .or_else(|| try_run(true))
            .map(|raw| Self::clean_version_string(&raw));
        (true, version, cmd_path)
    }

    /// 检测 Windows Terminal 状态与版本（仅 Windows）
    /// 避免直接运行 `wt.exe -v` / `--version` 触发 GUI 弹窗
    #[cfg(windows)]
    pub fn check_windows_terminal() -> (bool, Option<String>, Option<String>) {
        {
            // 优先从注册表 App Paths 读取安装路径并提取版本号
            let query_reg = |hkey| -> Option<String> {
                let root = RegKey::predef(hkey);
                let subkey = root
                    .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\wt.exe")
                    .ok()?;
                let path: String = subkey.get_value("").ok()?;
                // 路径通常为: C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.24.12741.0_x64__8wekyb3d8bbwe\wt.exe
                if let Some(start) = path.find("Microsoft.WindowsTerminal_") {
                    let sub = &path[start + "Microsoft.WindowsTerminal_".len()..];
                    if let Some(end) = sub.find('_') {
                        return Some(sub[..end].to_string());
                    }
                }
                Some("已安装".to_string())
            };

            if let Some(ver) = query_reg(HKEY_CURRENT_USER).or_else(|| query_reg(HKEY_LOCAL_MACHINE)) {
                return (true, Some(ver), Some("winget/msix".to_string()));
            }
        }

        // 回退检查 where.exe 是否能找到 wt
        let child_path = crate::platform::child_path();
        let mut cmd = Command::new("where.exe");
        cmd.arg("wt");
        cmd.env("PATH", &child_path);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
        (ok, if ok { Some("已安装".to_string()) } else { None }, if ok { Some("winget/msix".to_string()) } else { None })
    }

    /// 分析可执行文件路径，判定其安装途径
    pub fn infer_install_method(path_opt: Option<&str>, default_method: &str) -> Option<String> {
        let p = path_opt?.to_lowercase();
        #[cfg(windows)]
        {
            if p.contains("windowsapps") {
                Some("winget".to_string())
            } else if p.contains("node_modules") || p.contains("npm") || p.contains("nvm") {
                Some("npm".to_string())
            } else if p.contains(".local\\bin") || p.contains("appdata\\local\\omp") || p.contains("appdata\\local\\pi") {
                Some("native_cli".to_string())
            } else {
                Some(default_method.to_string())
            }
        }
        #[cfg(not(windows))]
        {
            if p.contains("/homebrew/") || p.contains("/cellar") {
                Some("brew".to_string())
            } else if p.contains("node_modules") || p.contains("/npm/") || p.contains("nvm") {
                Some("npm".to_string())
            } else if p.contains(".local/bin") {
                Some("native_cli".to_string())
            } else if p.starts_with("/usr/bin") || p.starts_with("/bin") || p.starts_with("/sbin") {
                Some("system".to_string())
            } else {
                Some(default_method.to_string())
            }
        }
    }

    /// 选出卡片「由 X 托管」的那一个主托管方（规则定死于设计文档 D4 / 实施计划 §2.7）。
    ///
    /// 纯函数：不读环境变量、不碰文件系统、不起进程，`detect()` 的结论全部由参数给出，
    /// 因此四条规则可以表驱动覆盖。与 `node_source::pick_managed_versions` 的分工是：
    /// 那边回答「生效那份归谁」（owner 不命中就留空），这边回答「卡片该说谁在托管」——
    /// GUI 认不出 owner 时也得给用户一个答案，于是按「谁装的版本最高」挑一个主托管方，
    /// **其余管理器不进 `managed_by`**（保持单一标签，不拼成 "nvm, volta"）。
    ///
    /// `Brew` / `SystemPkg` 返回 `None`：那两种情况下生效的 runtime 就在 DevUp 能直接操作的
    /// 位置上，说「托管中」反而是假陈述（渠道信息由 `install_method` 如实给出）。
    fn pick_managed_by(
        owner: node_source::NodeOwner,
        managers: &[node_source::Manager],
    ) -> Option<(&'static str, Vec<String>)> {
        match owner {
            // owner 命中那一个：只报它自己的版本，别的管理器一个字符都不许混进来
            node_source::NodeOwner::Manager(kind) => managers
                .iter()
                .find(|m| m.kind == kind && !m.versions.is_empty())
                .or_else(|| managers.iter().find(|m| m.kind == kind))
                .map(|m| (m.kind.label(), m.versions.clone())),
            // GUI 解析不到 node（nvm-only 机器从 Finder 启动的常态）或归属不明：
            // 主托管方 = 自己装了最高语义版本的那一个；一个版本都没有就退回清单第一个
            node_source::NodeOwner::None | node_source::NodeOwner::Unknown => {
                let mut best: Option<(&node_source::Manager, Vec<u64>)> = None;
                for m in managers {
                    // 比较键复用 `node_source::version_key`（数值逐段比，字典序会把 9 排到 26 前面）：
                    // 展示侧与采集侧共用一份，避免计划 §2.2 警告的「两处各写一遍、迟早给出两种结论」
                    let Some(key) = m.versions.first().map(|v| node_source::version_key(v)) else {
                        continue;
                    };
                    // 各管理器的 versions 已是降序，所以第一个就是它的最高版本
                    let take = match &best {
                        Some((_, best_key)) => key > *best_key,
                        None => true,
                    };
                    if take {
                        best = Some((m, key));
                    }
                }
                match best {
                    Some((m, _)) => Some((m.kind.label(), m.versions.clone())),
                    None => managers.first().map(|m| (m.kind.label(), Vec::new())),
                }
            }
            node_source::NodeOwner::Brew | node_source::NodeOwner::SystemPkg => None,
        }
    }

    /// 把 Node 所有权结论落到 nodejs 卡片上：托管展示字段 + `install_method` 口径修正。
    ///
    /// 只喂 nodejs 这一张卡片。`infer_install_method` 的字符串猜测继续服务其余卡片：
    /// Agent 落在 `~/.nvm/versions/node/*/bin` 下本来就是 npm 全局包，判 `"npm"` 没有错，
    /// 错的是拿同一套字面规则去说「node 这个 runtime 是 npm 装的」。
    fn apply_node_ownership(status: &mut SoftwareStatus, env: &node_source::NodeEnv) {
        if let Some((label, versions)) = Self::pick_managed_by(env.owner, &env.managers) {
            status.managed_by = Some(label.to_string());
            status.managed_versions = versions;
        }
        // Node 卡片的渠道按 owner 说真话：管理器 → 管理器名，brew → "brew"；
        // 系统包 / 归属不明 / 无 node 时保留原有路径推断（mac "pkg" / win "msi" 本来就是对的）
        match env.owner {
            node_source::NodeOwner::Manager(kind) => {
                status.install_method = Some(kind.label().to_string())
            }
            node_source::NodeOwner::Brew => status.install_method = Some("brew".to_string()),
            _ => {}
        }
    }

    /// 任务 H 组装：护栏若会拒掉安装族 / 卸载，把指引内容随卡片下发（`guard_hint` /
    /// `uninstall_hint`，分别喂「安装族」与「卸载」两类动作的弹窗）。
    ///
    /// 预览不是新判定：`guidance_preview` 调的就是护栏的同一对 `check_install` /
    /// `check_uninstall` + `apply_target_version`，文案与口径改动自动跟随，
    /// 不存在「弹窗一套、横幅另一套」的漂移面。渠道由调用侧传入（与真点击时
    /// `install_target_macos` / `uninstall_target_macos` 的路由各自同源），磁盘配置与
    /// brew 探测这些环境动作也留在 `detect_all`，本函数保持可表驱动测试。
    fn attach_guard_hints(
        status: &mut SoftwareStatus,
        env: &node_source::NodeEnv,
        guards_enabled: bool,
        install_channel: node_source::Channel,
        uninstall_channel: node_source::Channel,
    ) {
        status.guard_hint = None;
        status.uninstall_hint = None;
        if !guards_enabled {
            return;
        }
        // 建议版本与安装侧 `node_preflight` 同源：都取全局缓存（离线时回退内置基线）
        let ver = Self::get_latest_version("nodejs").unwrap_or_default();
        status.guard_hint = node_source::guidance_preview(env, install_channel, &ver, false);
        status.uninstall_hint = node_source::guidance_preview(env, uninstall_channel, &ver, true);
    }

    /// 查询指定组件已记录的最新版本（优先缓存，缺失回退内置基线）
    pub fn get_latest_version(id: &str) -> Option<String> {
        if let Ok(guard) = VERSION_CACHE.lock() {
            if let Some(v) = guard.get(id) {
                return Some(v.clone());
            }
        }
        Self::default_latest_version(id).map(|s| s.to_string())
    }

    /// 设置指定组件的最新版本到内存缓存
    pub fn set_latest_version(id: &str, ver: String) {
        if let Ok(mut guard) = VERSION_CACHE.lock() {
            guard.insert(id.to_string(), ver);
        }
    }

    /// 内置兜底基线版本（当网络不可达或首次启动时使用）
    pub fn default_latest_version(id: &str) -> Option<&'static str> {
        match id {
            "powershell7" => Some("v7.4.7"),
            "windows_terminal" => Some("v1.25.2733.0"),
            "nodejs" => Some("v24.21.0"),
            "git" => Some("v2.56.0"),
            "homebrew" => Some("4.4.20"),
            "claudecode" => Some("v2.1.303"),
            "codex" => Some("v0.163.0"),
            "pi" => Some("v1.3.0"),
            "oh-my-pi" => Some("v18.12.2"),
            "opencode" => Some("v2.0.32"),
            _ => None,
        }
    }

    /// 联网并发拉取所有组件的官方最新版本，并更新内存缓存
    /// 返回 (是否有版本变更, 本次成功拉取到的版本列表[(id, version)])
    pub async fn fetch_latest_versions_online(
        client: &reqwest::Client,
        npm_registry: &str,
    ) -> (bool, Vec<(String, String)>) {
        let mut updated = false;
        let mut fetched_results = Vec::new();
        let clean_registry = npm_registry.trim_end_matches('/');

        let mut futs = Vec::new();

        // 1. npm Agent 最新版本查询 (通过 npm 镜像源)
        let npm_tasks = [
            ("claudecode", "@anthropic-ai/claude-code"),
            ("codex", "@openai/codex"),
            ("oh-my-pi", "@oh-my-pi/pi-coding-agent"),
            ("opencode", "@opencode/cli"),
        ];

        for (id, pkg) in npm_tasks {
            let url = format!("{}/{}/latest", clean_registry, pkg);
            let client_c = client.clone();
            futs.push(tokio::spawn(async move {
                let res = client_c
                    .get(&url)
                    .timeout(std::time::Duration::from_millis(6000))
                    .send()
                    .await;
                if let Ok(resp) = res {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(ver) = json.get("version").and_then(|v| v.as_str()) {
                            let formatted = if ver.starts_with('v') || ver.starts_with('V') {
                                ver.to_string()
                            } else {
                                format!("v{}", ver)
                            };
                            return Some((id.to_string(), formatted));
                        }
                    }
                }
                None
            }));
        }

        // 2. Node.js LTS 版本查询（使用 nodejs 阿里镜像源 index.json）
        {
            let client_c = client.clone();
            futs.push(tokio::spawn(async move {
                let url = "https://npmmirror.com/mirrors/node/index.json";
                let res = client_c
                    .get(url)
                    .timeout(std::time::Duration::from_millis(6000))
                    .send()
                    .await;
                if let Ok(resp) = res {
                    if let Ok(list) = resp.json::<Vec<serde_json::Value>>().await {
                        for item in list {
                            let is_lts = item.get("lts").and_then(|v| v.as_bool()).unwrap_or(false)
                                || item.get("lts").and_then(|v| v.as_str()).is_some();
                            if is_lts {
                                if let Some(ver) = item.get("version").and_then(|v| v.as_str()) {
                                    return Some(("nodejs".to_string(), ver.to_string()));
                                }
                            }
                        }
                    }
                }
                None
            }));
        }

        // 3. GitHub Releases 查询 (PowerShell, Git, Windows Terminal, Pi, Homebrew)
        let gh_repos = [
            ("powershell7", "PowerShell/PowerShell"),
            ("git", "git-for-windows/git"),
            ("windows_terminal", "microsoft/terminal"),
            ("pi", "earendil-works/pi"),
            ("homebrew", "Homebrew/brew"),
        ];

        for (id, repo) in gh_repos {
            let client_c = client.clone();
            futs.push(tokio::spawn(async move {
                // 优先通过 GitHub API 查询
                let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo);
                let res = client_c
                    .get(&api_url)
                    .header("User-Agent", "DevUp-Client")
                    .timeout(std::time::Duration::from_millis(5000))
                    .send()
                    .await;

                if let Ok(resp) = res {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        // 从 GitHub Release 元数据中提取对应系统的安装包真实直链
                        if let Some(assets) = json.get("assets").and_then(|a| a.as_array()) {
                            for asset in assets {
                                if let (Some(name), Some(url)) = (
                                    asset.get("name").and_then(|n| n.as_str()),
                                    asset.get("browser_download_url").and_then(|u| u.as_str()),
                                ) {
                                    if id == "git" && name.ends_with("64-bit.exe") && !name.contains("pdbs") {
                                        Self::set_latest_version("git_download_url", url.to_string());
                                    } else if id == "powershell7" && name.ends_with("win-x64.msi") {
                                        Self::set_latest_version("powershell_download_url", url.to_string());
                                    } else if id == "windows_terminal" && name.ends_with(".msixbundle") && !name.contains("PreinstallKit") {
                                        Self::set_latest_version("windows_terminal_download_url", url.to_string());
                                    }
                                }
                            }
                        }

                        if let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) {
                            let clean_tag = tag.split(".windows.").next().unwrap_or(tag);
                            return Some((id.to_string(), clean_tag.to_string()));
                        }
                    }
                }

                // 兜底：通过 github.com releases/latest 302 重定向解析 tag
                let page_url = format!("https://github.com/{}/releases/latest", repo);
                if let Ok(resp) = client_c
                    .get(&page_url)
                    .header("User-Agent", "Mozilla/5.0")
                    .timeout(std::time::Duration::from_millis(5000))
                    .send()
                    .await
                {
                    let final_url = resp.url().as_str();
                    if let Some(idx) = final_url.rfind("/tag/") {
                        let tag = &final_url[idx + "/tag/".len()..];
                        let clean_tag = tag.split(".windows.").next().unwrap_or(tag);
                        if !clean_tag.is_empty() {
                            return Some((id.to_string(), clean_tag.to_string()));
                        }
                    }
                }

                None
            }));
        }

        for f in futs {
            if let Ok(Some((id, ver))) = f.await {
                let current = Self::get_latest_version(&id);
                if current.as_deref() != Some(&ver) {
                    Self::set_latest_version(&id, ver.clone());
                    updated = true;
                }
                fetched_results.push((id, ver));
            }
        }

        (updated, fetched_results)
    }

    /// 检测一个 CLI 软件并组装状态卡片数据
    fn probe_software(
        id: &str,
        name: &str,
        category: &str,
        description: &str,
        repo_url: &str,
        cli: &str,
        default_method: &str,
        default_channel: &str,
    ) -> SoftwareStatus {
        let (ok, ver, path) = Self::check_command(cli, "--version");
        let method = if ok {
            Self::infer_install_method(path.as_deref(), default_method)
        } else {
            None
        };
        SoftwareStatus {
            id: id.into(),
            name: name.into(),
            category: category.into(),
            description: description.into(),
            repo_url: repo_url.into(),
            installed: ok,
            version: ver,
            latest_version: Self::get_latest_version(id),
            install_method: method,
            default_channel: default_channel.into(),
            // 托管展示字段只有 nodejs 卡片会有值，由 apply_node_ownership / attach_guard_hint 覆盖
            managed_by: None,
            managed_versions: Vec::new(),
            guard_hint: None,
            uninstall_hint: None,
        }
    }

    /// 综合全量实时检测所有环境与 Agent 状态
    pub fn detect_all() -> Vec<SoftwareStatus> {
        // 探测前先刷新进程 PATH 环境变量
        crate::platform::refresh_process_path();

        // Node 所有权取证：**整轮只探这一次**（detect() 背着 pkgutil / npm / node 的子进程
        // 等待，每卡片各调一遍会把一次刷新变成 N 次）。同一份结论既喂 nodejs 卡片的托管展示，
        // 也用来修正 install_method 口径。为什么不在这里 spawn_blocking：本函数是同步的，
        // 而同步函数无论把活儿丢给谁，最终都要把调用线程等到结果出来为止；调用侧的线程
        // 归属结论见 main.rs 与任务 E 的报告（启动首轮在 UI 主线程，Refresh 在 tokio worker）。
        let node_env = node_source::detect();
        // 任务 H 的预览口径开关：mode=system 时护栏整体让位，弹窗也必须回到普通确认样式，
        // 否则前端会拦住一个后端其实会放行的动作。磁盘读一次几 KB，与取证同一量级。
        let node_guards_enabled = crate::config::AppConfig::load().node_guards_enabled();

        let mut list = Vec::new();

        // 1. 基础开发环境 (Prerequisites) —— 按平台差异化
        #[cfg(windows)]
        {
            let (wt_ok, wt_ver, wt_method) = Self::check_windows_terminal();
            list.push(SoftwareStatus {
                id: "windows_terminal".into(),
                name: "Windows Terminal".into(),
                category: "env".into(),
                description: "微软现代终端，支持多标签页、GPU 加速与 Unicode".into(),
                repo_url: "https://github.com/microsoft/terminal".into(),
                installed: wt_ok,
                version: wt_ver,
                latest_version: Self::get_latest_version("windows_terminal"),
                install_method: wt_method,
                default_channel: "WinGet / 微软官方MSIXBundle".into(),
                managed_by: None,
                managed_versions: Vec::new(),
                guard_hint: None,
                uninstall_hint: None,
            });

            list.push(Self::probe_software(
                "powershell7",
                "PowerShell 7",
                "env",
                "跨平台全新 PowerShell，现代脚本与 Agent 执行基础".into(),
                "https://github.com/PowerShell/PowerShell",
                "pwsh.exe",
                "msi",
                "GitHub加速镜像 / WinGet",
            ));

            let mut node_card = Self::probe_software(
                "nodejs",
                "Node.js (LTS)",
                "env",
                "Claude Code、Codex、Pi 等 Agent 必备的运行环境".into(),
                "https://nodejs.org",
                "node.exe",
                "msi",
                "阿里npmmirror镜像官方MSI",
            );
            Self::apply_node_ownership(&mut node_card, &node_env);
            // Win 的安装与卸载都是 MSI 渠道（`node_channel` 的 cfg!(windows) 分支）
            Self::attach_guard_hints(
                &mut node_card,
                &node_env,
                node_guards_enabled,
                node_source::node_channel(false),
                node_source::node_channel(false),
            );
            list.push(node_card);

            list.push(Self::probe_software(
                "git",
                "Git",
                "env",
                "代码版本管理系统，绝大多数 Agent 的依赖项".into(),
                "https://git-scm.com",
                "git.exe",
                "installer",
                "GitHub加速镜像 / WinGet",
            ));
        }

        #[cfg(target_os = "macos")]
        {
            list.push(Self::probe_software(
                "homebrew",
                "Homebrew",
                "env",
                "macOS 事实标准包管理器，托管 Node.js / Git 等工具链".into(),
                "https://brew.sh",
                "brew",
                "script",
                "官方脚本 / 清华镜像加速",
            ));

            let mut node_card = Self::probe_software(
                "nodejs",
                "Node.js (LTS)",
                "env",
                "Claude Code、Codex、Pi 等 Agent 必备的运行环境".into(),
                "https://nodejs.org",
                "node",
                "pkg",
                "Homebrew / 阿里npmmirror官方PKG",
            );
            Self::apply_node_ownership(&mut node_card, &node_env);
            // 两个渠道与真点击时的路由各自同源：安装看 `brew_available()`（which，无进程），
            // 卸载看 `brew_installed_formula("node")`（`uninstall_target_macos` 用的就是它）。
            // brew 子进程只在护栏开启且 brew 在场时跑一次；关着就不为预览花钱。
            let brew_present = crate::platform::which("brew").is_some();
            let brew_route_uninstall = node_guards_enabled
                && brew_present
                && crate::installer::macos::MacInstaller::brew_installed_formula("node");
            Self::attach_guard_hints(
                &mut node_card,
                &node_env,
                node_guards_enabled,
                node_source::node_channel(brew_present),
                node_source::node_channel(brew_route_uninstall),
            );
            list.push(node_card);

            list.push(Self::probe_software(
                "git",
                "Git",
                "env",
                "代码版本管理系统，绝大多数 Agent 的依赖项".into(),
                "https://git-scm.com",
                "git",
                "system",
                "Homebrew / Xcode 命令行工具",
            ));
        }

        // 2. AI Coding Agents（跨平台一致，check_command 自动处理 .exe 后缀）
        list.push(Self::probe_software(
            "oh-my-pi",
            "oh-my-pi (omp)",
            "agent",
            "带 IDE 级工具与高级编辑功能的终端 AI 编程智能体".into(),
            "https://github.com/can1357/oh-my-pi",
            "omp",
            "native_cli",
            "官方原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "claudecode",
            "Claude Code",
            "agent",
            "Anthropic 官方研究级终端编码智能体 CLI".into(),
            "https://github.com/anthropics/claude-code",
            "claude",
            "native_cli",
            "Anthropic原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "codex",
            "OpenAI Codex",
            "agent",
            "OpenAI 官方开源轻量级终端 Coding Agent".into(),
            "https://github.com/openai/codex",
            "codex",
            "npm",
            "官方npm包 (@openai/codex)",
        ));

        list.push(Self::probe_software(
            "pi",
            "Pi Agent",
            "agent",
            "Earendil Works 统一 LLM 循环与终端交互编程框架".into(),
            "https://github.com/earendil-works/pi",
            "pi",
            "native_cli",
            "官方原生更新器 / npmmirror",
        ));

        list.push(Self::probe_software(
            "opencode",
            "OpenCode",
            "agent",
            "Anomaly 团队开源的高性能轻量 Coding Agent".into(),
            "https://github.com/anomalyco/opencode",
            "opencode",
            "npm",
            "官方CLI自升级 / npmmirror",
        ));

        // Agent 的 npm 前置：与 `InstallerEngine::agent_preflight` 同一判定函数，
        // 缺 node/npm 时卡片随 `guard_hint` 下发指引，点击直接弹指引窗——不再
        // 「先确认、再吃一条 npm 报错」。`detect()` 本轮已跑过（node_env），
        // 只有既没 node 也没管理器时才补一次 `npm --version`（自定义 npm 落点）。
        let npm_available = node_env.active_node.is_some()
            || !node_env.managers.is_empty()
            || Self::check_command("npm", "--version").0;
        for status in list.iter_mut().filter(|s| s.category == "agent") {
            status.guard_hint = crate::installer::agent_npm_guidance(
                &status.id,
                &node_env,
                status.installed,
                npm_available,
            );
        }

        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::node_source::{Manager, ManagerKind, NodeEnv, NodeOwner};
    use std::path::PathBuf;

    /// 构造一个管理器条目：版本按传入目录名（`v26.12.0` 这种形态）清洗后降序排好，
    /// 与真实采集产出的 `Manager` 形状一致，免得测试自己手工维护顺序
    fn mgr(kind: ManagerKind, root: &str, versions: &[&str]) -> Manager {
        let mut vs: Vec<String> = versions.iter().map(|v| v.to_string()).collect();
        vs.sort_by(|a, b| node_source::version_key(b).cmp(&node_source::version_key(a)));
        Manager {
            kind,
            root: PathBuf::from(root),
            versions: vs,
        }
    }

    /// 把期望值折成与 `pick_managed_by` 相同的类型，避免逐字段手写比较
    fn want(label: &'static str, vs: &[&'static str]) -> Option<(&'static str, Vec<String>)> {
        Some((label, vs.iter().map(|v| v.to_string()).collect()))
    }

    /// 主托管方选择的四条规则（D4 / 计划 §2.7），表驱动。
    /// 本机现场就是第一条与第二名的对照：nvm 三个版本 + volta 两个版本并存。
    #[test]
    fn pick_managed_by_table() {
        let nvm = mgr(
            ManagerKind::Nvm,
            "/Users/u/.nvm",
            &["26.12.0", "24.22.0", "16.20.2"],
        );
        let volta = mgr(ManagerKind::Volta, "/Users/u/.volta", &["20.15.0", "16.14.2"]);
        let bare_fnm = mgr(ManagerKind::Fnm, "/Users/u/.fnm", &[]);
        let bare_proto = mgr(ManagerKind::Proto, "/Users/u/.proto", &[]);

        struct Case {
            name: &'static str,
            owner: NodeOwner,
            managers: Vec<Manager>,
            want: Option<(&'static str, Vec<String>)>,
        }

        let cases = [
            // 规则 1：owner 命中那一个，只取那一个
            Case {
                name: "owner=Manager(Nvm)：只报 nvm 的三个版本，volta 一个字符都不混",
                owner: NodeOwner::Manager(ManagerKind::Nvm),
                managers: vec![nvm.clone(), volta.clone()],
                want: want("nvm", &["26.12.0", "24.22.0", "16.20.2"]),
            },
            Case {
                name: "owner=Manager(Volta)：只出 volta 的版本（本机若生效的是 volta 就是这条）",
                owner: NodeOwner::Manager(ManagerKind::Volta),
                managers: vec![nvm.clone(), volta.clone()],
                want: want("Volta", &["20.15.0", "16.14.2"]),
            },
            // 规则 2：owner 定位不到但有管理器 → 版本最高的那个当主，其余不进 managed_by
            Case {
                name: "owner=None：nvm(26.12.0) 压过 volta(20.15.0)，标签只有 nvm",
                owner: NodeOwner::None,
                // 故意把 volta 放在清单前面，证明主托管方是按版本选的、不是按顺序
                managers: vec![volta.clone(), nvm.clone()],
                want: want("nvm", &["26.12.0", "24.22.0", "16.20.2"]),
            },
            Case {
                name: "owner=Unknown：与 None 同规则",
                owner: NodeOwner::Unknown,
                managers: vec![volta.clone(), nvm.clone()],
                want: want("nvm", &["26.12.0", "24.22.0", "16.20.2"]),
            },
            Case {
                name: "版本按数值比、不按字典序（9.x 不得压过 26.x）",
                owner: NodeOwner::None,
                managers: vec![
                    mgr(ManagerKind::Mise, "/Users/u/.local/share/mise", &["9.9.9"]),
                    mgr(ManagerKind::Asdf, "/Users/u/.asdf", &["26.0.0"]),
                ],
                want: want("asdf", &["26.0.0"]),
            },
            Case {
                name: "段数少的视为更小（24.22.0 > 24）",
                owner: NodeOwner::None,
                managers: vec![
                    mgr(ManagerKind::N, "/usr/local", &["24"]),
                    mgr(ManagerKind::Scoop, "/Users/u/scoop/apps/nodejs", &["24.22.0"]),
                ],
                want: want("Scoop", &["24.22.0"]),
            },
            // 规则 3：所有管理器都没版本 → 取清单第一个，版本为空
            Case {
                name: "全无版本：退回清单第一个（fnm），managed_versions 为空",
                owner: NodeOwner::None,
                managers: vec![bare_fnm.clone(), bare_proto.clone()],
                want: want("fnm", &[]),
            },
            Case {
                name: "有版本的排在后面也一样：有版本者优先于清单顺序",
                owner: NodeOwner::Unknown,
                managers: vec![bare_fnm.clone(), volta.clone()],
                want: want("Volta", &["20.15.0", "16.14.2"]),
            },
            // 规则 4：没有管理器 → None
            Case {
                name: "无管理器：None（老 config 机器 / 裸 MSI 机器就是这个形态）",
                owner: NodeOwner::None,
                managers: vec![],
                want: None,
            },
            // 防御与不托管的两种归属
            Case {
                name: "owner=Manager(Nvm) 但清单里没有 nvm：不给假标签",
                owner: NodeOwner::Manager(ManagerKind::Nvm),
                managers: vec![volta.clone()],
                want: None,
            },
            Case {
                name: "owner=Brew：不算托管（那份 runtime DevUp 能直接操作）",
                owner: NodeOwner::Brew,
                managers: vec![nvm.clone()],
                want: None,
            },
            Case {
                name: "owner=SystemPkg：不算托管",
                owner: NodeOwner::SystemPkg,
                managers: vec![nvm.clone()],
                want: None,
            },
        ];

        for c in cases {
            let got = Detector::pick_managed_by(c.owner, &c.managers);
            assert_eq!(got, c.want, "用例失败: {}", c.name);
        }
    }

    /// 任务 H：`mode=system` 让位时两个预览必须一起撤回（跨平台成立——开关优先于渠道，
    /// 开着时哪个渠道被拒随平台而异，内容断言在下方 mac 专项用例里钉）
    #[test]
    fn attach_guard_hints_respects_escape_switch() {
        let env = node_source::NodeEnv {
            owner: node_source::NodeOwner::Manager(node_source::ManagerKind::Nvm),
            managers: vec![mgr(node_source::ManagerKind::Nvm, "/Users/u/.nvm", &["26.12.0"])],
            ..Default::default()
        };
        let mut s = status("nodejs", true, Some("nvm"), Some("nvm"), &["26.12.0"]);
        Detector::attach_guard_hints(
            &mut s,
            &env,
            false,
            node_source::node_channel(false),
            node_source::node_channel(false),
        );
        assert_eq!(s.guard_hint, None, "逃生口开着还出预览 = 前端拦下后端会放行的动作");
        assert_eq!(s.uninstall_hint, None, "卸载侧同理");
    }

    /// 任务 H 的 mac 侧内容断言（安装 + 卸载两个预览）。渠道不写死结论：
    /// `node_channel(false)` 在 mac 即 MacPkg——nvm 在场必拒（G4）、无 receipt 的卸载必拒（G5）；
    /// Windows 主机同环境走 WindowsMsi（无 reparse 证据 → 都放行，按 D2 不误拦），故整条用例锁 mac。
    #[cfg(target_os = "macos")]
    #[test]
    fn attach_guard_hints_macos_carries_manager_guidance() {
        let env = node_source::NodeEnv {
            owner: node_source::NodeOwner::Manager(node_source::ManagerKind::Nvm),
            managers: vec![mgr(node_source::ManagerKind::Nvm, "/Users/u/.nvm", &["26.12.0"])],
            version: Some("26.12.0".into()),
            ..Default::default()
        };
        let mut s = status("nodejs", true, Some("nvm"), Some("nvm"), &["26.12.0"]);
        Detector::attach_guard_hints(
            &mut s,
            &env,
            true,
            node_source::node_channel(false),
            node_source::node_channel(false),
        );
        let g = s.guard_hint.expect("mac 上 nvm 托管机器应给出安装指引预览");
        assert!(g.reason.contains("nvm") && g.reason.contains("/Users/u/.nvm"));
        assert!(
            g.commands.iter().any(|c| c.starts_with("nvm install")),
            "命令形态必须来自 guard_commands: {:?}",
            g.commands
        );
        assert!(g.note.is_some());
        let u = s.uninstall_hint.expect("无 receipt 的托管机器卸载也该是指引而非确认");
        assert!(u.reason.contains("receipt"), "卸载预览要讲清 G5 依据: {}", u.reason);
        assert!(
            u.commands.iter().any(|c| c.starts_with("nvm uninstall")),
            "卸载预览的命令必须是移除形态: {:?}",
            u.commands
        );

        // 有 receipt 的机器：卸载真会执行 → `uninstall_hint` 必须留 None（确认窗保留）
        let mut s2 = status("nodejs", true, Some("nvm"), Some("nvm"), &["26.12.0"]);
        let receipt_env = node_source::NodeEnv {
            pkg_receipt: true,
            ..env.clone()
        };
        Detector::attach_guard_hints(
            &mut s2,
            &receipt_env,
            true,
            node_source::node_channel(false),
            node_source::node_channel(false),
        );
        assert_eq!(
            s2.uninstall_hint, None,
            "卸载放行时不得伪装成指引——那是真删，确认步骤是用户唯一防线"
        );
        assert!(s2.guard_hint.is_some(), "receipt 不改变安装侧 G4 拒绝");
    }

    /// 线上一行 JSON 的形状：新增两字段必须蛇形命名、排在 default_channel 之后，
    /// `managed_by` 为 None 时序列化成显式 null（不是字段消失），前端据此判空。
    /// 手法与任务 C 的 Guidance 快照同源：拿真实序列化结果钉死，而不是比对结构体。
    /// 测试本机的卡片构造器：不借 `probe_software`（那会起真实子进程、结果随本机环境漂移）
    fn status(
        id: &str,
        installed: bool,
        install_method: Option<&str>,
        managed_by: Option<&str>,
        managed_versions: &[&str],
    ) -> SoftwareStatus {
        SoftwareStatus {
            id: id.into(),
            name: "Node.js (LTS)".into(),
            category: "env".into(),
            description: "d".into(),
            repo_url: "https://nodejs.org".into(),
            installed,
            version: None,
            latest_version: Some("v24.21.0".into()),
            install_method: install_method.map(str::to_string),
            default_channel: "c".into(),
            managed_by: managed_by.map(str::to_string),
            managed_versions: managed_versions.iter().map(|v| v.to_string()).collect(),
            guard_hint: None,
            uninstall_hint: None,
        }
    }

    /// 任务 H 之后 `guard_hint` 进入线上形状：托管卡片必须能带出嵌套对象
    /// （reason/commands/note 三个蛇形键），这条快照同时钉住前端读法。
    #[test]
    fn test_managed_fields_json_snapshot() {
        let mut status = status(
            "nodejs",
            false,
            Some("nvm"),
            Some("nvm"),
            &["26.12.0", "24.22.0", "16.20.2"],
        );
        status.guard_hint = Some(node_source::Guidance {
            reason: "当前 Node.js 由 nvm（/Users/u/.nvm）托管。".into(),
            commands: vec!["nvm install 24.22.0".into()],
            note: None,
        });
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            json,
            r#"{"id":"nodejs","name":"Node.js (LTS)","category":"env","description":"d","repo_url":"https://nodejs.org","installed":false,"version":null,"latest_version":"v24.21.0","install_method":"nvm","default_channel":"c","managed_by":"nvm","managed_versions":["26.12.0","24.22.0","16.20.2"],"guard_hint":{"reason":"当前 Node.js 由 nvm（/Users/u/.nvm）托管。","commands":["nvm install 24.22.0"],"note":null},"uninstall_hint":null}"#
        );
    }

    /// 未托管的卡片（含 Agent 卡片）也要有稳定形状：显式 null + 空数组
    #[test]
    fn test_managed_fields_json_snapshot_unmanaged() {
        let status = status("git", true, Some("brew"), None, &[]);
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.ends_with(r#""managed_by":null,"managed_versions":[],"guard_hint":null,"uninstall_hint":null}"#),
            "实际: {json}"
        );
    }

    /// 老形状（缺这两个字段）必须还能反序列化：字段带 `#[serde(default)]`，
    /// 前端/M1 的回放路径不会因为多两个字段就整体解析失败
    #[test]
    fn test_managed_fields_deserialize_without_them() {
        let legacy = r#"{"id":"nodejs","name":"Node.js (LTS)","category":"env","description":"d",
            "repo_url":"https://nodejs.org","installed":true,"version":"26.12.0",
            "latest_version":"v24.21.0","install_method":"nvm","default_channel":"c"}"#;
        let parsed: SoftwareStatus = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.managed_by, None);
        assert!(parsed.managed_versions.is_empty());
    }

    /// install_method 口径修正 + `installed` 判定不受影响（D4 只改呈现，不改事实判定）
    #[test]
    fn apply_node_ownership_scopes_method_to_owner() {
        let nvm = mgr(
            ManagerKind::Nvm,
            "/Users/u/.nvm",
            &["26.12.0", "24.22.0", "16.20.2"],
        );
        let volta = mgr(ManagerKind::Volta, "/Users/u/.volta", &["20.15.0", "16.14.2"]);

        // 本机从 Finder 启动的形态：GUI 里没有 node，但 nvm 明明装着
        let mut gui = status("nodejs", false, None, None, &[]);
        let env = NodeEnv {
            owner: NodeOwner::Manager(ManagerKind::Nvm),
            managers: vec![nvm.clone(), volta.clone()],
            ..Default::default()
        };
        Detector::apply_node_ownership(&mut gui, &env);
        assert_eq!(gui.managed_by.as_deref(), Some("nvm"));
        assert_eq!(gui.managed_versions, vec!["26.12.0", "24.22.0", "16.20.2"]);
        // 谎报「npm 装的」这条老口径必须不再出现在 Node 卡片上
        assert_eq!(gui.install_method.as_deref(), Some("nvm"));
        // D4 不改事实判定：能否执行到 node 仍然是 false
        assert!(!gui.installed);

        // brew 生效、nvm 也装着：卡片说的是 brew，不是「由 nvm 托管」
        let mut brew = status("nodejs", true, Some("brew"), None, &[]);
        let brew_env = NodeEnv {
            owner: NodeOwner::Brew,
            managers: vec![nvm.clone()],
            ..Default::default()
        };
        Detector::apply_node_ownership(&mut brew, &brew_env);
        assert_eq!(brew.managed_by, None);
        assert!(brew.managed_versions.is_empty());
        assert_eq!(brew.install_method.as_deref(), Some("brew"));

        // 归属不明 + 系统包：install_method 保留原推断，不被覆盖成假标签
        let mut unknown = status("nodejs", true, Some("pkg"), None, &[]);
        let unknown_env = NodeEnv {
            owner: NodeOwner::Unknown,
            managers: vec![volta.clone()],
            ..Default::default()
        };
        Detector::apply_node_ownership(&mut unknown, &unknown_env);
        // owner=Unknown 时按规则 2 仍要给一个主托管方（volta 是唯一有版本的管理器）
        assert_eq!(unknown.managed_by.as_deref(), Some("Volta"));
        assert_eq!(unknown.managed_versions, vec!["20.15.0", "16.14.2"]);
        // 但渠道那栏不改：Unknown 不是「认出了管理器」，而是「认不出」
        assert_eq!(unknown.install_method.as_deref(), Some("pkg"));
    }

    #[test]
    fn test_clean_version_string() {
        assert_eq!(Detector::clean_version_string("2.1.303 (Claude Code)"), "2.1.303");
        assert_eq!(Detector::clean_version_string("codex-cli 0.163.0"), "0.163.0");
        assert_eq!(Detector::clean_version_string("omp/18.12.2"), "18.12.2");
        assert_eq!(Detector::clean_version_string("git version 2.55.0.windows.3"), "2.55.0");
        assert_eq!(Detector::clean_version_string("v24.21.0"), "24.21.0");
        assert_eq!(Detector::clean_version_string("Homebrew 4.5.11"), "4.5.11");
    }

    /// D4 的本机验收口径：真实 `detect()` 结果上打印卡片会用的三个值
    /// `cargo test -- --ignored --nocapture dump_managed_display`
    ///
    /// 本机预期（计划 §2.7 的数据点）：owner=Manager(Nvm) → `managed_by = "nvm"`、
    /// `managed_versions = ["26.12.0","24.22.0","16.20.2"]`，**不含** volta 的两个版本；
    /// `installed` 取决于本次执行环境能否真的跑起 node（从终端跑就是 true，
    /// Finder 启动的 GUI 场景是 false——那正是 D4 要用「由 nvm 托管」取代「未安装」的现场）。
    /// 附带打印一次 detect_all 的用时：detect() 内含子进程等待，这个数字决定刷新路径的观感。
    #[test]
    #[ignore]
    fn dump_managed_display() {
        let started = std::time::Instant::now();
        let list = Detector::detect_all();
        let elapsed = started.elapsed();

        let card = list
            .iter()
            .find(|s| s.id == "nodejs")
            .expect("detect_all 未产出 nodejs 卡片");
        println!("(managed_by, managed_versions, installed) = ({:?}, {:?}, {})",
            card.managed_by, card.managed_versions, card.installed);
        println!("  install_method = {:?}  version = {:?}", card.install_method, card.version);
        println!("  detect_all() 用时 {elapsed:?}（{count} 张卡片，其中 node 取证 1 次）",
            count = list.len());
    }

    /// 人工核对全量探测结果：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_detect_all() {
        for s in Detector::detect_all() {
            println!(
                "[{}] {} installed={} ver={:?} latest={:?} method={:?} channel={}",
                s.category, s.name, s.installed, s.version, s.latest_version, s.install_method, s.default_channel
            );
        }
    }
}
