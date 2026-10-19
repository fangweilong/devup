# DevUp · 现代化开发环境与 AI 工具链管家 (国内极速版)

专为国内开发者打造的 **Windows / macOS 现代化开发环境与 AI Coding Agent 一键装配/管理工具**。

采用 **Rust + 系统原生 WebView**（Windows 使用 WebView2，macOS 使用 WKWebView）构建，输出仅约 10MB 的单个绿色免安装可执行程序（`devup.exe` / `devup`），无需敲终端命令、无需配置复杂的环境变量，弹出现代化暗黑卡片操作界面，一键解决网络阻断与多依赖装配难题。

---

## ⚡ 核心能力

1. **统一开发环境与 AI 工具链管理**：
   - 不只是安装某个 Agent，而是覆盖从底层终端（Windows Terminal, PowerShell 7）、包管理（macOS Homebrew）、基础运行时（Node.js, Git）到顶层 AI 编程助手（oh-my-pi, Claude Code, Codex, Pi, OpenCode）的全链路环境。
2. **零终端操作，小白即用**：
   - 彻底免去繁琐的命令行排障、手动环境变量编辑。
   - 现代化暗黑卡片 UI（基于系统原生 WebView：Windows 10/11 内置 Edge WebView2 / macOS 内置 WKWebView，免装任何 Runtime）。
3. **国内网络三层加速引擎（彻底摆脱单一节点不稳定）**：
   - **智能感知本地系统代理**：实时只读嗅探系统代理（Windows 注册表 / macOS `scutil`，兼容 Clash / v2rayN 等），下载与 npm 自动挂载代理端口。
   - **国内 GitHub 多源竞速集群（免翻直连）**：内置 `ghproxy.net`、`mirror.ghproxy.com`、`ghps.cc` 等高速节点，并发测速，毫秒级选取最优下载源。
   - **官方 npmmirror 镜像源**：npm 全局包强制走阿里云维护的官方国内镜像，几秒下载完毕；macOS 上 Homebrew bottle 额外启用清华 TUNA 镜像加速。
4. **绿色便携，零系统污染**：
   - 纯内存执行实时探活检测，不私自改写系统注册表，所有软件安装后由其官方安装程序自行完成 PATH 注入。

---

## 📦 支持的管理软件清单

| 类别 | 软件名称 | 说明 | 仓库 / 官方地址 |
| :--- | :--- | :--- | :--- |
| **基础开发环境** (Windows) | **Windows Terminal** | 微软现代终端，支持多标签页与 GPU 渲染 | [microsoft/terminal](https://github.com/microsoft/terminal) |
| **基础开发环境** (Windows) | **PowerShell 7** | 现代化跨平台终端脚本引擎与执行基础 | [PowerShell/PowerShell](https://github.com/PowerShell/PowerShell) |
| **基础开发环境** (macOS) | **Homebrew** | macOS 事实标准包管理器，内置清华镜像加速安装 | [brew.sh](https://brew.sh) |
| **基础开发环境** | **Node.js (LTS)** | Claude Code、Codex、Pi 等 Agent 必备的运行环境 | [nodejs.org](https://nodejs.org) |
| **基础开发环境** | **Git** | 代码版本管理系统，绝大多数 Agent 的依赖项 | [git-scm.com](https://git-scm.com) |
| **AI 编程智能体** | **oh-my-pi (omp)** | 带 IDE 级工具与高级编辑功能的终端 AI 编程智能体 | [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) |
| **AI 编程智能体** | **Claude Code** | Anthropic 官方研究级终端编码智能体 CLI | [anthropics/claude-code](https://github.com/anthropics/claude-code) |
| **AI 编程智能体** | **OpenAI Codex** | OpenAI 官方开源轻量级终端 Coding Agent | [openai/codex](https://github.com/openai/codex) |
| **AI 编程智能体** | **Pi Agent** | Earendil Works 统一 LLM 循环与终端交互编程框架 | [earendil-works/pi](https://github.com/earendil-works/pi) |
| **AI 编程智能体** | **OpenCode** | Anomaly 团队开源的高性能轻量 Coding Agent | [anomalyco/opencode](https://github.com/anomalyco/opencode) |

---

## 🏗️ 极简技术栈与工程结构

无需安装 Node.js、Vite 或庞大的前端依赖，全项目基于标准 Rust 工具链编译打包：

```text
devup/
├── .github/
│   └── workflows/
│       └── release.yml        # GitHub Actions 自动化构建与发布工作流
├── src/
│   ├── main.rs                # 宿主入口：Wry/Tao 窗口、IPC 消息泵、Tokio 异步运行时
│   ├── platform.rs            # 跨平台适配层：命令查找 / PATH 扩展 / 外链打开 / 架构标识
│   ├── detector/              # 实时环境与 Agent 探活模块 (纯命令探测，零污染)
│   ├── installer/             # 平台安装器：Windows MSI/AppX/WinGet、macOS Homebrew/PKG 提权、npm 国内源自动安装
│   ├── network/               # 系统代理自动嗅探、多源镜像并发测速、流式带速下载器
│   └── state.rs               # IPC 协议与事件定义
├── ui/                        # 原生内嵌前端（零构建依赖，直接 include_str! 编译进单一可执行文件）
│   ├── index.html             # 现代化暗黑仪表盘
│   ├── style.css              # 霓虹发光、磨砂圆角、进度条动画
│   └── app.js                 # 交互逻辑与实时状态轮询
└── Cargo.toml                 # 纯粹的 Rust 依赖清单
```

---

## 🚀 GitHub Actions 自动化打包发布

项目已配置标准 GitHub Actions 工作流（`.github/workflows/release.yml`）。

### 触发打包的两种方式：

1. **一键手动触发**：
   - 提交代码到 GitHub -> 点击仓库页面 **Actions** -> 选择 **Build & Release** 工作流 -> 点击 **Run workflow**。
   - `windows-latest` 与 `macos-14`（arm64 / x64 双架构矩阵）并行编译，Artifacts 中分别输出 `devup.exe`、`devup-macos-arm64`、`devup-macos-x64`。
2. **Git Tag 自动发布 Release**：
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
   GitHub Actions 检测到 `v*` tag 后，会自动编译并将三个平台的产物统一挂载到 GitHub Releases 下载页面；应用内的「检查更新」会按当前平台与 CPU 架构自动匹配对应资产完成原地自升级。
3. **PR / 功能分支**：推送 `feat/**` 分支或发起 PR 时仅执行全平台编译验证，不发布 Release。

---

## 💻 本地构建（可选）

本地如有安装好的 Rust 工具链（Windows 需 MSVC 编译环境，macOS 需 Xcode Command Line Tools）：

```bash
# 启动开发调试
cargo run

# 构建单文件 Release 产物 (体积约 10MB，零额外依赖)
cargo build --release

# macOS 交叉构建 Intel 版本（在 Apple Silicon 上）
cargo build --release --target x86_64-apple-darwin
```

编译产物位于 `target/release/devup.exe`（Windows）或 `target/release/devup`（macOS）。

> macOS 使用提示：直接双击裸二进制可能被 Gatekeeper 拦截，可在终端运行，或在「系统设置 → 隐私与安全性」中允许；安装 Node.js / 卸载 PKG 版 Node 时会弹出系统原生授权框。
