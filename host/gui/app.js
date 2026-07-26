const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const captureModeToggle = document.getElementById('capture-mode-toggle');
const streamingModeToggle = document.getElementById('streaming-mode-toggle');
const monitorRow = document.getElementById('monitor-row');
const windowRow = document.getElementById('window-row');
const publicHostRow = document.getElementById('public-host-row');
const monitorGrid = document.getElementById('monitor-grid');
const windowGrid = document.getElementById('window-grid');
const refreshWindowsButton = document.getElementById('refresh-windows');
const fpsSelect = document.getElementById('fps-select');
const bitrateSlider = document.getElementById('bitrate-slider');
const publicHostInput = document.getElementById('public-host-input');
const settingsCard = document.getElementById('settings-card');
const startStopButton = document.getElementById('start-stop-button');

const statusBlock = document.getElementById('status-block');
const idleBlock = document.getElementById('idle-block');
const roomLine = document.getElementById('room-line');
const roomCodeEl = document.getElementById('room-code');
const watchUrlLine = document.getElementById('watch-url-line');
const watchUrlEl = document.getElementById('watch-url');
const metricsEl = document.getElementById('metrics');
const preparingText = document.getElementById('preparing-text');
const captureWarningEl = document.getElementById('capture-warning');
const errorTextEl = document.getElementById('error-text');

let monitorTargets = [];
let windowTargets = [];
let selectedMonitorIndex = 0;
let selectedWindowIndex = 0;
let monitorCards = [];
let windowCards = [];
let running = false;

function setSegmented(toggle, value) {
  for (const button of toggle.querySelectorAll('button')) {
    button.classList.toggle('active', button.dataset.value === value);
  }
}

function segmentedValue(toggle) {
  return toggle.querySelector('button.active').dataset.value;
}

captureModeToggle.addEventListener('click', (e) => {
  const button = e.target.closest('button');
  if (!button) return;
  setSegmented(captureModeToggle, button.dataset.value);
  const isWindow = button.dataset.value === 'window';
  monitorRow.style.display = isWindow ? 'none' : '';
  windowRow.style.display = isWindow ? '' : 'none';
});

streamingModeToggle.addEventListener('click', (e) => {
  const button = e.target.closest('button');
  if (!button) return;
  setSegmented(streamingModeToggle, button.dataset.value);
  const isMesh = button.dataset.value === 'mesh';
  publicHostRow.style.display = isMesh ? 'none' : '';
});

// Thumbnails are static snapshots (matching Discord's actual picker
// behavior, not continuously-live video): fetched once per card right
// after the target list loads, then cached — re-selecting a card never
// re-captures it.
function buildThumbCard(label) {
  const card = document.createElement('div');
  card.className = 'thumb-card';
  const img = document.createElement('img');
  img.className = 'thumb-image';
  card.appendChild(img);
  const labelEl = document.createElement('div');
  labelEl.className = 'thumb-label';
  labelEl.textContent = label;
  card.appendChild(labelEl);
  return card;
}

function highlightSelected(cards, selectedIndex) {
  cards.forEach((card, i) => card.classList.toggle('selected', i === selectedIndex));
}

function renderMonitorGrid() {
  monitorGrid.innerHTML = '';
  monitorCards = monitorTargets.map((t, i) => {
    const card = buildThumbCard(`${t.outputName} (${t.adapterName})`);
    card.addEventListener('click', () => {
      selectedMonitorIndex = i;
      highlightSelected(monitorCards, selectedMonitorIndex);
    });
    monitorGrid.appendChild(card);
    invoke('capture_monitor_thumbnail', { adapterIndex: t.adapterIndex, outputIndex: t.outputIndex })
      .then((url) => {
        card.querySelector('img').src = url;
      })
      .catch(() => {});
    return card;
  });
  selectedMonitorIndex = Math.min(selectedMonitorIndex, Math.max(monitorTargets.length - 1, 0));
  highlightSelected(monitorCards, selectedMonitorIndex);
}

function renderWindowGrid() {
  windowGrid.innerHTML = '';
  windowCards = windowTargets.map((w, i) => {
    const card = buildThumbCard(w.title);
    card.addEventListener('click', () => {
      selectedWindowIndex = i;
      highlightSelected(windowCards, selectedWindowIndex);
    });
    windowGrid.appendChild(card);
    invoke('capture_window_thumbnail', { hwnd: w.hwnd })
      .then((url) => {
        card.querySelector('img').src = url;
      })
      .catch(() => {});
    return card;
  });
  selectedWindowIndex = Math.min(selectedWindowIndex, Math.max(windowTargets.length - 1, 0));
  highlightSelected(windowCards, selectedWindowIndex);
}

async function refreshWindows() {
  windowTargets = await invoke('list_window_targets');
  renderWindowGrid();
}

refreshWindowsButton.addEventListener('click', refreshWindows);

async function init() {
  const [defaults, monitors, windows] = await Promise.all([
    invoke('get_defaults'),
    invoke('list_monitor_targets'),
    invoke('list_window_targets'),
  ]);

  monitorTargets = monitors;
  windowTargets = windows;
  renderMonitorGrid();
  renderWindowGrid();

  fpsSelect.innerHTML = '';
  defaults.fpsChoices.forEach((f) => {
    const opt = document.createElement('option');
    opt.value = f;
    opt.textContent = f;
    if (f === defaults.defaultFps) opt.selected = true;
    fpsSelect.appendChild(opt);
  });

  bitrateSlider.value = defaults.defaultBitrateMbps;
  publicHostInput.value = defaults.defaultPublicHost;
  setSegmented(streamingModeToggle, defaults.defaultUseMesh ? 'mesh' : 'direct');
  const isMesh = defaults.defaultUseMesh;
  publicHostRow.style.display = isMesh ? 'none' : '';
}

function currentSettings() {
  const captureMode = segmentedValue(captureModeToggle);
  const monitor = monitorTargets[selectedMonitorIndex] ?? { adapterIndex: 0, outputIndex: 0 };
  const win = windowTargets[selectedWindowIndex];
  return {
    captureMode,
    adapterIndex: monitor.adapterIndex,
    outputIndex: monitor.outputIndex,
    windowHwnd: captureMode === 'window' && win ? win.hwnd : null,
    fps: Number(fpsSelect.value),
    bitrateMbps: Number(bitrateSlider.value),
    useMesh: segmentedValue(streamingModeToggle) === 'mesh',
    publicHost: publicHostInput.value,
  };
}

startStopButton.addEventListener('click', async () => {
  if (running) {
    await invoke('stop_streaming');
  } else {
    try {
      await invoke('start_streaming', { settings: currentSettings() });
    } catch (e) {
      errorTextEl.textContent = `Error: ${e}`;
      errorTextEl.style.display = '';
    }
  }
});

document.getElementById('copy-room-code').addEventListener('click', () => {
  invoke('copy_to_clipboard', { text: roomCodeEl.textContent });
});
document.getElementById('copy-watch-url').addEventListener('click', () => {
  invoke('copy_to_clipboard', { text: watchUrlEl.textContent });
});

function applyStatus(status) {
  running = status.running;
  settingsCard.classList.toggle('disabled', running);
  startStopButton.textContent = running ? 'Stop Streaming' : 'Start Streaming';
  startStopButton.classList.toggle('stop', running);

  statusBlock.classList.toggle('visible', running);
  idleBlock.style.display = running ? 'none' : '';

  if (running && status.roomCode) {
    roomLine.style.display = '';
    watchUrlLine.style.display = '';
    roomCodeEl.textContent = status.roomCode;
    watchUrlEl.textContent = status.viewerUrl;
    preparingText.style.display = 'none';
  } else if (running) {
    roomLine.style.display = 'none';
    watchUrlLine.style.display = 'none';
    preparingText.style.display = '';
  } else {
    roomLine.style.display = 'none';
    watchUrlLine.style.display = 'none';
    preparingText.style.display = 'none';
  }

  if (running) {
    metricsEl.textContent =
      `Resolution: ${status.width}x${status.height}   ` +
      `FPS: ${status.measuredFps.toFixed(1)}   ` +
      `Bitrate: ${Math.round(status.currentBitrateBps / 1_000_000)}Mbps`;
  } else {
    metricsEl.textContent = '';
  }

  if (running && status.captureWarning) {
    captureWarningEl.textContent = `⚠ ${status.captureWarning}`;
    captureWarningEl.style.display = '';
  } else {
    captureWarningEl.style.display = 'none';
  }

  if (status.error) {
    errorTextEl.textContent = `Error: ${status.error}`;
    errorTextEl.style.display = '';
  } else {
    errorTextEl.style.display = 'none';
  }
}

listen('host-status', (event) => applyStatus(event.payload));

init();
