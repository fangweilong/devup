# DevUp Agent 约束

## Windows 子进程：禁止闪黑窗（强制）

DevUp 是 `windows_subsystem = "windows"` 的 GUI 程序。从主进程拉起任何控制台程序
（`git`、`cmd`、`powershell`、`where`、`winget`、`msiexec` 等）时：

- MUST 设置 `CREATE_NO_WINDOW`（`0x08000000`）；否则每次启动、刷新或登录都会在桌面上闪出黑框。
- `std::process::Command` 统一调用 `crate::platform::hide_console(&mut cmd)`，不要各写各的。
- `tokio::process::Command` 直接 `.creation_flags(0x08000000)`（Tokio 不经过上面的 helper）。
- 新增 `Command::new(...)` 时先判断目标是否控制台程序；确需用户可见终端时必须写明原因。
- 仅测试代码（`#[cfg(test)]`，控制台测试进程）可豁免。

验证方式：启动或刷新期间枚举顶层窗口，可见的 `ConsoleWindowClass` 数量应为 0。