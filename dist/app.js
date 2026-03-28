const state = {
  config: null,
  downloads: [],
  filter: "",
  sort: "added",
  direction: "desc",
  view: "expanded",
  refreshTimer: null,
  isRefreshing: false,
};

const elements = {
  queueForm: document.querySelector("#queue-form"),
  queueButton: document.querySelector("#queue-button"),
  sourceInput: document.querySelector("#source-input"),
  outputInput: document.querySelector("#output-input"),
  sourcePicker: document.querySelector("#source-picker"),
  outputPicker: document.querySelector("#output-picker"),
  defaultOutputButton: document.querySelector("#default-output-button"),
  refreshButton: document.querySelector("#refresh-button"),
  searchInput: document.querySelector("#search-input"),
  sortSelect: document.querySelector("#sort-select"),
  directionButton: document.querySelector("#direction-button"),
  viewButtons: [...document.querySelectorAll(".view-button")],
  downloadsList: document.querySelector("#downloads-list"),
  statusBar: document.querySelector(".status-bar"),
  statusText: document.querySelector("#status-text"),
  refreshText: document.querySelector("#refresh-text"),
  metricVisible: document.querySelector("#metric-visible"),
  metricFinished: document.querySelector("#metric-finished"),
  metricSpeed: document.querySelector("#metric-speed"),
  metricPeers: document.querySelector("#metric-peers"),
  dataDir: document.querySelector("#data-dir"),
  defaultDir: document.querySelector("#default-dir"),
  incomingDir: document.querySelector("#incoming-dir"),
};

const tauri = window.__TAURI__ ?? null;
const invoke = tauri?.core?.invoke?.bind(tauri.core);
const dialog = tauri?.dialog ?? null;

bootstrap();

async function bootstrap() {
  bindEvents();

  if (!invoke) {
    setStatus("Tauri API is unavailable in this context.", "error");
    renderDownloads();
    return;
  }

  try {
    state.config = await invoke("get_config");
    hydrateConfig();
    await refreshDownloads();
    state.refreshTimer = window.setInterval(refreshDownloads, 1500);
    setStatus("Desktop shell is ready.", "success");
  } catch (error) {
    setStatus(formatError(error), "error");
  }
}

function bindEvents() {
  elements.queueForm.addEventListener("submit", handleQueueSubmit);
  elements.sourcePicker.addEventListener("click", handleSourcePick);
  elements.outputPicker.addEventListener("click", handleOutputPick);
  elements.defaultOutputButton.addEventListener("click", resetOutputDirectory);
  elements.refreshButton.addEventListener("click", refreshDownloads);
  elements.searchInput.addEventListener("input", (event) => {
    state.filter = event.currentTarget.value;
    renderDownloads();
  });
  elements.sortSelect.addEventListener("change", (event) => {
    state.sort = event.currentTarget.value;
    renderDownloads();
  });
  elements.directionButton.addEventListener("click", () => {
    state.direction = state.direction === "desc" ? "asc" : "desc";
    syncControls();
    renderDownloads();
  });

  for (const button of elements.viewButtons) {
    button.addEventListener("click", () => {
      state.view = button.dataset.view;
      syncControls();
      renderDownloads();
    });
  }
}

function hydrateConfig() {
  if (!state.config) {
    return;
  }

  elements.dataDir.textContent = state.config.data_dir;
  elements.defaultDir.textContent = state.config.default_download_dir;
  elements.incomingDir.textContent = state.config.incoming_torrents_dir;

  if (!elements.outputInput.value.trim()) {
    elements.outputInput.value = state.config.default_download_dir;
  }
}

async function handleQueueSubmit(event) {
  event.preventDefault();

  const source = elements.sourceInput.value.trim();
  const outputDir = elements.outputInput.value.trim();

  if (!source) {
    setStatus("Torrent source is required.", "error");
    elements.sourceInput.focus();
    return;
  }

  elements.queueButton.disabled = true;
  elements.queueButton.textContent = "Queuing...";

  try {
    const result = await invoke("add_torrent", {
      source,
      outputDir: outputDir || null,
    });
    elements.sourceInput.value = "";
    setStatus(`Torrent #${result.id} queued successfully.`, "success");
    await refreshDownloads();
  } catch (error) {
    setStatus(formatError(error), "error");
  } finally {
    elements.queueButton.disabled = false;
    elements.queueButton.textContent = "Start Download";
  }
}

async function handleSourcePick() {
  if (!dialog?.open) {
    setStatus("Native file dialog is unavailable.", "error");
    return;
  }

  try {
    const selected = await dialog.open({
      multiple: false,
      directory: false,
      filters: [{ name: "Torrent", extensions: ["torrent"] }],
    });

    if (typeof selected === "string") {
      elements.sourceInput.value = selected;
      setStatus("Selected a local .torrent file.", "info");
    }
  } catch (error) {
    setStatus(formatError(error), "error");
  }
}

async function handleOutputPick() {
  if (!dialog?.open) {
    setStatus("Native folder dialog is unavailable.", "error");
    return;
  }

  try {
    const selected = await dialog.open({
      multiple: false,
      directory: true,
      defaultPath:
        elements.outputInput.value.trim() ||
        state.config?.default_download_dir ||
        undefined,
    });

    if (typeof selected === "string") {
      elements.outputInput.value = selected;
      setStatus("Selected output directory.", "info");
    }
  } catch (error) {
    setStatus(formatError(error), "error");
  }
}

function resetOutputDirectory() {
  if (!state.config) {
    return;
  }

  elements.outputInput.value = state.config.default_download_dir;
  setStatus("Output directory reset to the app default.", "info");
}

async function refreshDownloads() {
  if (!invoke || state.isRefreshing) {
    return;
  }

  state.isRefreshing = true;

  try {
    const downloads = await invoke("list_downloads");
    state.downloads = Array.isArray(downloads) ? downloads : [];
    elements.refreshText.textContent = `Last refresh ${formatTime(new Date())}`;
    renderDownloads();
  } catch (error) {
    setStatus(formatError(error), "error");
  } finally {
    state.isRefreshing = false;
  }
}

function renderDownloads() {
  syncControls();

  const downloads = getVisibleDownloads();
  const finishedCount = state.downloads.filter((download) => download.finished).length;
  const aggregateSpeed = state.downloads.reduce(
    (total, download) => total + (Number(download.download_speed_mib) || 0),
    0,
  );
  const aggregatePeers = state.downloads.reduce(
    (total, download) => total + (Number(download.live_peers) || 0),
    0,
  );

  elements.metricVisible.textContent = String(downloads.length);
  elements.metricFinished.textContent = String(finishedCount);
  elements.metricSpeed.textContent = `${aggregateSpeed.toFixed(2)} MiB/s`;
  elements.metricPeers.textContent = String(aggregatePeers);

  elements.downloadsList.dataset.view = state.view;

  if (downloads.length === 0) {
    const copy =
      state.downloads.length === 0
        ? {
            title: "Nothing is downloading yet",
            text: "Queue a local .torrent file, an HTTP link to a .torrent file, or a magnet link from the panel on the left.",
          }
        : {
            title: "No download matches this filter",
            text: "Adjust the search terms, change the sort mode, or clear the filter to bring torrents back into view.",
          };

    elements.downloadsList.innerHTML = `
      <article class="empty-state">
        <h3>${escapeHtml(copy.title)}</h3>
        <p>${escapeHtml(copy.text)}</p>
      </article>
    `;
    return;
  }

  elements.downloadsList.innerHTML = downloads
    .map((download) => renderDownloadCard(download))
    .join("");
}

function renderDownloadCard(download) {
  const stateClass = download.finished
    ? "state-finished"
    : download.error
      ? "state-error"
      : download.download_speed_mib > 0
        ? "state-working"
        : "state-live";
  const progressPercent = clamp(Number(download.progress_ratio) * 100, 0, 100);
  const compactClass = state.view === "compact" ? "compact" : "expanded";

  return `
    <article class="download-card ${compactClass}">
      <div>
        <div class="download-top">
          <div>
            <h3 class="download-title">${escapeHtml(download.name)}</h3>
          </div>
          <span class="state-pill ${stateClass}">${escapeHtml(download.state)}</span>
        </div>

        <div class="download-meta">
          <span>#${download.id}</span>
          <span>${progressPercent.toFixed(1)}%</span>
        </div>

        <div class="download-progress">
          <div class="progress-track">
            <div class="progress-fill" style="width: ${progressPercent.toFixed(1)}%"></div>
          </div>
          <div class="progress-caption">
            <span>${escapeHtml(formatBytes(download.progress_bytes))} downloaded</span>
            <span>${escapeHtml(formatBytes(download.total_bytes))} total</span>
          </div>
        </div>
      </div>

      <div>
        <div class="download-metrics">
          <article class="metric-chip">
            <span>Down</span>
            <strong>${escapeHtml(download.download_speed ?? "n/a")}</strong>
          </article>
          <article class="metric-chip">
            <span>Up</span>
            <strong>${escapeHtml(download.upload_speed ?? "n/a")}</strong>
          </article>
          <article class="metric-chip">
            <span>Peers</span>
            <strong>${download.live_peers}</strong>
          </article>
          <article class="metric-chip">
            <span>Seen</span>
            <strong>${download.seen_peers}</strong>
          </article>
        </div>

        <div class="download-paths">
          <div class="download-path">
            <strong>Source</strong>${escapeHtml(download.source)}
          </div>
          <div class="download-path">
            <strong>Output</strong>${escapeHtml(download.output_dir)}
          </div>
          ${
            download.error
              ? `<div class="download-path"><strong>Error</strong>${escapeHtml(download.error)}</div>`
              : ""
          }
        </div>
      </div>
    </article>
  `;
}

function getVisibleDownloads() {
  const filter = state.filter.trim().toLowerCase();
  const terms = filter ? filter.split(/\s+/).filter(Boolean) : [];

  const filtered = state.downloads.filter((download) => {
    if (terms.length === 0) {
      return true;
    }

    const haystack = [
      download.name,
      download.state,
      download.source,
      download.output_dir,
    ]
      .join(" ")
      .toLowerCase();

    return terms.every((term) => haystack.includes(term));
  });

  return filtered.sort(compareDownloads);
}

function compareDownloads(left, right) {
  let result = 0;

  switch (state.sort) {
    case "speed":
      result = compareNumber(left.download_speed_mib, right.download_speed_mib);
      break;
    case "progress":
      result = compareNumber(left.progress_ratio, right.progress_ratio);
      break;
    case "name":
      result = String(left.name).localeCompare(String(right.name));
      break;
    case "state":
      result = String(left.state).localeCompare(String(right.state));
      break;
    case "added":
    default:
      result = compareNumber(left.id, right.id);
      break;
  }

  if (result === 0) {
    result = compareNumber(left.id, right.id);
  }

  return state.direction === "desc" ? result * -1 : result;
}

function compareNumber(left, right) {
  return Number(left) - Number(right);
}

function syncControls() {
  elements.sortSelect.value = state.sort;
  elements.searchInput.value = state.filter;
  elements.directionButton.textContent =
    state.direction === "desc" ? "Descending" : "Ascending";

  for (const button of elements.viewButtons) {
    button.classList.toggle("is-active", button.dataset.view === state.view);
  }
}

function setStatus(message, tone = "info") {
  elements.statusText.textContent = message;
  elements.statusBar.classList.remove("success", "error", "info");
  elements.statusBar.classList.add(tone);
}

function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  if (value === 0) {
    return "0 B";
  }

  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let size = value;
  let unit = 0;

  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }

  return unit === 0 ? `${Math.round(size)} ${units[unit]}` : `${size.toFixed(1)} ${units[unit]}`;
}

function formatTime(date) {
  return new Intl.DateTimeFormat("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(date);
}

function formatError(error) {
  if (typeof error === "string") {
    return error;
  }

  if (error && typeof error === "object") {
    if (typeof error.message === "string") {
      return error.message;
    }

    return JSON.stringify(error);
  }

  return "Unknown error";
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}
