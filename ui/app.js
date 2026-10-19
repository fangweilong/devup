// 前端状态与 IPC 桥接
let state = {
  softwares: [],
  proxy: null,
  activeTaskId: null,
  activeFilter: "all",
  pendingUninstallId: null,
  updateInfo: null,
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

// 供 Rust 回调的全局事件接收器
window.onEvent = function (event) {
  switch (event.type) {
    case "StatusList":
      state.softwares = event.data;
      renderAll();
      break;

    case "ProxyDetected":
      state.proxy = event.data.proxy;
      updateProxyBadge();
      break;

    case "Log":
      appendLog(`[${event.data.target_id}] ${event.data.message}`);
      break;

    case "Progress":
      updateProgress(event.data);
      break;

    case "InstallSuccess":
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
  if (state.proxy) {
    text.textContent = `系统代理: ${state.proxy}`;
    badge.title = "已自动感知并配置系统代理，网络请求与 npm 将自动透传";
  } else {
    text.textContent = "国内多源加速集群 (免翻)";
    badge.title = "正在使用国内 GitHub/npm 高速镜像加速下载";
  }
}

function appendLog(line) {
  const consoleEl = document.getElementById("console-output");
  const time = new Date().toLocaleTimeString();
  consoleEl.textContent += `[${time}] ${line}\n`;
  consoleEl.scrollTop = consoleEl.scrollHeight;
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

function installTarget(id) {
  if (state.activeTaskId) return;
  state.activeTaskId = id;
  disableAllButtons(id, "正在安装/升级...");
  appendLog(`准备安装/升级目标: ${id}...`);
  postCommand("Install", { target_id: id });
}

function requestUninstall(id, name) {
  if (state.activeTaskId) return;
  state.pendingUninstallId = id;
  const modal = document.getElementById("confirm-modal");
  const title = document.getElementById("modal-title");
  const desc = document.getElementById("modal-desc");
  title.textContent = `确认卸载 ${name}？`;
  desc.textContent = `系统将清理其已安装的文件与全局环境配置。此操作无法撤销，确定继续吗？`;
  modal.classList.add("show");
}

function closeConfirmModal() {
  state.pendingUninstallId = null;
  const modal = document.getElementById("confirm-modal");
  modal.classList.remove("show");
}

function executePendingUninstall() {
  const id = state.pendingUninstallId;
  closeConfirmModal();
  if (!id || state.activeTaskId) return;
  state.activeTaskId = id;
  disableAllButtons(id, "正在卸载...");
  appendLog(`正在卸载目标软件: ${id}...`);
  postCommand("Uninstall", { target_id: id });
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

    const isInstalled = item.installed;
    const hasUpdate = isInstalled && hasNewerVersion(item.version, item.latest_version);

    let badgeClass = "badge-missing";
    let badgeText = "未安装";

    if (isInstalled) {
      if (hasUpdate) {
        badgeClass = "badge-update-available";
        badgeText = `可升级 · ${item.version || "已就绪"}`;
      } else {
        badgeClass = "badge-installed";
        badgeText = `最新 · ${item.version || "已就绪"}`;
      }
    }

    const currentChannel = item.install_method ? item.install_method : "未检测到";
    const defaultChannel = item.default_channel || "官方默认通道";
    const latestVerText = item.latest_version || "最新稳定版";

    // 操作按钮区域
    let actionButtonsHtml = "";
    if (!isInstalled) {
      actionButtonsHtml = `
        <button class="btn-action btn-install-primary" data-id="${item.id}" onclick="installTarget('${item.id}')">
          <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M12 5v14M5 12l7 7 7-7"/></svg>
          一键安装
        </button>
      `;
    } else if (hasUpdate) {
      actionButtonsHtml = `
        <button class="btn-action btn-uninstall" data-id="${item.id}" onclick="requestUninstall('${item.id}', '${item.name}')">
          卸载
        </button>
        <button class="btn-action btn-upgrade" data-id="${item.id}" onclick="installTarget('${item.id}')">
          <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M12 19V5M5 12l7-7 7 7"/></svg>
          升级最新
        </button>
      `;
    } else {
      actionButtonsHtml = `
        <button class="btn-action btn-uninstall" data-id="${item.id}" onclick="requestUninstall('${item.id}', '${item.name}')">
          卸载
        </button>
        <button class="btn-action btn-install-primary" style="background: rgba(255,255,255,0.08); box-shadow:none;" data-id="${item.id}" onclick="installTarget('${item.id}')">
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
          <span class="meta-val">${isInstalled ? (item.version || "已安装") + ` [${currentChannel}]` : "未安装"}</span>
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
  document.getElementById("btn-refresh").addEventListener("click", () => {
    appendLog("正在重新检测本地环境与网络配置...");
    postCommand("Refresh");
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
  document.getElementById("modal-btn-confirm").addEventListener("click", executePendingUninstall);

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
