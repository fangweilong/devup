// 前端状态与 IPC 桥接
let state = {
  softwares: [],
  proxy: null,
  proxyMode: "system",
  proxyUrl: null,
  githubMode: "auto",
  githubMirror: "https://ghproxy.net/",
  npmRegistry: "https://registry.npmmirror.com",
  githubMirrors: [],
  npmMirrors: [],
  isRefreshing: false,
  activeTaskId: null,
  activeFilter: "all",
  pendingUninstallId: null,
  pendingAction: null,
  updateInfo: null,
  // target_id -> 护栏指引（"未代为执行"的组件在卡片里挂横幅，随 renderAll 重建）
  guidance: {},
};

function postCommand(cmd, payload = null) {
  if (window.ipc) {
    window.ipc.postMessage(JSON.stringify({ cmd, payload }));
  } else {
    console.warn("IPC 未连接:", cmd, payload);
  }
}

function openExternal(url) {
  postCommand("OpenUrl", { url });
}

// macOS 透明标题栏融合模式下，为头部预留红绿灯与标题栏热区空间
const isMac = /Mac|iPod|iPhone|iPad/.test(navigator.platform || "") || /Mac OS X/.test(navigator.userAgent || "");
if (isMac) {
  document.body.classList.add("is-mac");

  // 必须在 mousedown 阶段启动原生拖拽，让 macOS 使用真实的按下事件作为
  // 拖拽锚点。延迟到 mousemove/IPC 阶段会丢失按下位置，尤其在副屏上会造成
  // 窗口瞬移到鼠标下方。
  const headerEl = document.querySelector("header");
  if (headerEl) {
    headerEl.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || e.target.closest("button, input, select, textarea, a")) {
        return;
      }
      postCommand("DragWindow");
    });
  }

  // tao 0.37 无应用菜单，Cmd+Q 没有系统级归宿；webview 获得焦点时快捷键
  // 会作为普通 keydown 进入页面，这里拦截并经 IPC 走与关闭窗口一致的退出链路。
  window.addEventListener("keydown", (e) => {
    if (
      e.metaKey &&
      !e.ctrlKey &&
      !e.altKey &&
      !e.shiftKey &&
      (e.key === "q" || e.key === "Q")
    ) {
      e.preventDefault();
      postCommand("Quit");
    }
  });
}

// 供 Rust 回调的全局事件接收器
window.onEvent = function (event) {
  switch (event.type) {
    case "StatusList":
      state.softwares = event.data;
      renderAll();
      const btnRefresh = document.getElementById("btn-refresh");
      if (btnRefresh) {
        btnRefresh.classList.remove("is-spinning");
        btnRefresh.disabled = false;
      }
      state.isRefreshing = false;
      break;

    case "ProxyStatus":
      state.proxy = event.data.proxy;
      state.proxyMode = event.data.mode || "system";
      state.proxyUrl = event.data.url;
      state.githubMode = event.data.github_mode || "auto";
      state.githubMirror = event.data.github_mirror || "https://ghproxy.net/";
      state.npmRegistry = event.data.npm_registry || "https://registry.npmmirror.com";
      updateProxyBadge();
      break;

    case "MirrorOptions":
      // 内置镜像清单由后端(唯一数据源)下发，避免与 Rust 解析逻辑漂移
      state.githubMirrors = event.data.github || [];
      state.npmMirrors = event.data.npm || [];
      renderMirrorOptions();
      break;

    case "Log":
      appendLog(`[${event.data.target_id}] ${event.data.message}`);
      break;

    case "Progress":
      updateProgress(event.data);
      break;

    case "InstallSuccess":
      // 这一轮真的执行完了，此前那条「未代为执行」的指引随即失效
      delete state.guidance[event.data.target_id];
      appendLog(`✅ [${event.data.target_id}] 安装完成！`);
      state.activeTaskId = null;
      hideProgress();
      enableAllButtons();
      // 安装成功后自动刷新状态
      postCommand("Refresh");
      break;

    case "InstallError":
      appendLog(`❌ [${event.data.target_id}] 安装失败: ${event.data.error}`);
      state.activeTaskId = null;
      hideProgress();
      enableAllButtons();
      break;

    // 护栏拒绝：按设计没执行，不是失败，所以不进上面那条红字路径
    case "Guidance":
      handleGuidance(event.data);
      break;

    case "UpdateAvailable":
      resetCheckUpdateButton();
      state.updateInfo = event.data;
      showUpdateModal(event.data);
      appendLog(`🎉 发现新版本: v${event.data.latest_version}（当前 v${event.data.current_version}）`);
      break;

    case "UpdateNotAvailable":
      resetCheckUpdateButton();
      appendLog(`ℹ️ 当前已是最新版本 (v${event.data.current_version})，无需更新。`);
      break;

    case "UpdateError":
      resetCheckUpdateButton();
      appendLog(`⚠️ 检查更新失败: ${event.data.error}`);
      break;
  }
};

function updateProxyBadge() {
  const badge = document.getElementById("proxy-badge");
  const text = document.getElementById("proxy-text");
  badge.classList.toggle("pill-custom", state.proxyMode === "custom");
  badge.classList.toggle("pill-direct", state.proxyMode === "direct");

  let proxyLabel = "";
  if (state.proxyMode === "custom") {
    proxyLabel = state.proxy ? `自定义代理` : "自定义代理未生效";
  } else if (state.proxyMode === "direct") {
    proxyLabel = "直连模式";
  } else if (state.proxy) {
    proxyLabel = `系统代理`;
  } else {
    proxyLabel = "加速集群";
  }

  let ghLabel = "";
  if (state.githubMode === "direct") {
    ghLabel = "GitHub直连";
  } else if (state.githubMode === "auto") {
    ghLabel = "GH自动测速";
  } else {
    try {
      const parsed = new URL(state.githubMirror);
      ghLabel = `GH:${parsed.hostname}`;
    } catch {
      ghLabel = "GH加速";
    }
  }

  text.textContent = `${proxyLabel} · ${ghLabel}`;
  badge.title = `【网络与镜像配置】\n代理: ${state.proxy || "无"} (${state.proxyMode})\nGitHub镜像: ${state.githubMode === "auto" ? "并发自动测速" : state.githubMirror}\nnpm镜像: ${state.npmRegistry}\n点击修改网络与加速镜像配置`;

  // 已保存状态变化时同步弹窗 Tab 摘要
  updateTabValues();
}

// ---- 网络代理与加速镜像配置弹窗 ----
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

/// 内置镜像单选按钮由后端清单渲染（Rust 为唯一数据源）
function renderMirrorOptions() {
  const ghHost = document.getElementById("github-mirror-options");
  const npmHost = document.getElementById("npm-mirror-options");
  if (ghHost) {
    ghHost.innerHTML = state.githubMirrors.map((m) => `
      <label class="proxy-mode-option">
        <input type="radio" name="github-mode" value="${escapeHtml(m.url)}" />
        <span class="proxy-mode-text">
          <strong>🌐 ${escapeHtml(m.name)} (内置节点)</strong>
          <em>${escapeHtml(m.url)} —— ${escapeHtml(m.desc)}</em>
        </span>
      </label>`).join("");
  }
  if (npmHost) {
    npmHost.innerHTML = state.npmMirrors.map((m, i) => `
      <label class="proxy-mode-option">
        <input type="radio" name="npm-registry" value="${escapeHtml(m.url)}" />
        <span class="proxy-mode-text">
          <strong>${i === 0 ? "⚡" : "🌐"} ${escapeHtml(m.name)}${i === 0 ? " (推荐)" : ""}</strong>
          <em>${escapeHtml(m.url)} —— ${escapeHtml(m.desc)}</em>
        </span>
      </label>`).join("");
  }
}

// ---- 弹窗分类 Tab ----
function activateModalTab(name) {
  document.querySelectorAll("#proxy-modal .modal-tab").forEach((t) => {
    const on = t.dataset.tab === name;
    t.classList.toggle("active", on);
    t.setAttribute("aria-selected", on ? "true" : "false");
  });
  document.querySelectorAll("#proxy-modal .modal-tab-panel").forEach((p) => {
    p.classList.toggle("active", p.dataset.panel === name);
  });
}

/// 各 Tab 顶部的当前值摘要，让用户不切换也能看到三类配置现状
function updateTabValues() {
  const set = (id, text) => {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
  };

  const proxyText =
    state.proxyMode === "custom" ? (state.proxy || "未生效")
      : state.proxyMode === "direct" ? "直连"
        : state.proxy ? "已嗅探" : "跟随系统";
  set("tab-value-proxy", proxyText);

  let ghText;
  if (state.githubMode === "direct") {
    ghText = "直连官方";
  } else if (state.githubMode === "auto") {
    ghText = "自动测速";
  } else {
    try {
      ghText = new URL(state.githubMirror).hostname;
    } catch {
      ghText = "自定义";
    }
  }
  set("tab-value-github", ghText);

  let npmText;
  try {
    npmText = new URL(state.npmRegistry).hostname;
  } catch {
    npmText = "自定义";
  }
  set("tab-value-npm", npmText);
}

function getSelectedProxyMode() {
  const checked = document.querySelector('input[name="proxy-mode"]:checked');
  return checked ? checked.value : "system";
}

function getSelectedGithubMode() {
  const checked = document.querySelector('input[name="github-mode"]:checked');
  return checked ? checked.value : "auto";
}

function getSelectedNpmRegistry() {
  const checked = document.querySelector('input[name="npm-registry"]:checked');
  return checked ? checked.value : "https://registry.npmmirror.com";
}

function syncProxyModalUi() {
  const isProxyCustom = getSelectedProxyMode() === "custom";
  document.getElementById("proxy-url-wrap").style.display = isProxyCustom ? "block" : "none";

  const isGhCustom = getSelectedGithubMode() === "custom";
  document.getElementById("github-custom-wrap").style.display = isGhCustom ? "block" : "none";

  const isNpmCustom = getSelectedNpmRegistry() === "custom";
  document.getElementById("npm-custom-wrap").style.display = isNpmCustom ? "block" : "none";

  document.querySelectorAll(".proxy-mode-option").forEach((opt) => {
    const input = opt.querySelector('input[type="radio"]');
    if (input) {
      opt.classList.toggle("selected", input.checked);
    }
  });

  updateTabValuesFromSelection();
}

/// 编辑态摘要：跟随弹窗内当前勾选（而非已保存状态）实时反映
function updateTabValuesFromSelection() {
  const set = (id, text) => {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
  };

  const pm = getSelectedProxyMode();
  set("tab-value-proxy", pm === "custom" ? "自定义" : pm === "direct" ? "直连" : "跟随系统");

  const gh = getSelectedGithubMode();
  if (gh === "auto") {
    set("tab-value-github", "自动测速");
  } else if (gh === "direct") {
    set("tab-value-github", "直连官方");
  } else if (gh === "custom") {
    set("tab-value-github", "自定义");
  } else {
    try {
      set("tab-value-github", new URL(gh).hostname);
    } catch {
      set("tab-value-github", "已选");
    }
  }

  const npm = getSelectedNpmRegistry();
  if (npm === "custom") {
    set("tab-value-npm", "自定义");
  } else {
    try {
      set("tab-value-npm", new URL(npm).hostname);
    } catch {
      set("tab-value-npm", "已选");
    }
  }
}

function openProxyModal() {
  // 兜底重绘：若清单事件早于 DOMContentLoaded 到达，选项容器可能仍为空
  renderMirrorOptions();

  // 1. 代理模式
  document.querySelectorAll('input[name="proxy-mode"]').forEach((r) => {
    r.checked = r.value === state.proxyMode;
  });
  document.getElementById("proxy-url-input").value = state.proxyUrl || "";

  // 2. GitHub 镜像
  let ghMatched = false;
  if (state.githubMode === "auto") {
    const autoRadio = document.querySelector('input[name="github-mode"][value="auto"]');
    if (autoRadio) { autoRadio.checked = true; ghMatched = true; }
  } else if (state.githubMode === "direct") {
    const directRadio = document.querySelector('input[name="github-mode"][value="direct"]');
    if (directRadio) { directRadio.checked = true; ghMatched = true; }
  } else {
    document.querySelectorAll('input[name="github-mode"]').forEach((r) => {
      if (r.value === state.githubMirror) {
        r.checked = true;
        ghMatched = true;
      }
    });
    if (!ghMatched) {
      const customRadio = document.querySelector('input[name="github-mode"][value="custom"]');
      if (customRadio) customRadio.checked = true;
      document.getElementById("github-custom-input").value = state.githubMirror || "";
    }
  }

  // 3. npm 镜像
  let npmMatched = false;
  document.querySelectorAll('input[name="npm-registry"]').forEach((r) => {
    if (r.value === state.npmRegistry) {
      r.checked = true;
      npmMatched = true;
    }
  });
  if (!npmMatched) {
    const customNpmRadio = document.querySelector('input[name="npm-registry"][value="custom"]');
    if (customNpmRadio) customNpmRadio.checked = true;
    document.getElementById("npm-custom-input").value = state.npmRegistry || "";
  }

  syncProxyModalUi();
  activateModalTab("proxy");
  document.getElementById("proxy-modal").classList.add("show");
  if (state.proxyMode === "custom") {
    document.getElementById("proxy-url-input").focus();
  }
}

function closeProxyModal() {
  document.getElementById("proxy-modal").classList.remove("show");
}

function saveProxyConfig() {
  const proxyMode = getSelectedProxyMode();
  const proxyUrl = document.getElementById("proxy-url-input").value.trim();
  if (proxyMode === "custom" && !proxyUrl) {
    appendLog("⚠️ 请填写代理地址，格式如 127.0.0.1:7890 或 http://host:port");
    return;
  }

  const ghVal = getSelectedGithubMode();
  let githubMode = "selected";
  let githubMirror = ghVal;
  if (ghVal === "auto") {
    githubMode = "auto";
    githubMirror = "https://ghproxy.net/";
  } else if (ghVal === "direct") {
    githubMode = "direct";
    githubMirror = "";
  } else if (ghVal === "custom") {
    githubMode = "selected";
    githubMirror = document.getElementById("github-custom-input").value.trim();
    if (!githubMirror) {
      appendLog("⚠️ 请填写自定义 GitHub 加速镜像地址，如 https://ghproxy.net/");
      return;
    }
    if (!githubMirror.endsWith("/")) {
      githubMirror += "/";
    }
  }

  const npmVal = getSelectedNpmRegistry();
  let npmRegistry = npmVal;
  if (npmVal === "custom") {
    npmRegistry = document.getElementById("npm-custom-input").value.trim();
    if (!npmRegistry) {
      appendLog("⚠️ 请填写自定义 npm 镜像源地址，如 https://registry.npmmirror.com");
      return;
    }
  }

  closeProxyModal();
  postCommand("SetNetworkConfig", {
    proxy_mode: proxyMode,
    proxy_url: proxyMode === "custom" ? proxyUrl : null,
    github_mode: githubMode,
    github_mirror: githubMirror,
    npm_registry: npmRegistry,
  });
  appendLog(`🌐 网络与镜像配置已更新并持久化: 代理(${proxyMode}) · GitHub(${githubMode === "auto" ? "自动测速" : githubMirror}) · npm(${npmRegistry})`);
  setTimeout(() => {
    postCommand("Refresh");
  }, 100);
}

function appendLog(line) {
  const consoleEl = document.getElementById("console-output");
  const time = new Date().toLocaleTimeString();
  consoleEl.textContent += `[${time}] ${line}\n`;
  consoleEl.scrollTop = consoleEl.scrollHeight;
}

// —— 护栏指引横幅（AppEvent::Guidance）———————————————————————
// 拒绝执行是护栏按设计行事的结果，不是故障：横幅走中性/黄色样式，卡片状态不转红、
// 不标失败，只让用户看清「DevUp 这次没有代跑」。横幅挂在对应卡片的详情区，
// 不往底部控制台里塞 DOM —— 控制台靠 appendLog 的 textContent += 追加文本，
// 任何子元素都会被下一条日志连带清掉。
function handleGuidance(data) {
  const targetId = data && data.target_id;
  if (!targetId) return;

  // 防御式落库：任务 D 可能只给 reason，commands 为空、note 为 null 都要照常渲染
  state.guidance[targetId] = {
    reason: data.reason || "DevUp 未对该组件执行本次操作。",
    commands: Array.isArray(data.commands) ? data.commands.filter((c) => typeof c === "string" && c.trim()) : [],
    note: data.note || null,
  };

  // 本轮到此为止：不清 activeTaskId 的话卡片按钮会一直停在「正在安装...」，
  // 而护栏拒绝路径不会再来 InstallSuccess / InstallError 兜底
  state.activeTaskId = null;
  hideProgress();
  appendLog(`ℹ️ [${targetId}] 未代为执行：${state.guidance[targetId].reason}`);
  enableAllButtons(); // 内部即 renderAll，横幅随卡片一并重建
}

// 逐字段用 textContent 写入：命令文本来自后端，不能拼进 HTML 字符串
function buildGuidanceBanner(guide) {
  const banner = document.createElement("div");
  banner.className = "guidance-banner";
  banner.setAttribute("role", "note");

  const head = document.createElement("div");
  head.className = "guidance-head";
  head.innerHTML = `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="12" cy="12" r="10"></circle>
              <line x1="12" y1="16" x2="12" y2="12"></line>
              <line x1="12" y1="8" x2="12.01" y2="8"></line>
            </svg>`;
  const headTitle = document.createElement("span");
  headTitle.className = "guidance-title";
  headTitle.textContent = "DevUp 未代为执行";
  head.appendChild(headTitle);
  banner.appendChild(head);

  const reason = document.createElement("div");
  reason.className = "guidance-reason";
  reason.textContent = guide.reason;
  banner.appendChild(reason);

  if (guide.commands.length) {
    const list = document.createElement("div");
    list.className = "guidance-cmds";

    const label = document.createElement("div");
    label.className = "guidance-cmds-label";
    label.textContent = "请在你的终端执行：";
    list.appendChild(label);

    guide.commands.forEach((cmd) => {
      const row = document.createElement("div");
      row.className = "guidance-cmd-row";

      const code = document.createElement("code");
      code.className = "guidance-cmd";
      code.textContent = cmd;

      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "cmd-copy-btn guidance-copy-btn";
      btn.dataset.cmd = cmd;
      btn.title = "复制这条命令";
      btn.innerHTML = `<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
              <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
            </svg>`;
      const btnLabel = document.createElement("span");
      btnLabel.className = "guidance-copy-label";
      btnLabel.textContent = "复制";
      btn.appendChild(btnLabel);

      row.appendChild(code);
      row.appendChild(btn);
      list.appendChild(row);
    });
    banner.appendChild(list);
  }

  if (guide.note) {
    const note = document.createElement("div");
    note.className = "guidance-note";
    note.textContent = guide.note;
    banner.appendChild(note);
  }

  return banner;
}

function attachGuidance(card) {
  const guide = state.guidance[card.dataset.targetId];
  if (!guide) return;
  const footer = card.querySelector(".card-footer");
  // 插在元信息之后、操作按钮之前：属于该组件自己的详情区，与其他卡片互不影响
  card.insertBefore(buildGuidanceBanner(guide), footer);
}

// 与弹窗用的 fallbackCopy 同源，但要拿 execCommand 的返回值：
// 复制到底成没成功决定下一级降级走不走，fallbackCopy 把它吞掉了
function legacyCopyText(text) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch (e) {
    ok = false;
  }
  document.body.removeChild(ta);
  return !!ok;
}

// 最后一道兜底：文本进不了剪贴板就替用户选中，让他自己按复制键
function selectElementText(el) {
  try {
    const range = document.createRange();
    range.selectNodeContents(el);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    return true;
  } catch (e) {
    return false;
  }
}

// wry 的 WebView 里 navigator.clipboard 未必可用（安全上下文 / 权限），execCommand
// 也可能被静默拒绝，故三级降级：clipboard -> execCommand -> 选中文本。
// 复制只影响按钮上的文案，横幅本身在任何分支都已渲染出来，不随复制结果变化。
async function copyGuidanceCommand(btn) {
  const cmd = btn.dataset.cmd || "";
  const label = btn.querySelector(".guidance-copy-label");
  const row = btn.parentElement;
  const codeEl = row ? row.querySelector(".guidance-cmd") : null;
  const feedback = (text) => {
    if (!label) return;
    label.textContent = text;
    btn.classList.add("copied");
    setTimeout(() => {
      btn.classList.remove("copied");
      label.textContent = "复制";
    }, 1500);
  };

  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(cmd);
      feedback("已复制 ✓");
      return;
    }
  } catch (e) {
    // 有些 webview 里光是访问 clipboard 就会抛，落到下一级
  }

  if (legacyCopyText(cmd)) {
    feedback("已复制 ✓");
    return;
  }
  if (codeEl && selectElementText(codeEl)) {
    feedback(isMac ? "已选中 · ⌘C" : "已选中 · Ctrl+C");
    return;
  }
  feedback("可手动选中文本");
}

function updateProgress(data) {
  const container = document.getElementById("progress-container");
  const fill = document.getElementById("progress-fill");
  const textLeft = document.getElementById("progress-text-left");
  const textRight = document.getElementById("progress-text-right");

  container.style.display = "flex";
  fill.style.width = `${Math.min(100, Math.max(0, data.percentage))}%`;

  const downloadedMb = (data.downloaded / (1024 * 1024)).toFixed(1);
  const totalMb = data.total > 0 ? (data.total / (1024 * 1024)).toFixed(1) + " MB" : "未知大小";

  textLeft.textContent = `正在下载 [${data.taskId}]: ${downloadedMb} MB / ${totalMb} (${data.percentage.toFixed(1)}%)`;
  textRight.textContent = `${data.speedMb.toFixed(2)} MB/s`;
}

function hideProgress() {
  const container = document.getElementById("progress-container");
  container.style.display = "none";
}

function resetCheckUpdateButton() {
  const btn = document.getElementById("btn-check-update");
  if (btn) {
    btn.classList.remove("loading");
    const textEl = document.getElementById("btn-update-text");
    if (textEl) textEl.textContent = "检查更新";
  }
}

function showUpdateModal(data) {
  const badge = document.getElementById("update-modal-badge");
  if (badge) badge.textContent = `v${data.latest_version}`;
  const desc = document.getElementById("update-modal-desc");
  if (desc) desc.textContent = `检测到官方发布了新版本（当前运行 v${data.current_version}，最新 v${data.latest_version}）。支持单文件绿色免安装原地自升级与自动重启。`;
  const notes = document.getElementById("update-notes-content");
  if (notes) notes.textContent = data.release_notes || "本次更新包含性能提升与功能优化。";
  const modal = document.getElementById("update-modal");
  if (modal) modal.classList.add("show");
}

function closeUpdateModal() {
  const modal = document.getElementById("update-modal");
  if (modal) modal.classList.remove("show");
}

function disableAllButtons(activeId, actionType = "正在处理...") {
  document.querySelectorAll(".btn-action").forEach((btn) => {
    btn.disabled = true;
    if (btn.dataset.id === activeId) {
      btn.textContent = actionType;
    }
  });
}

function enableAllButtons() {
  renderAll();
}

function getExecutionPlan(item, action) {
  const isUpgrade = action === "upgrade";
  const isUninstall = action === "uninstall";
  const isReinstall = action === "reinstall";

  let tagText = "一键安装";
  let tagClass = "install";
  let title = `确认安装 ${item.name}？`;
  let confirmText = "确认安装";
  let confirmClass = "primary";
  let iconWrapClass = "accent";
  let iconHtml = `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>`;

  if (isUpgrade) {
    tagText = "版本升级";
    tagClass = "upgrade";
    title = `确认升级 ${item.name}？`;
    confirmText = "确认升级";
    confirmClass = "primary";
    iconWrapClass = "success";
    iconHtml = `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M5 12l7-7 7 7"/></svg>`;
  } else if (isReinstall) {
    tagText = "重新安装";
    tagClass = "reinstall";
    title = `确认重新安装 ${item.name}？`;
    confirmText = "确认重装";
    confirmClass = "primary";
    iconWrapClass = "purple";
    iconHtml = `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="23 4 23 10 17 10"></polyline><polyline points="1 20 1 14 7 14"></polyline><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"></path></svg>`;
  } else if (isUninstall) {
    tagText = "卸载清理";
    tagClass = "uninstall";
    title = `确认卸载 ${item.name}？`;
    confirmText = "确认卸载";
    confirmClass = "danger";
    iconWrapClass = "danger";
    iconHtml = `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path><line x1="10" y1="11" x2="10" y2="17"></line><line x1="14" y1="11" x2="14" y2="17"></line></svg>`;
  }

  const isWinget = item.install_method === "winget" || (item.install_method && item.install_method.includes("winget"));
  const proxyNotice = state.proxy ? ` --proxy=${state.proxy}` : "";
  const proxyTip = state.proxy ? `已感知系统代理 ${state.proxy}，DevUp 会自动配置环境变量透传。` : "正在连接国内镜像加速通道进行高速处理。";

  let summary = "";
  let commands = [];
  let notes = "";

  switch (item.id) {
    case "homebrew":
      if (isUninstall) {
        summary = "即将卸载 Homebrew 包管理器。";
        commands = ['/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/uninstall.sh)"'];
        notes = "卸载 Homebrew 会同时移除通过 brew 安装的依赖项。";
      } else {
        summary = "通过清华大学开源镜像站加速安装 Homebrew。";
        commands = [
          'export HOMEBREW_BREW_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/brew.git"',
          'export HOMEBREW_CORE_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/homebrew-core.git"',
          'export HOMEBREW_BOTTLE_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles"',
          '/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"'
        ];
        notes = "DevUp 已为您预置国内清华高速镜像加速变量，安装脚本将在系统终端中执行。";
      }
      break;

    case "windows_terminal":
      {
        const termVer = (item.latest_version || "v1.25.2733.0").replace(/^v/, "");
        if (isUninstall) {
          summary = "即将卸载 Windows Terminal 并从系统中注销应用包。";
          commands = isWinget
            ? ["winget uninstall --id Microsoft.WindowsTerminal --exact --silent"]
            : ['powershell.exe -NoProfile -Command "Get-AppxPackage *WindowsTerminal* | Remove-AppxPackage -ErrorAction SilentlyContinue"'];
          notes = "卸载操作会注销 Windows Terminal AppX 包，不影响现有配置文件。";
        } else if (isUpgrade) {
          summary = `检测到官方新版本 (${item.version || "已装"} → ${item.latest_version || "最新"})，即将执行静默升级。`;
          commands = [
            "winget upgrade --id Microsoft.WindowsTerminal --exact --silent --accept-source-agreements --accept-package-agreements",
            `# 备用回退: powershell.exe -Command "Add-AppxPackage -Path %TEMP%\\devup-cache\\WindowsTerminal_${termVer}.msixbundle"`
          ];
          notes = "DevUp 会优先通过 WinGet 静默升级；若不支持则自动通过加速镜像下载最新 MSIXBundle 覆盖部署。";
        } else {
          summary = `即将通过微软官方原生 MSIXBundle 离线包部署 Windows Terminal (${termVer})。`;
          commands = [
            `# 1. 从加速节点高速下载 Microsoft.WindowsTerminal_${termVer} MSIXBundle`,
            `powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Add-AppxPackage -Path '%TEMP%\\devup-cache\\WindowsTerminal_${termVer}.msixbundle'"`
          ];
          notes = "微软原生 AppX 方式安装，无需提权弹窗，安装完成后开箱即用。";
        }
      }
      break;

    case "powershell7":
      {
        const psVer = (item.latest_version || "v7.4.7").replace(/^v/, "");
        if (isUninstall) {
          summary = "即将从系统中卸载 PowerShell 7 核心运行库。";
          commands = isWinget
            ? ["winget uninstall --id Microsoft.PowerShell --exact --silent"]
            : ['powershell.exe -NoProfile -Command "Get-Package -Name \'PowerShell 7*\' -ErrorAction SilentlyContinue | Uninstall-Package -Force"'];
          notes = "卸载后资源管理器右键快捷菜单的“在 PowerShell 7 中打开”将被移除。";
        } else {
          const opName = isUpgrade ? "版本升级" : (isReinstall ? "重新安装" : "全自动静默安装");
          summary = `通过加速镜像下载最新 MSI (${psVer}) 并执行${opName}。`;
          commands = [
            `# 1. 优选镜像下载: PowerShell-${psVer}-win-x64.msi`,
            `msiexec.exe /i "%TEMP%\\devup-cache\\PowerShell-${psVer}-win-x64.msi" /quiet /norestart ADD_EXPLORER_CONTEXT_MENU_OPENPOWERSHELL=1`
          ];
          notes = "安装参数已启用 ADD_EXPLORER_CONTEXT_MENU，自动集成右键快捷菜单。";
        }
      }
      break;

    case "nodejs":
      {
        const nodeVer = item.latest_version || "v24.22.0";
        if (isUninstall) {
          summary = "即将卸载 Node.js LTS 运行环境。";
          if (item.managed_by) {
            summary = `当前 Node.js 由 ${item.managed_by} 托管。`;
            const currentVer = item.version ? item.version.replace(/^v/i, "") : "";
            commands = currentVer
              ? [`${item.managed_by} uninstall ${currentVer}`]
              : [`${item.managed_by} uninstall`];
            notes = "⚠️ 警告：检测到当前 Node.js 由版本管理器托管，DevUp 不代为删除，建议在终端中执行对应卸载命令。";
          } else if (isMac) {
            // 预览必须与后端同构：brew 装的走 brew 自己的账本，只有官方 PKG 落点才动 /usr/local。
            // 0.3.6 起受版本管理器托管的机器会在护栏处被拒，根本执行不到这里。
            commands = [
              "brew uninstall node",
              '# 备用回退（官方 PKG 装入时）: 清理 /usr/local 落点后 pkgutil --forget org.nodejs.node.pkg（需系统授权，托管路径自动跳过）'
            ];
            notes = "⚠️ 警告：卸载 Node.js 将导致依赖 Node 的所有 AI Coding Agents (Claude Code, Codex, Pi 等) 无法运行。";
          } else {
            commands = ['powershell.exe -NoProfile -Command "Get-Package -Name \'Node.js*\' -ErrorAction SilentlyContinue | Uninstall-Package -Force"'];
            notes = "⚠️ 警告：卸载 Node.js 将导致依赖 Node 的所有 AI Coding Agents (Claude Code, Codex, Pi 等) 无法运行。";
          }
        } else {
          const opName = isUpgrade ? "升级" : (isReinstall ? "重装" : "安装");
          if (isMac) {
            summary = `优先通过 Homebrew ${opName} Node.js LTS (${nodeVer}，清华 bottle 加速)；未装 Homebrew 时回落阿里 npmmirror 官方 PKG。`;
            commands = [
              "brew install node  # 本机已有 node formula 时自动改用 brew upgrade node",
              `# 备用回退（无 Homebrew 时）: 下载 node-${nodeVer}-arm64.pkg 后 /usr/sbin/installer -pkg <file> -target /（弹出系统授权框）`
            ];
            notes = "Node 由版本管理器（nvm / fnm / Volta 等）托管时，DevUp 不代为安装/覆盖，改为给出终端指引命令。";
          } else {
            summary = `从阿里 npmmirror 官方镜像下载 Node.js LTS (${nodeVer}) 并执行${opName}。`;
            commands = [
              `# 1. 下载 node-${nodeVer}-x64.msi`,
              `msiexec.exe /i "%TEMP%\\devup-cache\\node-${nodeVer}-x64.msi" /quiet /norestart`
            ];
            notes = "静默安装后自动写入系统 PATH，提供全局 node 与 npm 命令。";
          }
        }
      }
      break;

    case "git":
      {
        const gitVer = item.latest_version || "最新版";
        if (isMac) {
          // mac 后端没有 winget/Git for Windows 这回事：有 brew 走 brew，没有就唤起 CLT 安装框
          if (isUninstall) {
            summary = "即将通过 Homebrew 卸载 git formula。";
            commands = ["brew uninstall git"];
            notes = "若 git 由 Xcode 命令行工具 (/usr/bin/git) 提供，则随系统管理、不支持单独卸载，DevUp 不会执行删除。";
          } else {
            const opName = isUpgrade ? "升级" : (isReinstall ? "重装" : "安装");
            summary = `通过 Homebrew ${opName} Git (${gitVer}，清华 bottle 加速)。`;
            commands = [
              "brew install git  # 本机已有 git formula 时自动改用 brew upgrade git",
              '# 备用回退（无 Homebrew 时）: xcode-select --install 唤起 Command Line Tools 安装对话框'
            ];
            notes = "Homebrew 路径无需提权；Command Line Tools 由系统安装对话框完成，装完后点「刷新状态」即可看到结果。";
          }
          break;
        }
        if (isUninstall) {
          summary = "即将卸载 Git for Windows 版本管理工具。";
          commands = isWinget
            ? ["winget uninstall --id Git.Git --exact --silent"]
            : ['"C:\\Program Files\\Git\\unins000.exe" /VERYSILENT /NORESTART /SUPPRESSMSGBOXES'];
          notes = "卸载后将移除全局 git 命令与 Git Bash 终端。";
        } else if (isUpgrade) {
          summary = `检测到 Git 新版本 (${item.version || "已装"} → ${gitVer})，即将执行静默更新。`;
          commands = [
            "winget upgrade --id Git.Git --exact --silent --accept-source-agreements --accept-package-agreements",
            '# 备用回退: Git-Installer.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL /SP- /CLOSEAPPLICATIONS'
          ];
          notes = "优先调用 WinGet 原生更新；若未注册则自动通过加速节点下载安装包静默覆盖升级。";
        } else {
          summary = `优选 GitHub 加速镜像下载 Git for Windows (${gitVer}) 并执行静默安装。`;
          commands = [
            `# 1. 从加速节点高速下载 Git for Windows (${gitVer}) 安装包到临时目录`,
            'Git-Installer.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL /SP- /CLOSEAPPLICATIONS'
          ];
          notes = "预设最佳 Windows 编码与换行符配置，自带 Git Credential Manager。";
        }
      }
      break;

    case "claudecode":
      {
        const reg = state.npmRegistry || "https://registry.npmmirror.com";
        if (isUninstall) {
          summary = "即将卸载 Claude Code 全局 npm 包并清理残留二进制。";
          commands = [
            "npm uninstall -g @anthropic-ai/claude-code",
            isMac
              ? "# 自动清理可能存在的原生文件: ~/.local/bin/claude"
              : "# 自动清理可能存在的原生文件: %USERPROFILE%\\.local\\bin\\claude.exe"
          ];
          notes = "卸载后全局 claude 命令将被移除，用户本地配置与项目上下文保持不变。";
        } else if (isUpgrade && item.installed) {
          summary = `即将升级 Claude Code (${item.version || "已装"} → ${item.latest_version || "最新"})。`;
          commands = [
            "claude update",
            `# 备用回退: npm install -g @anthropic-ai/claude-code --registry=${reg}${proxyNotice}`
          ];
          notes = "优先使用 Anthropic 官方内置更新命令 `claude update`；若受阻则自动通过配置的 npm 镜像覆盖。";
        } else {
          summary = `使用配置的 npm 镜像源 (${reg}) 全局安装 Claude Code。`;
          commands = [`npm install -g @anthropic-ai/claude-code --registry=${reg}${proxyNotice}`];
          notes = `${proxyTip} 安装后直接在终端执行 \`claude\` 即可进入交互会话。`;
        }
      }
      break;

    case "codex":
      {
        const reg = state.npmRegistry || "https://registry.npmmirror.com";
        if (isUninstall) {
          summary = "即将卸载 OpenAI Codex 全局 CLI 包。";
          commands = ["npm uninstall -g @openai/codex"];
          notes = "卸载后全局 codex 命令将被移除。";
        } else {
          const opName = isUpgrade ? "升级" : (isReinstall ? "重新安装" : "安装");
          summary = `使用配置的 npm 镜像源 (${reg}) ${opName} OpenAI Codex。`;
          commands = [`npm install -g @openai/codex --registry=${reg}${proxyNotice}`];
          notes = `${proxyTip} 安装完成后终端输入 \`codex\` 即可使用。`;
        }
      }
      break;

    case "pi":
      {
        const reg = state.npmRegistry || "https://registry.npmmirror.com";
        if (isUninstall) {
          summary = "即将卸载 Pi Agent 并清理本地数据缓存。";
          commands = [
            "npm uninstall -g @earendil-works/pi",
            "# 自动清理本地应用数据目录: %LOCALAPPDATA%\\pi"
          ];
          notes = "卸载后全局 pi 命令将被移除。";
        } else if (isUpgrade && item.installed) {
          summary = `即将升级 Pi Agent (${item.version || "已装"} → ${item.latest_version || "最新"})。`;
          commands = [
            "pi update",
            `# 备用回退: npm install -g @earendil-works/pi --registry=${reg}${proxyNotice}`
          ];
          notes = "官方原生更新器会无缝保留您在本地的模型连接配置。";
        } else {
          summary = `使用配置的 npm 镜像源 (${reg}) 安装 Earendil Works Pi Agent。`;
          commands = [`npm install -g @earendil-works/pi --registry=${reg}${proxyNotice}`];
          notes = `${proxyTip} 安装后终端输入 \`pi\` 即可体验统一 Coding Agent 架构。`;
        }
      }
      break;

    case "oh-my-pi":
      {
        const reg = state.npmRegistry || "https://registry.npmmirror.com";
        if (isUninstall) {
          summary = "即将卸载 oh-my-pi (omp) 智能体并清理数据目录。";
          commands = [
            "npm uninstall -g @oh-my-pi/pi-coding-agent",
            "# 自动清理本地原生目录: %LOCALAPPDATA%\\omp"
          ];
          notes = "卸载后全局 omp 命令将被移除。";
        } else if (isUpgrade && item.installed) {
          summary = `即将升级 oh-my-pi (${item.version || "已装"} → ${item.latest_version || "最新"})。`;
          commands = [
            "omp update",
            `# 备用回退: npm install -g @oh-my-pi/pi-coding-agent --registry=${reg}${proxyNotice}`
          ];
          notes = "原生自升级命令会自动同步最新的 subagent 架构与工具链。";
        } else {
          summary = `通过配置的 npm 镜像源 (${reg}) 安装 oh-my-pi (omp) 智能体。`;
          commands = [`npm install -g @oh-my-pi/pi-coding-agent --registry=${reg}${proxyNotice}`];
          notes = `${proxyTip} 安装完成后在终端输入 \`omp\` 即可唤起全功能 AI 编程会话。`;
        }
      }
      break;

    case "opencode":
      {
        const reg = state.npmRegistry || "https://registry.npmmirror.com";
        if (isUninstall) {
          summary = "即将卸载 OpenCode 及其关联的全局 npm 包。";
          commands = [
            "npm uninstall -g @opencode/cli",
            "npm uninstall -g opencode-ai"
          ];
          notes = "同时清理新老版本可能注册的全局包名。";
        } else if (isUpgrade && item.installed) {
          summary = `即将升级 OpenCode (${item.version || "已装"} → ${item.latest_version || "最新"})。`;
          commands = [
            "opencode upgrade",
            `# 备用回退: npm install -g @opencode/cli --registry=${reg}${proxyNotice}`
          ];
          notes = "优先调用官方原生升级命令，保持与官方代码库实时同步。";
        } else {
          summary = `通过配置的 npm 镜像源 (${reg}) 安装 OpenCode 全局 CLI。`;
          commands = [`npm install -g @opencode/cli --registry=${reg}${proxyNotice}`];
          notes = `${proxyTip} 安装后在终端键入 \`opencode\` 即可使用。`;
        }
      }
      break;
    default:
      summary = `即将执行 ${item.name} 的 ${tagText} 操作。`;
      commands = [`# 执行目标: ${item.id} (${action})`];
      notes = "";
      break;
  }

  return {
    summary,
    commands,
    notes,
    tagText,
    tagClass,
    title,
    confirmText,
    confirmClass,
    iconWrapClass,
    iconHtml,
  };
}

function requestAction(id, action) {
  if (state.activeTaskId) return;
  const item = state.softwares.find((s) => s.id === id) || { id, name: id, installed: false };
  // 任务 H：护栏预览会拒的动作不再弹「确认执行」窗——本轮注定拒绝，直接弹管理器指引窗
  // （内容 = 后端下发的同源预览：安装族看 `guard_hint`，卸载看 `uninstall_hint`）。
  // 卸载的特殊性：有 PKG receipt / brew 自管的机器**真会删**，后端那侧预览就是 None，
  // 确认步骤原样保留——弹窗分流完全跟随判定结果，不预设「托管=卸载必拒」。
  // 后端 preflight 仍是最终权威，本窗只是免掉一次「确认了个不会执行的动作」的空转。
  if (id === "nodejs") {
    const hint = action === "uninstall" ? item.uninstall_hint : item.guard_hint;
    if (hint) {
      showGuardHintModal(item, hint);
      return;
    }
  }
  const plan = getExecutionPlan(item, action);
  state.pendingAction = { id, action, item, plan };

  const modal = document.getElementById("confirm-modal");
  const iconWrap = document.getElementById("modal-icon-wrap");
  const title = document.getElementById("modal-title");
  const tag = document.getElementById("modal-action-tag");
  const desc = document.getElementById("modal-desc");
  const cmdContent = document.getElementById("modal-cmd-content");
  const tipText = document.getElementById("modal-tip-text");
  const confirmBtn = document.getElementById("modal-btn-confirm");
  const copyBtn = document.getElementById("modal-btn-copy-cmd");
  const copyLabel = document.getElementById("copy-btn-text");

  if (iconWrap) {
    iconWrap.className = `modal-icon-wrap ${plan.iconWrapClass}`;
    iconWrap.innerHTML = plan.iconHtml;
  }
  if (title) title.textContent = plan.title;
  if (tag) {
    tag.textContent = plan.tagText;
    tag.className = `action-tag ${plan.tagClass}`;
  }
  if (desc) desc.textContent = plan.summary;
  if (cmdContent) cmdContent.textContent = plan.commands.join("\n");
  if (tipText) tipText.textContent = plan.notes;
  if (confirmBtn) {
    confirmBtn.textContent = plan.confirmText;
    confirmBtn.className = `modal-btn ${plan.confirmClass}`;
  }
  if (copyBtn && copyLabel) {
    copyBtn.classList.remove("copied");
    copyLabel.textContent = "复制命令";
  }
  // 指引窗会把这两处文案换掉，普通确认窗每次打开时复位
  const labelText = document.getElementById("modal-cmd-label-text");
  if (labelText) labelText.textContent = "即将执行的操作指令：";

  if (modal) modal.classList.add("show");
}

function closeConfirmModal() {
  state.pendingAction = null;
  state.pendingUninstallId = null;
  const modal = document.getElementById("confirm-modal");
  if (modal) modal.classList.remove("show");
}

// 任务 H：管理器指引窗。复用确认弹窗的 DOM 外壳，配色走 --warning 中性黄
// （与护栏横幅 .guidance-banner 同语义：按设计未代为执行，不是错误）。
// 文案零自造：reason / commands / note 全部来自后端 guard_hint；
// commands 已在后端填好版本（`nvm install 24.22.0`），前端不许再拼命令模板。
function showGuardHintModal(item, hint) {
  const g = hint;
  // pendingAction 保持 plan 形状给 copyModalCommands 用；action 标成 "guidance"，
  // executePendingAction 见此只关窗、不发 IPC
  state.pendingAction = {
    id: item.id,
    action: "guidance",
    item,
    plan: { commands: Array.isArray(g.commands) ? g.commands : [] },
  };

  const modal = document.getElementById("confirm-modal");
  const iconWrap = document.getElementById("modal-icon-wrap");
  const title = document.getElementById("modal-title");
  const tag = document.getElementById("modal-action-tag");
  const desc = document.getElementById("modal-desc");
  const cmdContent = document.getElementById("modal-cmd-content");
  const tipText = document.getElementById("modal-tip-text");
  const confirmBtn = document.getElementById("modal-btn-confirm");
  const copyBtn = document.getElementById("modal-btn-copy-cmd");
  const copyLabel = document.getElementById("copy-btn-text");

  if (iconWrap) {
    iconWrap.className = "modal-icon-wrap warning";
    iconWrap.innerHTML = `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"></circle><line x1="12" y1="16" x2="12" y2="12"></line><line x1="12" y1="8" x2="12.01" y2="8"></line></svg>`;
  }
  if (title) title.textContent = "DevUp 不代为执行";
  if (tag) {
    tag.textContent = item.managed_by ? `由 ${item.managed_by} 托管` : "版本管理器托管";
    tag.className = "action-tag managed";
  }
  // 逐字段 textContent：后端文本不能拼进 HTML（与横幅构建函数同一约束）
  if (desc) desc.textContent = g.reason || "DevUp 未对该组件执行本次操作。";
  if (cmdContent) {
    // 逐条命令各带复制按钮：用户多半只照抄其中一条（install 或 use），
    // 整块复制反而要他回终端删掉另一半。行结构与卡片横幅同源（guidance-cmd-row）
    cmdContent.textContent = "";
    state.pendingAction.plan.commands.forEach((cmd) => {
      const row = document.createElement("div");
      row.className = "guidance-cmd-row";
      const code = document.createElement("code");
      code.className = "guidance-cmd";
      code.textContent = cmd;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "cmd-copy-btn guidance-copy-btn";
      btn.dataset.cmd = cmd;
      btn.title = "复制这条命令";
      btn.innerHTML = `<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2">
              <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
              <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
            </svg>`;
      const btnLabel = document.createElement("span");
      btnLabel.className = "guidance-copy-label";
      btnLabel.textContent = "复制";
      btn.appendChild(btnLabel);
      // 卡片区有 grid 级事件委托，模态框在 grid 之外——这里的按钮自持监听器，
      // 复制逻辑仍走同一个 copyGuidanceCommand（三级降级同源）
      btn.addEventListener("click", () => copyGuidanceCommand(btn));
      row.appendChild(code);
      row.appendChild(btn);
      cmdContent.appendChild(row);
    });
  }
  if (tipText) tipText.textContent = g.note || "";
  const gLabel = document.getElementById("modal-cmd-label-text");
  if (gLabel) gLabel.textContent = "请在你的终端执行：";
  if (confirmBtn) {
    confirmBtn.textContent = "知道了";
    confirmBtn.className = "modal-btn primary";
  }
  if (copyBtn && copyLabel) {
    copyBtn.classList.remove("copied");
    // 这里的整块复制语义是「全部命令」，与逐条按钮并存；普通确认窗由 requestAction 复位为「复制命令」
    copyLabel.textContent = "复制全部";
  }

  if (modal) modal.classList.add("show");
}

function fallbackCopy(text) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  try {
    document.execCommand("copy");
  } catch (e) {
    console.error("复制失败", e);
  }
  document.body.removeChild(ta);
}

function copyModalCommands() {
  if (!state.pendingAction || !state.pendingAction.plan) return;
  const text = state.pendingAction.plan.commands.join("\n");

  const finishCopy = () => {
    const btn = document.getElementById("modal-btn-copy-cmd");
    const label = document.getElementById("copy-btn-text");
    if (btn && label) {
      btn.classList.add("copied");
      label.textContent = "已复制 ✓";
      setTimeout(() => {
        btn.classList.remove("copied");
        label.textContent = "复制命令";
      }, 1500);
    }
  };

  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(finishCopy).catch(() => {
      fallbackCopy(text);
      finishCopy();
    });
  } else {
    fallbackCopy(text);
    finishCopy();
  }
}

function executePendingAction() {
  const pending = state.pendingAction;
  closeConfirmModal();
  if (!pending || state.activeTaskId) return;
  // 任务 H：指引窗的「知道了」只关窗。不发 IPC——护栏在后端仍会拒，
  // 但那趟往返只会把同一份文案再挂成卡片横幅，空转一次任务态而已
  if (pending.action === "guidance") return;

  const { id, action, item } = pending;
  state.activeTaskId = id;

  if (action === "uninstall") {
    disableAllButtons(id, "正在卸载...");
    appendLog(`正在卸载目标软件: ${id} (${item.name || id})...`);
    postCommand("Uninstall", { target_id: id });
  } else {
    const actionLabel = action === "upgrade" ? "升级" : "安装";
    disableAllButtons(id, `正在${actionLabel}...`);
    appendLog(`准备${actionLabel}目标: ${id} (${item.name || id})...`);
    postCommand("Install", { target_id: id });
  }
}

function installTarget(id) {
  const item = state.softwares.find((s) => s.id === id);
  const action = item && item.installed && hasNewerVersion(item.version, item.latest_version)
    ? "upgrade"
    : (item && item.installed ? "reinstall" : "install");
  requestAction(id, action);
}

function requestUninstall(id, name) {
  requestAction(id, "uninstall");
}

function executePendingUninstall() {
  executePendingAction();
}
function getAppSvgIcon(id) {
  switch (id) {
    case "windows_terminal":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"></polyline><line x1="12" y1="19" x2="20" y2="19"></line></svg>`;
    case "powershell7":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>`;
    case "nodejs":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2 3 7v10l9 5 9-5V7z"/><path d="m3 7 9 5 9-5"/><path d="M12 12v10"/></svg>`;
    case "homebrew":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 22h8"/><path d="M7 13.742C7 10.5 10 6.5 12 4c2 2.5 5 6.5 5 9.742C17 16.012 14.54 18 12 18s-5-1.988-5-4.258"/><path d="M7 21c0-1.197.838-2.692 1-3h8c.162.308 1 1.803 1 3"/></svg>`;
    case "git":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><path d="M13 6h3a2 2 0 0 1 2 2v7"/><line x1="6" y1="9" x2="6" y2="21"/></svg>`;
    case "claudecode":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><line x1="9" y1="9" x2="9.01" y2="9"/><line x1="15" y1="9" x2="15.01" y2="9"/></svg>`;
    case "oh-my-pi":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></svg>`;
    case "codex":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m18 16 4-4-4-4"/><path d="m6 8-4 4 4 4"/><path d="m14.5 4-5 16"/></svg>`;
    case "pi":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M8 9h8"/><path d="M10 9v9"/><path d="M14 9v9"/></svg>`;
    case "opencode":
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"></polyline><polyline points="8 6 2 12 8 18"></polyline></svg>`;
    default:
      return `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2" ry="2"></rect></svg>`;
  }
}

function parseVersionNumbers(v) {
  if (!v) return [];
  const matches = v.match(/\d+/g);
  return matches ? matches.map(Number) : [];
}

function hasNewerVersion(currentVer, latestVer) {
  if (!currentVer || !latestVer) return false;
  const cur = parseVersionNumbers(currentVer);
  const lat = parseVersionNumbers(latestVer);
  const minLen = Math.min(cur.length, lat.length);
  for (let i = 0; i < minLen; i++) {
    if (cur[i] < lat[i]) return true;  // 本地比最新版本低，需要升级
    if (cur[i] > lat[i]) return false; // 本地版本更高或等于最新，无需升级
  }
  return lat.length > cur.length;
}
function renderAll() {
  const envContainer = document.getElementById("env-grid");
  const agentContainer = document.getElementById("agent-grid");
  const envSection = envContainer.closest("section");
  const agentSection = agentContainer.closest("section");

  if (state.activeFilter === "env") {
    envSection.style.display = "block";
    agentSection.style.display = "none";
  } else if (state.activeFilter === "agent") {
    envSection.style.display = "none";
    agentSection.style.display = "block";
  } else {
    envSection.style.display = "block";
    agentSection.style.display = "block";
  }

  envContainer.innerHTML = "";
  agentContainer.innerHTML = "";

  state.softwares.forEach((item) => {
    const card = document.createElement("div");
    card.className = "card";
    // 指引横幅按组件定位挂载点，卡片重建后仍能找回自己的那条
    card.dataset.targetId = item.id;

    const isInstalled = item.installed;
    // D4「显示但不代管」：后端认出托管方时，badge 说「托管中」—— 既不是「需更新」也不是「已是最新」，
    // 因为 DevUp 不认为那份 runtime 归自己管。这里**只**改判色，不改 hasUpdate：按钮行保持原判定，
    // 用户点「升级最新」会走护栏拿到「由 nvm 托管，请在终端执行 nvm install …」的指引横幅，
    // 一次点击换来一条正确指引，好过把主行动藏起来（挡掉 hasUpdate 会把卡片推去「卸载」，
    // 而那条在托管机器上同样被 G5 拒绝，是两个都不会执行的动作里挑一个）。
    const isManaged = !!item.managed_by;
    const hasUpdate = isInstalled && hasNewerVersion(item.version, item.latest_version);

    let badgeClass = "badge-missing";
    let badgeText = "未安装";

    if (isManaged) {
      badgeClass = "badge-managed";
      badgeText = "托管中";
    } else if (isInstalled) {
      if (hasUpdate) {
        badgeClass = "badge-update-available";
        badgeText = `可升级 · ${item.version || "已就绪"}`;
      } else {
        badgeClass = "badge-installed";
        badgeText = `最新 · ${item.version || "已就绪"}`;
      }
    }

    // 托管方的版本取自 managed_versions[0]（后端已按语义降序）；一个版本都没扫到就只说托管方名字。
    // 注意这与 item.version 不是一回事：后者是「GUI 真能执行到的那份」，装了 nvm 的 GUI 场景恒为 null。
    const managedVer = item.managed_versions && item.managed_versions.length ? `v${item.managed_versions[0]}` : "";
    const managedText = isManaged ? `由 ${item.managed_by} 托管${managedVer ? ` · ${managedVer}` : ""}` : "";

    const currentChannel = item.install_method ? item.install_method : "未检测到";
    const defaultChannel = item.default_channel || "官方默认通道";
    const latestVerText = item.latest_version || "最新稳定版";

    // 操作按钮区域
    let actionButtonsHtml = "";
    if (!isInstalled) {
      actionButtonsHtml = `
        <button class="btn-action btn-install-primary" data-id="${item.id}" onclick="requestAction('${item.id}', 'install')">
          <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M12 5v14M5 12l7 7 7-7"/></svg>
          一键安装
        </button>
      `;
    } else if (hasUpdate) {
      actionButtonsHtml = `
        <button class="btn-action btn-uninstall" data-id="${item.id}" onclick="requestAction('${item.id}', 'uninstall')">
          卸载
        </button>
        <button class="btn-action btn-upgrade" data-id="${item.id}" onclick="requestAction('${item.id}', 'upgrade')">
          <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M12 19V5M5 12l7-7 7 7"/></svg>
          升级最新
        </button>
      `;
    } else {
      actionButtonsHtml = `
        <button class="btn-action btn-uninstall" data-id="${item.id}" onclick="requestAction('${item.id}', 'uninstall')">
          卸载
        </button>
        <button class="btn-action btn-install-primary" style="background: rgba(255,255,255,0.08); box-shadow:none;" data-id="${item.id}" onclick="requestAction('${item.id}', 'reinstall')">
          重新安装
        </button>
      `;
    }

    card.innerHTML = `
      <div class="card-header">
        <div class="card-title-wrap">
          <div class="card-app-icon">
            ${getAppSvgIcon(item.id)}
          </div>
          <div>
            <div class="card-title">${item.name}</div>
          </div>
        </div>
        <div class="card-header-badges">
          <span class="badge-status ${badgeClass}">${badgeText}</span>
        </div>
      </div>

      <div class="card-desc">${item.description}</div>

      <div class="card-meta-list">
        <div class="meta-row">
          <span class="meta-label">已装版本 / 渠道:</span>
          <span class="meta-val">${isInstalled ? (item.version || "已安装") + ` [${currentChannel}]` : (isManaged ? managedText : "未安装")}</span>
        </div>
        <div class="meta-row">
          <span class="meta-label">官方最新版本:</span>
          <span class="meta-val highlight">${latestVerText}</span>
        </div>
        <div class="meta-row">
          <span class="meta-label">原生升级渠道:</span>
          <span class="meta-val">${defaultChannel}</span>
        </div>
      </div>

      <div class="card-footer">
        <a class="repo-link" href="${item.repo_url}" target="_blank" onclick="event.preventDefault(); openExternal('${item.repo_url}');">
          官网/仓库 ↗
        </a>
        <div class="card-actions">
          ${actionButtonsHtml}
        </div>
      </div>
    `;

    attachGuidance(card);

    if (item.category === "env") {
      envContainer.appendChild(card);
    } else {
      agentContainer.appendChild(card);
    }
  });
}

// 页面加载完成后主动绑定
window.addEventListener("DOMContentLoaded", () => {
  // 分类 Tab 切换
  document.querySelectorAll(".nav-tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      document.querySelectorAll(".nav-tab").forEach((t) => t.classList.remove("active"));
      tab.classList.add("active");
      state.activeFilter = tab.dataset.filter;
      renderAll();
    });
  });

  // 刷新按钮
  const btnRefresh = document.getElementById("btn-refresh");
  btnRefresh.addEventListener("click", () => {
    btnRefresh.classList.add("is-spinning");
    btnRefresh.disabled = true;
    state.isRefreshing = true;
    appendLog("正在重新检测本地环境、组件版本与网络配置...");
    postCommand("Refresh");
    // 安全兜底：10 秒后如未收到响应自动恢复按钮
    setTimeout(() => {
      if (state.isRefreshing) {
        state.isRefreshing = false;
        btnRefresh.classList.remove("is-spinning");
        btnRefresh.disabled = false;
      }
    }, 10000);
  });

  // 代理与镜像配置弹窗
  document.getElementById("proxy-badge").addEventListener("click", openProxyModal);

  // 卡片会被 renderAll 反复重建，复制按钮改用网格上的事件委托，避免重建后失绑
  ["env-grid", "agent-grid"].forEach((gridId) => {
    const grid = document.getElementById(gridId);
    if (!grid) return;
    grid.addEventListener("click", (e) => {
      const btn = e.target.closest ? e.target.closest(".guidance-copy-btn") : null;
      if (btn) copyGuidanceCommand(btn);
    });
  });

  // 弹窗内分类 Tab 切换
  document.querySelectorAll("#proxy-modal .modal-tab").forEach((tab) => {
    tab.addEventListener("click", () => activateModalTab(tab.dataset.tab));
  });

  // 镜像单选按钮由后端清单动态渲染，故用事件委托绑定，避免渲染时机竞态
  const modalEl = document.getElementById("proxy-modal");
  modalEl.addEventListener("change", (e) => {
    const name = e.target && e.target.name;
    if (name === "proxy-mode" || name === "github-mode" || name === "npm-registry") {
      syncProxyModalUi();
    }
  });

  const proxyUrlInput = document.getElementById("proxy-url-input");
  proxyUrlInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") saveProxyConfig();
  });
  const ghCustomInput = document.getElementById("github-custom-input");
  if (ghCustomInput) {
    ghCustomInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") saveProxyConfig();
    });
  }
  const npmCustomInput = document.getElementById("npm-custom-input");
  if (npmCustomInput) {
    npmCustomInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") saveProxyConfig();
    });
  }

  document.getElementById("proxy-btn-cancel").addEventListener("click", closeProxyModal);
  document.getElementById("proxy-btn-save").addEventListener("click", saveProxyConfig);
  const proxyModal = document.getElementById("proxy-modal");
  proxyModal.addEventListener("click", (e) => {
    if (e.target.id === "proxy-modal") closeProxyModal();
  });
  // 检查更新按钮
  const btnCheckUpdate = document.getElementById("btn-check-update");
  if (btnCheckUpdate) {
    btnCheckUpdate.addEventListener("click", () => {
      btnCheckUpdate.classList.add("loading");
      const textEl = document.getElementById("btn-update-text");
      if (textEl) textEl.textContent = "检查中...";
      appendLog("正在连接 GitHub 获取最新版本信息...");
      postCommand("CheckUpdate");
    });
  }

  // 自更新弹窗按钮
  const btnCancelUpdate = document.getElementById("modal-btn-update-cancel");
  if (btnCancelUpdate) {
    btnCancelUpdate.addEventListener("click", closeUpdateModal);
  }
  const btnBrowserUpdate = document.getElementById("modal-btn-update-browser");
  if (btnBrowserUpdate) {
    btnBrowserUpdate.addEventListener("click", () => {
      closeUpdateModal();
      if (state.updateInfo && state.updateInfo.release_url) {
        openExternal(state.updateInfo.release_url);
      }
    });
  }
  const btnConfirmUpdate = document.getElementById("modal-btn-update-confirm");
  if (btnConfirmUpdate) {
    btnConfirmUpdate.addEventListener("click", () => {
      if (!state.updateInfo || !state.updateInfo.download_url) return;
      const downloadUrl = state.updateInfo.download_url;
      closeUpdateModal();
      disableAllButtons("self_update", "正在下载新版本...");
      appendLog(`🚀 开始自更新：正在高速下载 v${state.updateInfo.latest_version}...`);
      postCommand("PerformSelfUpdate", { download_url: downloadUrl });
    });
  }

  // 更新弹窗遮罩点击关闭
  const updateModal = document.getElementById("update-modal");
  if (updateModal) {
    updateModal.addEventListener("click", (e) => {
      if (e.target.id === "update-modal") {
        closeUpdateModal();
      }
    });
  }

  // 模态弹窗事件
  document.getElementById("modal-btn-cancel").addEventListener("click", closeConfirmModal);
  document.getElementById("modal-btn-confirm").addEventListener("click", executePendingAction);
  const btnCopyCmd = document.getElementById("modal-btn-copy-cmd");
  if (btnCopyCmd) {
    btnCopyCmd.addEventListener("click", copyModalCommands);
  }

  // 点击遮罩外部关闭
  document.getElementById("confirm-modal").addEventListener("click", (e) => {
    if (e.target.id === "confirm-modal") {
      closeConfirmModal();
    }
  });

  // 初始加载
  setTimeout(() => {
    postCommand("Refresh");
  }, 200);
});
