// 前端状态与 IPC 桥接
let state = {
  softwares: [],
  proxy: null,
  activeTaskId: null,
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

function disableAllButtons(activeId) {
  document.querySelectorAll(".btn-install").forEach((btn) => {
    btn.disabled = true;
    if (btn.dataset.id === activeId) {
      btn.textContent = "正在安装...";
    }
  });
}

function enableAllButtons() {
  renderAll();
}

function installTarget(id) {
  if (state.activeTaskId) return;
  state.activeTaskId = id;
  disableAllButtons(id);
  appendLog(`准备安装/更新目标: ${id}...`);
  postCommand("Install", { target_id: id });
}

function renderAll() {
  const envContainer = document.getElementById("env-grid");
  const agentContainer = document.getElementById("agent-grid");

  envContainer.innerHTML = "";
  agentContainer.innerHTML = "";

  state.softwares.forEach((item) => {
    const card = document.createElement("div");
    card.className = "card";

    const badgeClass = item.installed ? "badge-installed" : "badge-missing";
    let badgeText = item.installed
      ? `已就绪 ${item.version ? "· " + item.version : ""}`
      : "未安装";

    if (item.installed && item.install_method) {
      badgeText += ` (${item.install_method})`;
    }

    const btnClass = item.installed ? "btn-install secondary" : "btn-install";
    const btnText = item.installed ? "检查更新 / 升级" : "一键安装";
    card.innerHTML = `
      <div class="card-header">
        <div class="card-title-wrap">
          <span class="card-title">${item.name}</span>
        </div>
        <span class="badge-status ${badgeClass}">${badgeText}</span>
      </div>
      <div class="card-desc">${item.description}</div>
      <div class="card-footer">
        <a class="repo-link" href="${item.repo_url}" target="_blank" onclick="event.preventDefault(); openExternal('${item.repo_url}');">
          官网/仓库 ↗
        </a>
        <button class="${btnClass}" data-id="${item.id}" onclick="installTarget('${item.id}')">
          ${btnText}
        </button>
      </div>
    `;

    if (item.category === "env") {
      envContainer.appendChild(card);
    } else {
      agentContainer.appendChild(card);
    }
  });
}

// 页面加载完成后主动请求数据
window.addEventListener("DOMContentLoaded", () => {
  document.getElementById("btn-refresh").addEventListener("click", () => {
    appendLog("正在重新检测本地环境与网络配置...");
    postCommand("Refresh");
  });

  // 初始加载
  setTimeout(() => {
    postCommand("Refresh");
  }, 200);
});
