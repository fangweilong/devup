use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::detector::{NodeEnv, is_within, path_key};
use crate::platform;

/// Homebrew 清华镜像环境变量（国内网络加速），仅作用于 DevUp 发起的 brew 调用
fn brew_mirror_env(cmd: &mut Command) {
    cmd.env("HOMEBREW_BOTTLE_DOMAIN", "https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1");
}

pub struct MacInstaller;

impl MacInstaller {
    /// 当前机器是否存在 brew 命令
    pub fn brew_available() -> bool {
        platform::which("brew").is_some()
    }

    /// brew 中是否已安装指定 formula
    pub fn brew_installed_formula(formula: &str) -> bool {
        let Some(brew) = platform::which("brew") else {
            return false;
        };
        std::process::Command::new(brew)
            .args(["list", "--versions", formula])
            .env("PATH", platform::child_path())
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false)
    }

    /// 执行 brew 子命令并逐行透传输出（安装/升级/卸载 formula）
    pub async fn brew_run<F>(args: &[&str], mut on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        let brew = platform::which("brew").context("未检测到 Homebrew，请先安装 Homebrew")?;
        let mut cmd = Command::new(brew);
        cmd.args(args);
        cmd.env("PATH", platform::child_path());
        brew_mirror_env(&mut cmd);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 brew {:?} 失败", args))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let _ = tx.send(line);
                }
            });
        }
        drop(tx);

        while let Some(line) = rx.recv().await {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                on_log(trimmed.to_string());
            }
        }

        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            bail!("brew {:?} 退出码异常: {:?}", args, status.code())
        }
    }

    /// 安装或升级 formula：已存在则 upgrade，否则 install
    pub async fn brew_install_or_upgrade<F>(formula: &str, on_log: F) -> Result<()>
    where
        F: FnMut(String) + Send + 'static,
    {
        if Self::brew_installed_formula(formula) {
            Self::brew_run(&["upgrade", formula], on_log).await
        } else {
            Self::brew_run(&["install", formula], on_log).await
        }
    }

    /// 以管理员权限执行 shell 片段（弹出 macOS 原生授权对话框）
    pub async fn run_admin_shell(script_body: &str, prompt: &str) -> Result<()> {
        // AppleScript 字符串内的双引号需转义
        let escaped = script_body.replace('\\', "\\\\").replace('"', "\\\"");
        let source = format!(
            r#"do shell script "{}" with administrator privileges with prompt "{}""#,
            escaped, prompt
        );

        let output = Command::new("osascript")
            .args(["-e", &source])
            .output()
            .await
            .context("调用 osascript 管理员授权失败")?;

        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            // 用户取消授权时 osascript 返回 -128
            if err.contains("-128") {
                bail!("已取消管理员授权，安装中止");
            }
            bail!("管理员命令执行失败: {}", err.trim())
        }
    }

    /// 静默安装官方 .pkg 安装包（经原生授权对话框提权）
    pub async fn install_pkg(pkg_path: &Path, prompt: &str) -> Result<()> {
        let script = format!(
            "/usr/sbin/installer -pkg '{}' -target /",
            pkg_path.to_string_lossy().replace('\'', "'\\''")
        );
        Self::run_admin_shell(&script, prompt).await
    }

    /// 在系统终端中打开一个交互式脚本（用于 Homebrew 官方安装/卸载这类
    /// 必须真人输入密码、拒绝 root 直接执行的长流程）
    pub async fn open_in_terminal(script: &str) -> Result<()> {
        let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
        let source = format!(
            r#"tell application "Terminal"
    activate
    do script "{}"
end tell"#,
            escaped
        );
        let output = Command::new("osascript")
            .args(["-e", &source])
            .output()
            .await
            .context("调用 osascript 打开终端失败")?;
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("唤起终端失败: {}", err.trim())
        }
    }

    /// 以 root 权限清理官方 PKG（npmmirror 镜像的 PKG 同源）安装的 Node.js 文件。
    ///
    /// 计划 §2.5 的第 2 步。护栏 D 的 `check_uninstall` 回答的是「该不该删」（没有
    /// receipt 就不许碰 `/usr/local`），这里补的是剩下那个洞：**receipt 存在、判定放行，
    /// 也不等于清单上每一条都能删** —— `/usr/local/bin/node` 可能已被谁 `ln -s` 到
    /// `~/.nvm/versions/...`，旧的硬编码 `rm -rf` 会顺着链接把托管方的 runtime 删掉。
    ///
    /// 两道动作：
    /// 1. 候选先过 `pkg_removal_targets`（纯路径判定，canonical 目标落在管理器目录内的跳过）；
    /// 2. 过滤后一个可删项都没有时**直接返回，不调 `run_admin_shell`** —— 那个函数一被调用
    ///    就弹出 macOS 系统授权框，让用户对着「点了就执行、实际什么都不删」的密码框确认
    ///    是欺骗，弹框本身就是需要省掉的副作用。
    ///
    /// `on_log` 只承载提权前的那条诚实预告（全局 node_modules）。跳过清单**不**在这里印：
    /// 它还要进 `Guidance`，两处各印一份迟早措辞漂移，交给调用方一次性呈现。
    pub async fn remove_pkg_node_files<F>(
        env: &NodeEnv,
        prompt: &str,
        mut on_log: F,
    ) -> Result<PkgRemoval>
    where
        F: FnMut(String),
    {
        let candidates: Vec<PathBuf> = PKG_NODE_CANDIDATES.iter().map(PathBuf::from).collect();
        let (removable, skipped) = pkg_removal_targets(env, &candidates, fs_canonical_target);
        if removable.is_empty() {
            return Ok(PkgRemoval {
                deleted: Vec::new(),
                skipped,
                executed: false,
            });
        }
        if let Some(warn) = npm_prefix_uninstall_warning(env, &removable) {
            on_log(warn);
        }
        on_log("正在弹出系统授权框以清理官方 PKG 安装的 Node.js 文件...".into());
        let script = pkg_removal_script(&removable);
        // 提权失败（含用户在授权框点「取消」）继续 `?` 向上传播，走既有的 InstallError 通路
        Self::run_admin_shell(&script, prompt).await?;
        Ok(PkgRemoval {
            deleted: removable,
            skipped,
            executed: true,
        })
    }
}

/// 官方 PKG（含 npmmirror 镜像分发的同名包）在 `/usr/local` 下的全部落点。
///
/// 这是卸载清单的**唯一来源**，但只是「候选」：每一项都要先过 `pkg_removal_targets`
/// 的所有权核对才进得了 `rm -rf`（旧实现把这串硬编码直接塞给 rm，§2.5 要改掉的就是这个）。
const PKG_NODE_CANDIDATES: &[&str] = &[
    "/usr/local/bin/node",
    "/usr/local/bin/npm",
    "/usr/local/bin/npx",
    "/usr/local/bin/corepack",
    "/usr/local/lib/node_modules",
    "/usr/local/include/node",
];

/// 全局包目录：诚实预告（`npm_prefix_uninstall_warning`）只盯这一条，其余落点没有
/// 「用户自己的东西住在里面」的问题。
const PKG_NODE_MODULES: &str = "/usr/local/lib/node_modules";

/// PKG 卸载的核对/执行结果。调用方据此走 `Ok(true)` / `Ok(false)`：护栏契约（见
/// `installer::node_preflight` 与 §9 台账 R1）要求「已出指引、本轮未执行」不得再发
/// `InstallSuccess`，`executed` 就是那条分支的判据。
pub struct PkgRemoval {
    /// 真正进入 `rm -rf` 的路径
    pub deleted: Vec<PathBuf>,
    /// 因所有权核对跳过的路径（每一项都必须反映到用户可见输出，不许静默吞掉）
    pub skipped: Vec<PathBuf>,
    /// 是否实际提权执行过。`false` 时保证**没有**弹出系统授权框。
    pub executed: bool,
}

/// 按所有权过滤 PKG 卸载清单：canonical 目标落在任一管理器根目录内的路径不删。
///
/// 纯路径逻辑：不起进程、不读环境变量、也不碰文件系统。「canonical 目标」经 `resolve`
/// 注入 —— 真机传 `fs_canonical_target`，单传一张合成映射表。为什么必须这样拆：本函数
/// 的核心场景（候选自身在 `/usr/local`、真实身份却在 `~/.nvm/versions/...` 里）在合成路径上
/// 要能复现，而真机上某个路径此刻在不在磁盘、指向哪，都属于现场证据、不属于判定规则。
///
/// 判定口径与 `detector::node_source` 的 G1/G2/G6 完全同一套（`path_key` 归一化 +
/// `is_within` 边界），这里不新起第二套规则：两处各归一次类，迟早对 `/usr/local2`
/// 这类同前缀陷阱给出两种结论。候选自身已在管理器目录内（如 `n` 把 `N_PREFIX` 设到
/// `/usr/local`）也跳过，这种不必再解析。`env.managers` 为空 ⇒ 无一条命中 ⇒ 全删，
/// 这正是 `node.mode = "system"` 逃生口的形态（preflight 交回默认 `NodeEnv`，行为退回旧版）。
///
/// 返回 `(可删, 因托管跳过)`。
fn pkg_removal_targets(
    env: &NodeEnv,
    candidates: &[PathBuf],
    resolve: impl Fn(&Path) -> Option<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let roots: Vec<String> = env
        .managers
        .iter()
        .map(|m| path_key(&m.root.to_string_lossy()))
        .collect();
    let owned = |p: &Path| {
        let key = path_key(&p.to_string_lossy());
        roots.iter().any(|r| is_within(&key, r))
    };
    let mut removable = Vec::new();
    let mut skipped = Vec::new();
    for c in candidates {
        // 解析失败（路径不存在 / 无权限）按「没有额外证据」处理：退化为只看自身路径。
        // 不存在的东西既不会被这次 `rm` 删到，也不会顺链接删到别人。
        let hit = owned(c) || resolve(c).is_some_and(|t| owned(&t));
        if hit {
            skipped.push(c.clone());
        } else {
            removable.push(c.clone());
        }
    }
    (removable, skipped)
}

/// 真机用的 canonical 目标解析：穿透 symlink（含别人手工
/// `ln -s ~/.nvm/versions/node/v26/bin/node /usr/local/bin/node` 这种）。
/// 只做读取，任何写入都不在这条路上。
fn fs_canonical_target(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// shell 单引号安全包裹（内部 `'` 转成 `'\''`，与 `install_pkg` 同款写法）
fn shell_squote(p: &str) -> String {
    format!("'{}'", p.replace('\'', "'\\''"))
}

/// 用核对通过的清单动态拼卸载脚本：只删放行的路径，一个都不能多。
///
/// 单独成函数是为了让单测能断言「没被放行的路径一个字符都不该出现在脚本里」——
/// 过滤逻辑对不对，最终看的是这条交给 `run_admin_shell` 的字符串。
/// `pkgutil --forget` 只在真的有实体文件被删时才有意义，故挂在 `rm` 的 `&&` 之后
/// （与旧脚本同形）；可删集合为空时根本不会走到拼脚本，见 `remove_pkg_node_files`。
fn pkg_removal_script(removable: &[PathBuf]) -> String {
    let paths = removable
        .iter()
        .map(|p| shell_squote(&p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    format!("rm -rf {paths} && pkgutil --forget org.nodejs.node.pkg 2>/dev/null; true")
}

/// 「删 `/usr/local/lib/node_modules` 会连带抹掉用户的全局包」的诚实预告。
///
/// 官方 PKG 的典型形态是 `npm config get prefix` = `/usr/local`，于是这个目录里住着用户
/// 用 DevUp 装过的全局 Agent（claude / codex / pi / omp / opencode）。今天的确认弹窗只说
/// 「依赖 Node 的 Agents 无法运行」——那是半句话，实际是**连包本身一起删掉**。
///
/// 只出日志，**不改删除语义**：PKG 的 `lib/node_modules` 本来就该随 PKG 卸载走，这里不是
/// 要发明第二条卸载路径，只是把后果说全。
fn npm_prefix_uninstall_warning(env: &NodeEnv, removable: &[PathBuf]) -> Option<String> {
    let prefix = env.npm_prefix.as_ref()?;
    let nm_key = path_key(PKG_NODE_MODULES);
    // 全局包目录这次没被排进删除清单（被跳过，或本就不在候选里）就没什么可预告的
    let nm_scheduled = removable
        .iter()
        .any(|p| path_key(&p.to_string_lossy()) == nm_key);
    if !nm_scheduled {
        return None;
    }
    let pk = path_key(&prefix.to_string_lossy());
    // 两个方向都要认：prefix 是全局目录的祖先（`/usr/local` 这种典型形态）或落在其内，
    // 删除范围都会盖住用户的包。prefix 指到 `~/.nvm/...` 之类他处时不触发 ——
    // 那种情况全局包根本不住在将要删除的目录里。
    if !is_within(&nm_key, &pk) && !is_within(&pk, &nm_key) {
        return None;
    }
    Some(format!(
        "⚠ 本机 npm 全局前缀是 {}，`{PKG_NODE_MODULES}` 里装着你自己安装的全局包（含经 DevUp 安装的 claude / codex / pi / omp / opencode）。卸载 Node 会把它们一并删除且不可恢复；需要先留底，请在终端执行 `npm ls -g --depth=0` 保存输出。要保住这些包请在即将弹出的授权框点「取消」，或先把全局前缀迁到别处。",
        prefix.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::{Manager, ManagerKind};
    use std::collections::HashMap;

    fn env_with(roots: &[(&str, ManagerKind)]) -> NodeEnv {
        NodeEnv {
            managers: roots
                .iter()
                .map(|(r, k)| Manager {
                    kind: *k,
                    root: PathBuf::from(r),
                    versions: Vec::new(),
                })
                .collect(),
            ..Default::default()
        }
    }

    /// 合成现场的 symlink 表：候选路径 -> canonical 目标。查不到即「解析失败」，
    /// 与真机上路径不存在时的 `fs_canonical_target` 行为一致 —— 判定全程不碰磁盘。
    fn links(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn resolve_from(map: HashMap<String, String>) -> impl Fn(&Path) -> Option<PathBuf> {
        move |p: &Path| map.get(&p.to_string_lossy().to_string()).map(PathBuf::from)
    }

    fn names(v: &[PathBuf]) -> Vec<String> {
        v.iter().map(|p| p.to_string_lossy().to_string()).collect()
    }

    /// 表驱动：`pkg_removal_targets` 的每条判定规则钉一格。
    /// 第 3 行是本任务的存在理由 —— 候选自身好端端在 `/usr/local`，顺着 symlink 才露出
    /// 「真身住在 nvm 版本目录里」，旧的硬编码 `rm -rf` 看不见这一层。
    #[test]
    fn pkg_removal_targets_filters_by_ownership() {
        struct Case {
            name: &'static str,
            managers: Vec<(&'static str, ManagerKind)>,
            candidates: Vec<&'static str>,
            symlinks: Vec<(&'static str, &'static str)>,
            expect_removable: Vec<&'static str>,
            expect_skipped: Vec<&'static str>,
        }
        let cases = [
            Case {
                name: "无管理器时清单照单全删（node.mode=system 的形态）",
                managers: vec![],
                candidates: vec!["/usr/local/bin/node", "/usr/local/lib/node_modules"],
                symlinks: vec![],
                expect_removable: vec!["/usr/local/bin/node", "/usr/local/lib/node_modules"],
                expect_skipped: vec![],
            },
            Case {
                name: "候选自身就落在管理器根目录内",
                managers: vec![("/usr/local", ManagerKind::N)],
                candidates: vec!["/usr/local/bin/node", "/usr/local/include/node"],
                symlinks: vec![],
                expect_removable: vec![],
                expect_skipped: vec!["/usr/local/bin/node", "/usr/local/include/node"],
            },
            Case {
                name: "候选在系统目录、canonical 目标指向 nvm —— 核心场景",
                managers: vec![("/Users/u/.nvm", ManagerKind::Nvm)],
                candidates: vec!["/usr/local/bin/node", "/usr/local/bin/npm"],
                symlinks: vec![(
                    "/usr/local/bin/node",
                    "/Users/u/.nvm/versions/node/v26.12.0/bin/node",
                )],
                expect_removable: vec!["/usr/local/bin/npm"],
                expect_skipped: vec!["/usr/local/bin/node"],
            },
            Case {
                name: "lib/node_modules 落在 nvm 版本目录内",
                managers: vec![("/Users/u/.nvm", ManagerKind::Nvm)],
                candidates: vec!["/usr/local/lib/node_modules"],
                symlinks: vec![(
                    "/usr/local/lib/node_modules",
                    "/Users/u/.nvm/versions/node/v20.15.0/lib/node_modules",
                )],
                expect_removable: vec![],
                expect_skipped: vec!["/usr/local/lib/node_modules"],
            },
            Case {
                name: "多管理器并存（本机 nvm + volta 的形态）各认各的",
                managers: vec![
                    ("/Users/u/.nvm", ManagerKind::Nvm),
                    ("/Users/u/.volta", ManagerKind::Volta),
                ],
                candidates: vec![
                    "/usr/local/bin/node",
                    "/usr/local/bin/npm",
                    "/usr/local/bin/npx",
                    "/usr/local/include/node",
                ],
                symlinks: vec![
                    (
                        "/usr/local/bin/node",
                        "/Users/u/.nvm/versions/node/v26.12.0/bin/node",
                    ),
                    (
                        "/usr/local/bin/npm",
                        "/Users/u/.volta/tools/image/node/20.15.0/bin/npm",
                    ),
                    ("/usr/local/bin/npx", "/usr/local/bin/npx"),
                ],
                expect_removable: vec!["/usr/local/bin/npx", "/usr/local/include/node"],
                expect_skipped: vec!["/usr/local/bin/node", "/usr/local/bin/npm"],
            },
            Case {
                name: "候选不在磁盘上（解析失败）时仍按自身路径纯逻辑判定",
                managers: vec![("/Users/u/.nvm", ManagerKind::Nvm)],
                candidates: vec!["/usr/local/bin/node", "/usr/local/bin/corepack"],
                symlinks: vec![],
                expect_removable: vec!["/usr/local/bin/node", "/usr/local/bin/corepack"],
                expect_skipped: vec![],
            },
            Case {
                name: "大小写折叠：管理器根与候选大小写不同仍算命中",
                managers: vec![("/USR/local", ManagerKind::N)],
                candidates: vec!["/usr/local/BIN/node"],
                symlinks: vec![],
                expect_removable: vec![],
                expect_skipped: vec!["/usr/local/BIN/node"],
            },
            Case {
                name: "同前缀陷阱：/usr/local2 不是 /usr/local 的孩子",
                managers: vec![("/usr/local", ManagerKind::N)],
                candidates: vec!["/usr/local2/bin/node"],
                symlinks: vec![],
                expect_removable: vec!["/usr/local2/bin/node"],
                expect_skipped: vec![],
            },
        ];
        for c in cases {
            let env = env_with(&c.managers);
            let candidates: Vec<PathBuf> = c.candidates.iter().map(PathBuf::from).collect();
            let (removable, skipped) =
                pkg_removal_targets(&env, &candidates, resolve_from(links(&c.symlinks)));
            assert_eq!(
                names(&removable),
                c.expect_removable,
                "可删集合不符 [{name}]",
                name = c.name
            );
            assert_eq!(
                names(&skipped),
                c.expect_skipped,
                "跳过集合不符 [{name}]",
                name = c.name
            );
        }
    }

    /// 「可删集合为空 ⇒ 绝不走提权路径」的可证明形式：`remove_pkg_node_files` 唯一的
    /// `run_admin_shell` 调用点在 `removable.is_empty()` 早退之后，因此纯函数返回空集
    /// 就等价于「没有弹框」。不去 mock `osascript`，是因为 mock 出来的不弹框证明不了
    /// 真分支 —— 要钉的是那个 `if` 的输入条件本身。
    #[test]
    fn empty_removable_set_means_no_elevation() {
        let env = env_with(&[("/Users/u/.nvm", ManagerKind::Nvm)]);
        let candidates: Vec<PathBuf> = PKG_NODE_CANDIDATES.iter().map(PathBuf::from).collect();
        // 最坏现场：六条候选全部被 symlink 进 nvm 目录
        let all_links: Vec<(&str, &str)> = PKG_NODE_CANDIDATES
            .iter()
            .map(|p| {
                (
                    *p,
                    "/Users/u/.nvm/versions/node/v26.12.0/lib/escaped-target",
                )
            })
            .collect();
        let (removable, skipped) =
            pkg_removal_targets(&env, &candidates, resolve_from(links(&all_links)));
        assert!(
            removable.is_empty(),
            "全被托管时不得留下任何可删项，否则会走到 run_admin_shell"
        );
        assert_eq!(skipped.len(), PKG_NODE_CANDIDATES.len());
    }

    /// 动态拼接的脚本只准包含放行的路径：未放行的一个字符都不许出现（包括路径的任何
    /// 片段），`'` 按 shell 语义转义。这条断言看的是最终交给 `run_admin_shell` 的字符串，
    /// 过滤逻辑对不对，到这里才算闭上环。
    #[test]
    fn pkg_removal_script_only_touches_allowed_paths() {
        let allowed = [
            "/usr/local/bin/node",
            "/usr/local/lib/node_modules",
            "/tmp/we'red dir/node",
        ];
        let removable: Vec<PathBuf> = allowed.iter().map(PathBuf::from).collect();
        let script = pkg_removal_script(&removable);
        for p in &allowed {
            // 期望形态由 `shell_squote` 自己给出：含 `'` 的路径不能拿原文去比引号
            assert!(script.contains(&shell_squote(p)), "脚本缺少路径 {p}");
        }
        // 未进清单的候选不得以任何形式混进来
        for p in [
            "/usr/local/bin/npm",
            "/usr/local/bin/npx",
            "/usr/local/bin/corepack",
            "/usr/local/include/node",
        ] {
            assert!(!script.contains(p), "脚本越权包含 {p}");
        }
        assert!(script.contains(r#"'\''"#), "单引号未按 shell 语义转义");
        assert!(script.starts_with("rm -rf '"));
        assert!(script.ends_with("&& pkgutil --forget org.nodejs.node.pkg 2>/dev/null; true"));
    }

    /// 诚实预告的触发面：前缀与将删的全局目录有包含关系才说话，其余一律闭嘴。
    /// 只验文案条件，不验删除语义 —— 这个函数没有能力改变删不删。
    #[test]
    fn npm_prefix_warning_table() {
        struct Case {
            name: &'static str,
            prefix: Option<&'static str>,
            removable: Vec<&'static str>,
            expect_warn: bool,
        }
        let nm = "/usr/local/lib/node_modules";
        let cases = [
            Case {
                name: "官方 PKG 典型形态 prefix=/usr/local",
                prefix: Some("/usr/local"),
                removable: vec!["/usr/local/bin/node", nm],
                expect_warn: true,
            },
            Case {
                name: "prefix 就是全局目录本身",
                prefix: Some(nm),
                removable: vec![nm],
                expect_warn: true,
            },
            Case {
                name: "prefix 在 nvm 版本目录里，全局包不住在要删的地方",
                prefix: Some("/Users/u/.nvm/versions/node/v26.12.0"),
                removable: vec![nm],
                expect_warn: false,
            },
            Case {
                name: "全局目录这次没被放行（被跳过了），没什么可预告",
                prefix: Some("/usr/local"),
                removable: vec!["/usr/local/bin/node"],
                expect_warn: false,
            },
            Case {
                name: "探测不到 prefix（npm 不可用 / 超时）",
                prefix: None,
                removable: vec![nm],
                expect_warn: false,
            },
        ];
        for c in cases {
            let env = NodeEnv {
                npm_prefix: c.prefix.map(PathBuf::from),
                ..Default::default()
            };
            let removable: Vec<PathBuf> = c.removable.iter().map(PathBuf::from).collect();
            let warn = npm_prefix_uninstall_warning(&env, &removable);
            assert_eq!(warn.is_some(), c.expect_warn, "预告判定不符 [{}]", c.name);
            if let Some(text) = warn {
                assert!(
                    text.contains("npm ls -g --depth=0"),
                    "预告必须给出留底命令 [{}]",
                    c.name
                );
            }
        }
    }

    /// 本机真实现场的只读核对：打印真实 `detect()` 的 NodeEnv 下，PKG 清单的可删/跳过两列，
    /// 以及实际会交给 `run_admin_shell` 的那条命令（**只拼字符串，不执行**）。
    /// 本机 `pkg_receipt=false` + nvm/volta 并存 ⇒ 护栏 D 的 G5 会先拒绝，这个函数在生产
    /// 里根本轮不到；它证明的是「过滤层的判定与本机现场一致」，不是「端到端卸载验证过」。
    #[test]
    #[ignore = "依赖本机真实 Node 现场，仅手动核对"]
    fn dump_uninstall_targets() {
        let env = crate::detector::detect();
        println!(
            "owner={:?} managers={:?} npm_prefix={:?} pkg_receipt={}",
            env.owner,
            env.managers
                .iter()
                .map(|m| format!("{}({})", m.kind.label(), m.root.display()))
                .collect::<Vec<_>>(),
            env.npm_prefix,
            env.pkg_receipt
        );
        let candidates: Vec<PathBuf> = PKG_NODE_CANDIDATES.iter().map(PathBuf::from).collect();
        let (removable, skipped) = pkg_removal_targets(&env, &candidates, fs_canonical_target);
        println!("可删: {:?}", names(&removable));
        println!("因托管跳过: {:?}", names(&skipped));
        if !removable.is_empty() {
            println!("将执行的脚本: {}", pkg_removal_script(&removable));
        }
        println!(
            "G5 层判定: {:?}",
            crate::detector::check_uninstall(&env, crate::detector::Channel::MacPkg)
        );
    }
}

/// Homebrew 官方安装脚本（预置清华镜像环境变量，国内网络友好）
pub const HOMEBREW_INSTALL_SCRIPT: &str = r#"export HOMEBREW_INSTALL_FROM_API=1 HOMEBREW_API_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles/api" HOMEBREW_BOTTLE_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles" HOMEBREW_BREW_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/brew.git" HOMEBREW_CORE_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/homebrew-core.git"; /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#;

/// Homebrew 官方卸载脚本
pub const HOMEBREW_UNINSTALL_SCRIPT: &str = r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/uninstall.sh)""#;
