//! GitHub / GitLab 账号与凭据管理。
//!
//! 设计约束：
//! - `config.json` 只保存 host / kind / username，**token 永不落盘到配置**；
//! - token 写入系统凭据库（Windows Credential Manager / macOS Keychain），
//!   非 Windows/macOS 平台仅保存在进程内存（测试用，重启即失效）；
//! - 认证头只注入到账号 host 自身的请求。ghproxy 等镜像前缀属于第三方域名，
//!   永远不带 token，避免把凭据交给加速节点；
//! - 401 表示已有凭据失效，必须重新登录；403/429 只在尚未配置凭据时提示
//!   （通常是匿名 API 限流），避免把权限不足误判成「没登录」。

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GITHUB_HOST: &str = "github.com";
pub const GITLAB_HOST: &str = "gitlab.com";

/// 系统凭据库里的服务名前缀，条目用户名就是 host
const KEYRING_SERVICE: &str = "devup.git";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Github,
    Gitlab,
}

impl AuthKind {
    /// 用户手动输入域名时的默认推断；无法判断时按 GitHub 处理（当前应用只调用 GitHub API）
    pub fn infer(host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        if host == GITHUB_HOST || host.ends_with(".github.com") {
            Self::Github
        } else if host == GITLAB_HOST || host.contains("gitlab") {
            Self::Gitlab
        } else {
            Self::Github
        }
    }

    /// 账号信息接口：GitHub 官方走 api.github.com，GitHub Enterprise 走 /api/v3
    fn user_endpoint(self, host: &str) -> String {
        match self {
            Self::Github if host.eq_ignore_ascii_case(GITHUB_HOST) => {
                "https://api.github.com/user".to_string()
            }
            Self::Github => format!("https://{host}/api/v3/user"),
            Self::Gitlab => format!("https://{host}/api/v4/user"),
        }
    }

    /// 统一走 Authorization 头：GitHub PAT 与 GitLab PAT 都支持 Bearer。
    /// 不用 GitLab 的 PRIVATE-TOKEN 头——reqwest 只在跨域重定向时剥离
    /// Authorization/Cookie，自定义头会被转发到对象存储，等于把 token 交出去。
    fn auth_header(self, token: &str) -> (&'static str, String) {
        ("authorization", format!("Bearer {token}"))
    }
}

/// 持久化到 `AppConfig` 的账号元数据（不含 token）
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitAccount {
    pub host: String,
    pub kind: AuthKind,
    /// 登录时由服务端返回，仅用于界面展示
    pub username: String,
    /// 登录前该 host 的 credential helper 配置；退出登录时原样恢复。
    /// 只在 DevUp 首次接管该 host 时写入，重复登录不清空。
    #[serde(default)]
    pub git_helper_backup: Vec<String>,
}

/// 下发给前端的账号视图：凭据是否存在以系统凭据库的实时读取为准
#[derive(Clone, Debug, Serialize)]
pub struct GitAccountView {
    pub host: String,
    pub kind: AuthKind,
    pub username: String,
    pub has_token: bool,
    /// 在线校验结果：None = 未校验（无凭据 / 网络不可达），Some(false) = 服务端拒绝。
    /// 只看本地 keyring 是否有值无法区分「登录过期」，所以配置页打开时会补一次校验。
    pub valid: Option<bool>,
    /// `git credential fill` 能从系统 helper（GCM / osxkeychain / store / gh 等）取到完整凭据。
    /// 这类登录不归 DevUp 管，但 clone / pull / push 已经能用，界面不能谎报「未登录」。
    pub system_ready: bool,
    /// true = 由 DevUp 接管（config 里有账号，helper 指向本程序）；false = 系统 Git 原有登录
    pub managed: bool,
}

/// Token 校验失败的两类结果：区分「凭据被拒」与「网络不可达」，
/// 前者要把账号标成失效，后者不能冤枉用户的 token。
#[derive(Debug)]
pub enum VerifyError {
    Rejected(String),
    Unavailable(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(msg) | Self::Unavailable(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for VerifyError {}

/// 需要登录时随事件下发的引导信息
#[derive(Clone, Debug, Serialize)]
pub struct AuthChallenge {
    pub host: String,
    pub kind: AuthKind,
    pub message: String,
}

static ACCOUNTS: LazyLock<RwLock<Vec<GitAccount>>> = LazyLock::new(|| RwLock::new(Vec::new()));
/// 本轮运行已经提示过的 host：同一次启动内不重复弹窗，登录成功后再清掉
static CHALLENGED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 配置加载 / 更新后同步内存账号表；所有请求线程共用这份快照
pub fn sync_accounts(accounts: &[GitAccount]) {
    if let Ok(mut guard) = ACCOUNTS.write() {
        *guard = accounts.to_vec();
    }
}

pub fn account_views() -> Vec<GitAccountView> {
    let accounts = ACCOUNTS
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    accounts
        .into_iter()
        .map(|a| GitAccountView {
            has_token: secret_store::get(&a.host).is_some(),
            host: a.host,
            kind: a.kind,
            username: a.username,
            valid: None,
            system_ready: false,
            managed: true,
        })
        .collect()
}

/// 已由 DevUp 接管登录的 host（配置里有账号）。这些 host 的 helper 指向本程序，
/// 系统里即使还有旧凭据也不会被 git 使用，所以不做系统检测。
pub fn is_managed(host: &str) -> bool {
    find_account(host).is_some()
}

static EXTRA_DETECT_HOSTS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// 记住用户手动检测过且命中的 host，后续刷新也会纳入系统登录列表
pub fn remember_detected_host(host: &str) {
    if let Ok(mut hosts) = EXTRA_DETECT_HOSTS.lock() {
        hosts.insert(host.to_ascii_lowercase());
    }
}

/// 检测系统 Git 是否已保存该 host 的完整凭据，返回用户名（密码只用于判断完整性，不保留）。
///
/// 关键约束：禁止交互。`GIT_TERMINAL_PROMPT=0` + `credential.interactive=never` +
/// `GCM_INTERACTIVE=Never`，最后还有 3 秒硬超时并 kill，避免打开 GCM 浏览器授权窗。
pub fn detect_system_credential(host: &str) -> Option<String> {
    detect_system_credential_at(host, None)
}

fn detect_system_credential_at(host: &str, config_global: Option<&Path>) -> Option<String> {
    let host = normalize_host(host).ok()?;
    if find_account(&host).is_some() {
        return None;
    }

    let mut cmd = Command::new("git");
    cmd.args(["-c", "credential.interactive=never", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        // 双保险：GCM 新老版本的交互开关 + 缩短它自动探测 host provider 的网络等待
        .env("GCM_GUI_PROMPT", "0")
        .env("GCM_DETECT_TIMEOUT", "2")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(path) = config_global {
        cmd.env("GIT_CONFIG_GLOBAL", path);
    }
    crate::platform::hide_console(&mut cmd);
    let mut child = cmd.spawn().ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if stdin
            .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())
            .is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        // stdin 在此 drop，向 git 发送 EOF
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(40));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut username = String::new();
    let mut has_password = false;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("username=") {
            username = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("password=") {
            has_password = !value.trim().is_empty();
        }
    }
    if !has_password {
        return None;
    }
    Some(username)
}

/// 并发检测默认 host（github.com / gitlab.com）与用户手动检测过的 host。
/// 仅返回有完整系统凭据的项；已由 DevUp 接管的 host 被跳过。
pub fn detect_system_accounts() -> Vec<(String, AuthKind, String)> {
    let mut hosts = vec![GITHUB_HOST.to_string(), GITLAB_HOST.to_string()];
    if let Ok(extra) = EXTRA_DETECT_HOSTS.lock() {
        for host in extra.iter() {
            if !hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
                hosts.push(host.clone());
            }
        }
    }

    std::thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .into_iter()
            .map(|host| {
                scope.spawn(move || {
                    detect_system_credential(&host)
                        .map(|username| (host.clone(), AuthKind::infer(&host), username))
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect()
    })
}

/// 完整账号视图：DevUp 接管的账号 + 系统 Git 已有登录。
/// 含进程/子进程调用，必须在阻塞线程里执行。
pub fn account_views_with_system() -> Vec<GitAccountView> {
    let mut views = account_views();
    for (host, kind, username) in detect_system_accounts() {
        if views.iter().any(|v| v.host.eq_ignore_ascii_case(&host)) {
            continue;
        }
        views.push(GitAccountView {
            host,
            kind,
            username,
            has_token: false,
            valid: None,
            system_ready: true,
            managed: false,
        });
    }
    views
}

fn find_account(host: &str) -> Option<GitAccount> {
    ACCOUNTS
        .read()
        .ok()?
        .iter()
        .find(|a| a.host.eq_ignore_ascii_case(host))
        .cloned()
}

/// 用户输入的域名（可带 scheme / 路径）归一为小写 host；只接受 HTTPS
pub fn normalize_host(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("请填写域名");
    }
    let raw = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url = reqwest::Url::parse(&raw).context("域名格式不正确")?;
    if url.scheme() != "https" {
        bail!("仅支持 HTTPS 域名（Token 不能走明文 HTTP）");
    }
    let host = url
        .host_str()
        .context("域名缺少主机名")?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if host.is_empty() {
        bail!("域名缺少主机名");
    }
    // 自建 GitLab 常用非 443 端口，账号按 host:port 精确匹配，不能把端口吞掉
    Ok(match url.port().filter(|p| *p != 443) {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// 把请求 URL 的 host 归一为「账号 host」：GitHub 的 API/下载子域都归 github.com。
/// 其它域名（含自建 GitLab/GitHub Enterprise、镜像前缀）原样返回，按精确匹配处理。
pub fn account_host_for_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    match host {
        "github.com" | "api.github.com" | "codeload.github.com" | "uploads.github.com" => {
            Some(GITHUB_HOST.to_string())
        }
        _ => Some(match parsed.port().filter(|p| *p != 443) {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        }),
    }
}

pub fn account_for_url(url: &str) -> Option<GitAccount> {
    let host = account_host_for_url(url)?;
    find_account(&host)
}

/// 给指向已登录 host 的请求注入认证头；镜像/第三方域名原样返回
pub fn authorize(builder: RequestBuilder, url: &str) -> RequestBuilder {
    let Some(account) = account_for_url(url) else {
        return builder;
    };
    let Some(token) = secret_store::get(&account.host) else {
        return builder;
    };
    let (name, value) = account.kind.auth_header(&token);
    builder.header(name, value)
}

/// 用 /user 接口验证 token 并返回账号名。验证通过前不会写入凭据库。
pub async fn verify_token(
    client: &Client,
    host: &str,
    kind: AuthKind,
    token: &str,
) -> std::result::Result<String, VerifyError> {
    let url = kind.user_endpoint(host);
    let (header_name, header_value) = kind.auth_header(token);
    let resp = client
        .get(&url)
        .header(header_name, header_value)
        .header("User-Agent", "DevUp-Client")
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|_| VerifyError::Unavailable(format!("无法连接 {url}，请检查域名与网络")))?;

    let status = resp.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(VerifyError::Rejected(format!(
            "Token 无效或无权限（HTTP {}）",
            status.as_u16()
        )));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(VerifyError::Rejected(
            "接口不存在（HTTP 404），请确认域名与服务类型是否匹配".to_string(),
        ));
    }
    if !status.is_success() {
        return Err(VerifyError::Unavailable(format!(
            "验证失败（HTTP {}）",
            status.as_u16()
        )));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|_| VerifyError::Unavailable("账号信息解析失败".to_string()))?;
    let username = match kind {
        AuthKind::Github => json.get("login"),
        AuthKind::Gitlab => json.get("username"),
    }
    .and_then(Value::as_str)
    .filter(|s| !s.is_empty())
    .unwrap_or(host)
    .to_string();

    Ok(username)
}

pub fn store_token(host: &str, token: &str) -> Result<()> {
    secret_store::set(host, token.trim())
}

/// 仅供 crate 内部做在线校验：调用方拿不到 token 引用之外的持久化副本
pub fn token_for(host: &str) -> Option<String> {
    secret_store::get(host)
}

pub fn delete_token(host: &str) {
    secret_store::delete(host);
}

// ---- Git 凭据助手：让终端里的 git clone / pull / push 直接复用 DevUp 的 Token ----

/// `devup git-credential <get|store|erase>` 的入口。
///
/// 返回 `Some(exit_code)` 表示当前进程是 Git 调用的凭据助手，调用方必须直接退出，
/// 不能初始化窗口 / Tokio：helper 是同步、短命、stdin→stdout 的子进程。
pub fn run_git_credential_helper() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next()?;
    if command.to_string_lossy() != "git-credential" {
        return None;
    }
    let op = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !matches!(op.as_str(), "get" | "store" | "erase") {
        return Some(2);
    }

    // 助手进程独立于 GUI，必须从磁盘重读账号配置
    let cfg = crate::config::AppConfig::load();
    sync_accounts(&cfg.git_accounts);

    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return Some(1);
    }

    if op == "get" {
        let accounts = ACCOUNTS.read().map(|g| g.clone()).unwrap_or_default();
        if let Some(answer) = credential_answer_with(&input, &accounts, secret_store::get) {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(answer.as_bytes());
            let _ = out.flush();
        }
    }
    // store / erase：凭据生命周期由 DevUp 的登录/退出流程管理。
    // 这里若跟随 Git 的 erase 删除 Token，一次输错密码就会把已保存的凭据清掉。
    Some(0)
}

#[derive(Debug, PartialEq, Eq)]
struct CredentialRequest {
    protocol: String,
    host: String,
}

fn parse_credential_request(input: &str) -> Option<CredentialRequest> {
    let mut protocol = None;
    let mut host = None;
    for line in input.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once('=')?;
        match key {
            "protocol" => protocol = Some(value.trim().to_string()),
            "host" => host = Some(value.trim().to_string()),
            _ => {}
        }
    }
    Some(CredentialRequest {
        protocol: protocol?,
        host: host?,
    })
}

/// 生成 Git credential helper 的应答。只对 HTTPS 且已在 DevUp 登录的 host 返回凭据；
/// 镜像域名、API 子域（api.github.com）不在账号表里，因此永远不会拿到 Token。
fn credential_answer_with(
    input: &str,
    accounts: &[GitAccount],
    token_for: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let req = parse_credential_request(input)?;
    if !req.protocol.eq_ignore_ascii_case("https") {
        return None;
    }
    let host = normalize_host(&req.host).ok()?;
    let account = accounts
        .iter()
        .find(|a| a.host.eq_ignore_ascii_case(&host))?;
    let token = token_for(&account.host)?;

    let username = match account.kind {
        AuthKind::Github if account.username.trim().is_empty() => "x-access-token",
        AuthKind::Gitlab if account.username.trim().is_empty() => "oauth2",
        _ => account.username.trim(),
    };
    Some(format!("username={username}\npassword={token}\n\n"))
}

fn helper_key(host: &str) -> String {
    // 按 URL 精确限定，不覆盖其它 host 的全局 helper（如 GCM）
    format!("credential.https://{host}.helper")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn helper_command() -> Result<String> {
    let exe = std::env::current_exe().context("无法定位 DevUp 可执行文件")?;
    // Git for Windows / macOS 都通过 sh 执行 `!` helper，路径统一成正斜杠并用单引号包裹
    let path = exe.to_string_lossy().replace('\\', "/");
    Ok(format!("!{} git-credential", shell_quote(&path)))
}

fn is_devup_helper(value: &str) -> bool {
    value.contains("git-credential")
        && (value.contains("devup") || value.contains("DevUp"))
}

fn run_git_config(
    config_global: Option<&Path>,
    args: &[&str],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(path) = config_global {
        cmd.env("GIT_CONFIG_GLOBAL", path);
    }
    crate::platform::hide_console(&mut cmd);
    cmd.output().context("无法执行 git，请先安装 Git 后重试")
}

fn git_config_get_all_at(key: &str, config_global: Option<&Path>) -> Vec<String> {
    let Ok(out) = run_git_config(config_global, &["config", "--global", "--get-all", key])
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

/// 把当前 host 的 Git credential helper 指向 DevUp。
///
/// 先写入空值重置该 URL 的 helper 链，再写 DevUp：否则 Git 会先调用用户原有的
/// GCM 并可能弹出浏览器登录。返回 `Some(原配置)` 表示本次是首次接管，
/// 调用方应持久化这份备份供退出登录时恢复；`None` 表示之前已指向 DevUp。
pub fn install_git_helper(host: &str) -> Result<Option<Vec<String>>> {
    install_git_helper_at(host, None)
}

fn install_git_helper_at(
    host: &str,
    config_global: Option<&Path>,
) -> Result<Option<Vec<String>>> {
    let key = helper_key(host);
    let command = helper_command()?;
    let before = git_config_get_all_at(&key, config_global);
    let already = before
        .iter()
        .any(|value| is_devup_helper(value) || value.trim() == command.trim());
    let backup = if already { None } else { Some(before) };

    let _ = run_git_config(
        config_global,
        &["config", "--global", "--unset-all", key.as_str()],
    )?;
    for value in ["", command.as_str()] {
        let out = run_git_config(
            config_global,
            &["config", "--global", "--add", key.as_str(), value],
        )?;
        if !out.status.success() {
            bail!(
                "写入 Git 凭据助手配置失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(backup)
}

pub fn uninstall_git_helper(host: &str, backup: &[String]) -> Result<()> {
    uninstall_git_helper_at(host, backup, None)
}

fn uninstall_git_helper_at(
    host: &str,
    backup: &[String],
    config_global: Option<&Path>,
) -> Result<()> {
    let key = helper_key(host);
    let _ = run_git_config(
        config_global,
        &["config", "--global", "--unset-all", key.as_str()],
    )?;
    for value in backup {
        let out = run_git_config(
            config_global,
            &["config", "--global", "--add", key.as_str(), value],
        )?;
        if !out.status.success() {
            bail!(
                "恢复 Git 凭据助手配置失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// 判断一个失败响应是否应触发登录引导；返回 `None` 表示不弹窗。
///
/// 去重范围是「进程内每个 host 一次」：用户主动关掉弹窗后，同一次运行里的重复
/// 刷新不会反复打扰；重新登录成功后由 `clear_challenge` 解除。
pub fn challenge(url: &str, status: StatusCode) -> Option<AuthChallenge> {
    let code = status.as_u16();
    if !matches!(code, 401 | 403 | 429) {
        return None;
    }
    let host = account_host_for_url(url)?;
    let account = find_account(&host);
    // 已配置凭据时的 403/429 多半是权限或限流，不是「未登录」，交给调用方报错
    if code != 401 && account.is_some() {
        return None;
    }

    let kind = account
        .as_ref()
        .map(|a| a.kind)
        .unwrap_or_else(|| AuthKind::infer(&host));
    let mut seen = CHALLENGED.lock().ok()?;
    if !seen.insert(host.clone()) {
        return None;
    }

    let message = if code == 401 {
        "Git 凭据已失效，请重新登录后继续。".to_string()
    } else {
        "GitHub 匿名请求被限流；登录后版本检测与终端 git 操作都可继续（Token 只保存在系统凭据库）。"
            .to_string()
    };

    Some(AuthChallenge {
        host,
        kind,
        message,
    })
}

pub fn clear_challenge(host: &str) {
    if let Ok(mut seen) = CHALLENGED.lock() {
        seen.remove(host);
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod secret_store {
    use anyhow::{Context, Result};

    pub fn get(host: &str) -> Option<String> {
        let entry = keyring::Entry::new(super::KEYRING_SERVICE, host).ok()?;
        match entry.get_password() {
            Ok(token) if !token.trim().is_empty() => Some(token),
            _ => None,
        }
    }

    pub fn set(host: &str, token: &str) -> Result<()> {
        if token.is_empty() {
            anyhow::bail!("Token 不能为空");
        }
        let entry = keyring::Entry::new(super::KEYRING_SERVICE, host)
            .context("打开系统凭据库失败")?;
        entry.set_password(token).context("写入系统凭据库失败")
    }

    pub fn delete(host: &str) {
        if let Ok(entry) = keyring::Entry::new(super::KEYRING_SERVICE, host) {
            let _ = entry.delete_credential();
        }
    }
}

/// 其它平台没有接入系统凭据库：只保留进程内存，避免测试/交叉编译引入额外依赖。
/// 生产构建只发布 Windows / macOS，不会走到这里。
#[cfg(not(any(windows, target_os = "macos")))]
mod secret_store {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use anyhow::Result;

    static MEMORY: LazyLock<Mutex<HashMap<String, String>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub fn get(host: &str) -> Option<String> {
        MEMORY.lock().ok()?.get(host).cloned()
    }

    pub fn set(host: &str, token: &str) -> Result<()> {
        if token.is_empty() {
            anyhow::bail!("Token 不能为空");
        }
        MEMORY
            .lock()
            .map_err(|_| anyhow::anyhow!("凭据内存表已损坏"))?
            .insert(host.to_string(), token.to_string());
        Ok(())
    }

    pub fn delete(host: &str) {
        if let Ok(mut guard) = MEMORY.lock() {
            guard.remove(host);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_service_hosts_map_to_account_host() {
        for url in [
            "https://github.com/a/b/releases/download/v1/x.exe",
            "https://api.github.com/repos/a/b/releases/latest",
            "https://codeload.github.com/a/b/zip/refs/heads/main",
            "https://uploads.github.com/foo",
        ] {
            assert_eq!(
                account_host_for_url(url).as_deref(),
                Some(GITHUB_HOST),
                "{url}"
            );
        }
    }

    #[test]
    fn third_party_and_self_hosted_hosts_stay_exact() {
        assert_eq!(
            account_host_for_url("https://ghproxy.net/https://github.com/a/b").as_deref(),
            Some("ghproxy.net")
        );
        assert_eq!(
            account_host_for_url("https://git.corp.example.com/api/v4/user").as_deref(),
            Some("git.corp.example.com")
        );
        assert_eq!(
            account_host_for_url("https://gitlab.com/api/v4/projects").as_deref(),
            Some("gitlab.com")
        );
    }

    #[test]
    fn normalize_host_accepts_bare_and_scheme() {
        assert_eq!(normalize_host("github.com").unwrap(), "github.com");
        assert_eq!(
            normalize_host("https://WWW.GitLab.com/foo").unwrap(),
            "gitlab.com"
        );
        assert_eq!(
            normalize_host("  git.corp.example.com  ").unwrap(),
            "git.corp.example.com"
        );
    }

    #[test]
    fn normalize_host_rejects_plain_http_and_empty() {
        assert!(normalize_host("http://git.corp.example.com").is_err());
        assert!(normalize_host("   ").is_err());
        assert!(normalize_host("ftp://github.com").is_err());
    }

    #[test]
    fn kind_inference_defaults_to_github_for_unknown_hosts() {
        assert_eq!(AuthKind::infer("github.com"), AuthKind::Github);
        assert_eq!(AuthKind::infer("api.github.com"), AuthKind::Github);
        assert_eq!(AuthKind::infer("gitlab.com"), AuthKind::Gitlab);
        assert_eq!(AuthKind::infer("gitlab.corp.example.com"), AuthKind::Gitlab);
        assert_eq!(AuthKind::infer("git.corp.example.com"), AuthKind::Github);
    }

    fn account(host: &str, kind: AuthKind, username: &str) -> GitAccount {
        GitAccount {
            host: host.to_string(),
            kind,
            username: username.to_string(),
            git_helper_backup: Vec::new(),
        }
    }

    #[test]
    fn credential_answer_only_for_known_https_hosts() {
        let accounts = vec![
            account("github.com", AuthKind::Github, "alice"),
            account("git.corp.example.com:8443", AuthKind::Gitlab, ""),
        ];
        let answer = |input: &str| {
            credential_answer_with(input, &accounts, |_| Some("tok".to_string()))
        };

        assert_eq!(
            answer("protocol=https\nhost=github.com\n\n").unwrap(),
            "username=alice\npassword=tok\n\n"
        );
        // GitLab 空用户名回退到 oauth2；非默认端口按 host:port 精确匹配
        assert_eq!(
            answer("protocol=https\nhost=git.corp.example.com:8443\n\n").unwrap(),
            "username=oauth2\npassword=tok\n\n"
        );

        // 明文 HTTP、API 子域、未登录 host 都不得返回凭据
        assert_eq!(answer("protocol=http\nhost=github.com\n\n"), None);
        assert_eq!(answer("protocol=https\nhost=api.github.com\n\n"), None);
        assert_eq!(answer("protocol=https\nhost=git.corp.example.com\n\n"), None);
        assert_eq!(answer("protocol=https\nhost=unknown.example.com\n\n"), None);
        assert_eq!(answer("protocol=https\n\n"), None);
    }

    #[test]
    fn detects_system_credential_from_git_helper() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "devup-syscred-{}-{}",
            std::process::id(),
            "test"
        ));
        let _ = std::fs::remove_file(&path);
        let host = "devup-sys.invalid";
        let key = format!("credential.https://{host}.helper");

        let Ok(out) = run_git_config(
            Some(&path),
            &["config", "--global", "--add", key.as_str(), ""],
        ) else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let with_secret = "!f() { echo username=sys-user; echo password=dummy-pass; }; f";
        let out = run_git_config(
            Some(&path),
            &["config", "--global", "--add", key.as_str(), with_secret],
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(
            detect_system_credential_at(host, Some(&path)).as_deref(),
            Some("sys-user")
        );

        // 只有用户名没有密码：不算完整登录，不能误报
        let _ = run_git_config(
            Some(&path),
            &["config", "--global", "--unset-all", key.as_str()],
        );
        let _ = run_git_config(
            Some(&path),
            &["config", "--global", "--add", key.as_str(), ""],
        );
        let username_only = "!f() { echo username=sys-user; }; f";
        let _ = run_git_config(
            Some(&path),
            &["config", "--global", "--add", key.as_str(), username_only],
        );
        assert_eq!(detect_system_credential_at(host, Some(&path)), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn git_helper_config_is_scoped_and_restored() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "devup-gitconfig-{}-{}",
            std::process::id(),
            "helper-test"
        ));
        let _ = std::fs::remove_file(&path);

        let host = "devup-test.invalid";
        let key = helper_key(host);
        let Ok(out) = run_git_config(
            Some(&path),
            &[
                "config",
                "--global",
                "--add",
                key.as_str(),
                "!echo user-helper",
            ],
        ) else {
            return;
        };
        if !out.status.success() {
            return;
        }

        let backup = install_git_helper_at(host, Some(&path))
            .unwrap()
            .expect("首次接管必须返回原配置");
        assert_eq!(backup, vec!["!echo user-helper".to_string()]);

        let values = git_config_get_all_at(&key, Some(&path));
        assert!(
            values.iter().any(|v| is_devup_helper(v)),
            "helper 未写入: {values:?}"
        );
        assert_eq!(
            values.first().map(String::as_str),
            Some(""),
            "空值必须排在最前，才能重置该 URL 的 helper 链"
        );

        uninstall_git_helper_at(host, &backup, Some(&path)).unwrap();
        assert_eq!(
            git_config_get_all_at(&key, Some(&path)),
            vec!["!echo user-helper".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }
}