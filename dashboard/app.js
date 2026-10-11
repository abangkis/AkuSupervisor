const LIFECYCLE_LABELS = {
  stopped: "Berhenti",
  starting: "Memulai",
  running: "Berjalan",
  stopping: "Menghentikan",
  termination_pending: "Menunggu terminasi",
  unhealthy: "Tidak sehat",
  failed: "Gagal",
};

const DESIRED_LABELS = { running: "Berjalan", stopped: "Berhenti" };
const HEALTH_LABELS = { unknown: "Belum diketahui", healthy: "Sehat", unhealthy: "Tidak sehat" };
const HOLD_LABELS = { none: "Tidak ada", running: "Tahan saat berjalan", stopped: "Tahan saat berhenti" };

function normalized(value) {
  return String(value ?? "").trim().toLocaleLowerCase("id-ID");
}

export function isRunning(service) {
  return normalized(service?.lifecycle) === "running";
}

export function applicationUrls(ports) {
  if (!Array.isArray(ports)) return [];
  const seen = new Set();
  const urls = [];
  for (const port of ports) {
    if (!Number.isInteger(port) || port < 1 || port > 65535 || seen.has(port)) continue;
    seen.add(port);
    const url = safeHttpUrl(`http://127.0.0.1:${port}/`);
    if (url) urls.push({ url, port, inferred: true });
  }
  return urls;
}

export function filterServices(services, query = "", mode = "all") {
  const needle = normalized(query).normalize("NFKD").replace(/[\u0300-\u036f]/g, "");
  return (Array.isArray(services) ? services : []).filter((service) => {
    if (mode === "running" && !isRunning(service)) return false;
    if (!needle) return true;
    const searchable = [
      service?.id,
      service?.label,
      service?.lifecycle,
      service?.desiredState,
      service?.health?.status,
      service?.health?.detail,
      service?.rootPid,
      ...(Array.isArray(service?.ownedPids) ? service.ownedPids : []),
      ...(Array.isArray(service?.ports) ? service.ports : []),
      ...applicationUrls(service?.ports).map((item) => item.url),
    ].filter((value) => value !== null && value !== undefined).join(" ")
      .toLocaleLowerCase("id-ID").normalize("NFKD").replace(/[\u0300-\u036f]/g, "");
    return searchable.includes(needle);
  });
}

export function safeHttpUrl(value) {
  if (typeof value !== "string" || value.trim() === "") return null;
  try {
    const parsed = new URL(value);
    if (!["http:", "https:"].includes(parsed.protocol) || !parsed.hostname) return null;
    if (parsed.username || parsed.password) return null;
    return parsed.href;
  } catch {
    return null;
  }
}

export function appUrlLabel(item) {
  const safeUrl = safeHttpUrl(item?.url);
  if (!safeUrl) return null;
  const parsed = new URL(safeUrl);
  const port = item?.port !== null && item?.port !== undefined && String(item.port) !== ""
    ? String(item.port)
    : parsed.port;
  return `${parsed.protocol}//${parsed.hostname}${port ? `:${port}` : ""}`;
}

export function formatTimestamp(value) {
  if (typeof value !== "string" || value.trim() === "") return "Belum tersedia";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Belum tersedia";
  return new Intl.DateTimeFormat("id-ID", {
    dateStyle: "medium",
    timeStyle: "medium",
  }).format(date);
}

export function needsAttention(service) {
  const lifecycle = normalized(service?.lifecycle);
  const health = normalized(service?.health?.status);
  return lifecycle === "unhealthy" || lifecycle === "failed" || health === "unhealthy";
}

function text(value, fallback = "Tidak tersedia") {
  if (value === null || value === undefined || value === "") return fallback;
  if (["string", "number", "boolean"].includes(typeof value)) return String(value);
  return fallback;
}

function enumLabel(value, labels, fallback = "Tidak diketahui") {
  const key = normalized(value);
  return labels[key] ?? (key ? `Nilai: ${text(value)}` : fallback);
}

function makeElement(tag, className, content) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content !== undefined) node.textContent = content;
  return node;
}

function appendCell(row, className = "") {
  const cell = makeElement("td", className);
  row.append(cell);
  return cell;
}

function addMessageRow(message) {
  const row = makeElement("tr", "message-row");
  const cell = makeElement("td", "", message);
  cell.colSpan = 7;
  row.append(cell);
  serviceRows.replaceChildren(row);
}

function addServiceCell(row, service) {
  const cell = appendCell(row);
  cell.append(makeElement("span", "service-name", text(service.label, "Layanan tanpa nama")));
  cell.append(makeElement("span", "service-id", text(service.id, "ID tidak tersedia")));
}

function addStateCell(row, service) {
  const cell = appendCell(row);
  const stack = makeElement("div", "state-stack");
  const lifecycle = normalized(service.lifecycle);
  const badge = makeElement("span", "badge", enumLabel(service.lifecycle, LIFECYCLE_LABELS));
  badge.dataset.state = lifecycle || "unknown";
  stack.append(badge);
  stack.append(makeElement("span", "desired-state", `Tujuan: ${enumLabel(service.desiredState, DESIRED_LABELS)}`));
  cell.append(stack);
}

function addHealthCell(row, service) {
  const cell = appendCell(row, "health-cell");
  const status = normalized(service.health?.status);
  const badge = makeElement("span", "badge", enumLabel(service.health?.status, HEALTH_LABELS));
  badge.dataset.state = status || "unknown";
  cell.append(badge);
  const detail = service.health?.detail;
  if (typeof detail === "string" && detail.trim()) {
    cell.append(makeElement("span", "health-detail", detail));
  }
}

function addUrlsCell(row, service) {
  const cell = appendCell(row);
  const urls = applicationUrls(service.ports);
  if (!urls.length) {
    cell.append(makeElement("span", "empty-value", "Tidak ada URL aplikasi"));
    return;
  }
  const list = makeElement("div", "url-list");
  for (const item of urls) {
    const href = safeHttpUrl(item?.url);
    const label = appUrlLabel(item);
    if (!href || !label) continue;
    const link = makeElement("a", "app-link", label);
    link.href = href;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.setAttribute("aria-label", `Buka aplikasi pada ${label} di tab baru`);
    list.append(link);
    if (item.inferred === true) {
      link.title = "URL dari port terdaftar; layanan yang berhenti mungkin belum dapat dibuka.";
    }
  }
  if (!list.childNodes.length) {
    cell.append(makeElement("span", "empty-value", "URL aplikasi tidak valid"));
  } else {
    cell.append(list);
  }
}

function addPidCell(row, service) {
  const cell = appendCell(row, "pid-value");
  const pid = service.rootPid;
  cell.textContent = typeof pid === "number" && Number.isFinite(pid) ? String(pid) : "Tidak ada";
  if (!(typeof pid === "number" && Number.isFinite(pid))) cell.className += " empty-value";
}

function addProcessesCell(row, service) {
  const cell = appendCell(row);
  const pidsKnown = Array.isArray(service.ownedPids);
  const pids = pidsKnown ? service.ownedPids : [];
  const processesKnown = Array.isArray(service.ownedProcesses);
  const processes = processesKnown ? service.ownedProcesses : [];
  if (pidsKnown && !pids.length && processesKnown && !processes.length) {
    cell.append(makeElement("span", "empty-value", "Tidak ada"));
    return;
  }
  const details = makeElement("details", "process-details");
  details.dataset.serviceId = service.id;
  details.open = expandedServices.has(service.id);
  details.addEventListener("toggle", () => {
    if (!details.isConnected) return;
    if (details.open) expandedServices.add(service.id);
    else expandedServices.delete(service.id);
  });
  const summary = makeElement("summary", "", `Proses (${pidsKnown ? pids.length : "?"}) · lihat detail`);
  details.append(summary);
  const pidLine = makeElement("p", "");
  pidLine.textContent = pidsKnown
    ? `Semua PID dimiliki: ${pids.length ? pids.map((pid) => text(pid)).join(", ") : "tidak ada"}`
    : "Daftar PID tidak tersedia.";
  details.append(pidLine);
  if (!processesKnown) {
    details.append(makeElement("p", "", "Daftar proses tidak tersedia."));
  } else if (processes.length) {
    const list = makeElement("ul", "");
    for (const process of processes) {
      const name = text(process?.name, "Proses tidak diketahui");
      const pid = process?.pid;
      list.append(makeElement("li", "", `${name} · PID ${text(pid)}`));
    }
    details.append(list);
  } else {
    details.append(makeElement("p", "", "Tidak ada identitas proses."));
  }
  cell.append(details);
}

function addHoldCell(row, service) {
  const cell = appendCell(row);
  const hold = normalized(service.operatorHold);
  const value = makeElement("span", "hold-value", enumLabel(service.operatorHold, HOLD_LABELS));
  value.dataset.state = hold || "unknown";
  cell.append(value);
}

function renderRows() {
  if (!hasSnapshot) return;
  // Read current disclosure state before replacing rows: native toggle events
  // can arrive after a search input or refresh has already rendered the table.
  for (const details of serviceRows.querySelectorAll("details[data-service-id]")) {
    if (details.open) expandedServices.add(details.dataset.serviceId);
    else expandedServices.delete(details.dataset.serviceId);
  }
  const filtered = filterServices(snapshot, searchInput.value, filterInput.value);
  if (!snapshot.length) {
    addMessageRow("Belum ada layanan terdaftar.");
  } else if (!filtered.length) {
    addMessageRow("Tidak ada layanan yang cocok dengan pencarian atau filter.");
  } else {
    const fragment = document.createDocumentFragment();
    for (const service of filtered) {
      const row = makeElement("tr", "");
      addServiceCell(row, service);
      addStateCell(row, service);
      addHealthCell(row, service);
      addUrlsCell(row, service);
      addPidCell(row, service);
      addProcessesCell(row, service);
      addHoldCell(row, service);
      fragment.append(row);
    }
    serviceRows.replaceChildren(fragment);
  }
  resultsSummary.textContent = `Menampilkan ${filtered.length} dari ${snapshot.length} layanan.`;
}

function updateCounts(services) {
  countTotal.textContent = String(services.length);
  countRunning.textContent = String(services.filter(isRunning).length);
  countAttention.textContent = String(services.filter(needsAttention).length);
}

function setRefreshState(state, message) {
  refreshStatus.dataset.state = state;
  refreshStatus.textContent = message;
}

async function fetchJson(path) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(path, { method: "GET", headers: { Accept: "application/json" }, signal: controller.signal, cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return await response.json();
  } finally {
    clearTimeout(timeout);
  }
}

function errorMessage(error) {
  if (error?.name === "AbortError") return "Waktu permintaan habis.";
  return error instanceof Error ? error.message : "Terjadi kesalahan saat mengambil data.";
}

function updateSupervisorHealth(result) {
  if (result.status === "rejected") {
    supervisorHealth.textContent = "API Supervisor tidak tersedia";
    supervisorHealth.dataset.state = "unhealthy";
    return;
  }
  const payload = result.value;
  const status = normalized(payload?.status);
  supervisorHealth.dataset.state = ["healthy", "ok", "ready", "up"].includes(status)
    ? "healthy"
    : ["unhealthy", "error", "down", "failed"].includes(status) ? "unhealthy" : "connected";
  supervisorHealth.textContent = status
    ? `API Supervisor · ${enumLabel(status, { healthy: "Sehat", ok: "Sehat", ready: "Siap", up: "Aktif", unhealthy: "Tidak sehat", error: "Galat", down: "Tidak aktif", failed: "Gagal" }, text(status))}`
    : "API Supervisor · Terhubung";
}

async function refresh() {
  if (inFlight) return;
  inFlight = true;
  refreshButton.disabled = true;
  refreshButton.setAttribute("aria-busy", "true");
  if (!hasSnapshot) setRefreshState("loading", "Mengambil data layanan…");
  const [servicesResult, healthResult] = await Promise.allSettled([
    fetchJson("/v1/services"),
    fetchJson("/v1/health"),
  ]);
  updateSupervisorHealth(healthResult);

  if (servicesResult.status === "fulfilled" && Array.isArray(servicesResult.value?.services)) {
    snapshot = servicesResult.value.services;
    hasSnapshot = true;
    updateCounts(snapshot);
    const fetchedAt = new Date().toISOString();
    const formatted = formatTimestamp(fetchedAt);
    updatedAt.textContent = formatted;
    if (formatted !== "Belum tersedia") updatedAt.dateTime = fetchedAt;
    else updatedAt.removeAttribute("datetime");
    renderRows();
    setRefreshState("success", formatted === "Belum tersedia"
      ? "Data layanan berhasil dimuat; waktu pembaruan tidak tersedia."
      : `Data layanan berhasil diperbarui pada ${formatted}.`);
  } else {
    const error = servicesResult.status === "rejected"
      ? errorMessage(servicesResult.reason)
      : "Format respons layanan tidak valid.";
    if (hasSnapshot) {
      setRefreshState("stale", `Pembaruan gagal (${error}); menampilkan data terakhir yang berhasil dimuat.`);
    } else {
      setRefreshState("error", `Data layanan belum tersedia: ${error}`);
      addMessageRow("Data layanan belum tersedia. Periksa koneksi, lalu segarkan kembali.");
    }
  }

  inFlight = false;
  refreshButton.disabled = false;
  refreshButton.removeAttribute("aria-busy");
}

const REQUEST_TIMEOUT_MS = 4500;
const REFRESH_INTERVAL_MS = 5000;
let snapshot = [];
let hasSnapshot = false;
let inFlight = false;
let refreshTimer = null;
const expandedServices = new Set();
let countTotal;
let countRunning;
let countAttention;
let updatedAt;
let refreshButton;
let refreshStatus;
let serviceRows;
let searchInput;
let filterInput;
let supervisorHealth;
let resultsSummary;

if (typeof document !== "undefined" && document.getElementById("service-rows")) {
  countTotal = document.getElementById("count-total");
  countRunning = document.getElementById("count-running");
  countAttention = document.getElementById("count-attention");
  updatedAt = document.getElementById("updated-at");
  refreshButton = document.getElementById("refresh-button");
  refreshStatus = document.getElementById("refresh-status");
  serviceRows = document.getElementById("service-rows");
  searchInput = document.getElementById("search-input");
  filterInput = document.getElementById("service-filter");
  supervisorHealth = document.getElementById("supervisor-health");
  resultsSummary = document.getElementById("results-summary");

  refreshButton.addEventListener("click", () => void refresh());
  searchInput.addEventListener("input", renderRows);
  filterInput.addEventListener("change", renderRows);

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      if (refreshTimer !== null) clearInterval(refreshTimer);
      refreshTimer = null;
    } else {
      void refresh();
      if (refreshTimer === null) refreshTimer = setInterval(() => void refresh(), REFRESH_INTERVAL_MS);
    }
  });

  void refresh();
  refreshTimer = setInterval(() => {
    if (!document.hidden) void refresh();
  }, REFRESH_INTERVAL_MS);
}
