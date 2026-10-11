const LIFECYCLE_LABELS = {
  stopped: "Stopped",
  starting: "Starting",
  running: "Running",
  stopping: "Stopping",
  termination_pending: "Termination pending",
  unhealthy: "Unhealthy",
  failed: "Failed",
};

const DESIRED_LABELS = { running: "Running", stopped: "Stopped" };
const HEALTH_LABELS = { unknown: "Unknown", healthy: "Healthy", unhealthy: "Unhealthy" };
const HOLD_LABELS = { none: "None", running: "Hold while running", stopped: "Hold while stopped" };
const REQUEST_TIMEOUT_MS = 4500;
const MUTATION_TIMEOUT_MS = 180000;
const REFRESH_INTERVAL_MS = 5000;

function normalized(value) {
  return String(value ?? "").trim().toLocaleLowerCase("en-US");
}

export function isRunning(service) {
  return normalized(service?.lifecycle) === "running";
}

function hasOwnedProcesses(service) {
  if (Number.isSafeInteger(service?.rootPid) && service.rootPid > 0) return true;
  return Array.isArray(service?.ownedPids)
    && service.ownedPids.some((pid) => Number.isSafeInteger(pid) && pid > 0);
}

function ownershipIsKnownEmpty(service) {
  return service?.rootPid === null
    && Array.isArray(service?.ownedPids)
    && service.ownedPids.length === 0;
}

export function actionForService(service) {
  const lifecycle = normalized(service?.lifecycle);
  const owned = hasOwnedProcesses(service);
  if ((lifecycle === "stopped" || lifecycle === "failed") && ownershipIsKnownEmpty(service)) return "start";
  if ((lifecycle === "running" || lifecycle === "unhealthy" || lifecycle === "failed") && owned) return "stop";
  return null;
}

export function actionsForService(service) {
  const action = actionForService(service);
  if (!action) return [];
  return action === "stop" && normalized(service?.lifecycle) === "running"
    ? ["stop", "restart"]
    : [action];
}

export function isActionDisabled(service, state = {}, action = actionForService(service)) {
  return !action || !actionsForService(service).includes(action)
    || state.snapshotFresh !== true
    || state.sessionAvailable !== true
    || state.mutationPending === true
    || state.refreshRequired === true
    || state.refreshInFlight === true;
}

export function applicationUrls(ports) {
  if (!Array.isArray(ports)) return [];
  const seen = new Set();
  const urls = [];
  for (const port of ports) {
    if (!Number.isInteger(port) || port < 1 || port > 65535 || seen.has(port)) continue;
    seen.add(port);
    const url = safeHttpUrl("http://127.0.0.1:" + port + "/");
    if (url) urls.push({ url, port, inferred: true });
  }
  return urls;
}

export function filterServices(services, query = "", mode = "all", retainedInRunningFilter = new Set()) {
  const needle = normalized(query).normalize("NFKD").replace(/[\u0300-\u036f]/g, "");
  const retained = retainedInRunningFilter instanceof Set ? retainedInRunningFilter : new Set();
  return (Array.isArray(services) ? services : []).filter((service) => {
    const serviceId = service?.id === null || service?.id === undefined ? null : String(service.id);
    if (mode === "running" && !isRunning(service) && !retained.has(serviceId)) return false;
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
      .toLocaleLowerCase("en-US").normalize("NFKD").replace(/[\u0300-\u036f]/g, "");
    return searchable.includes(needle);
  });
}

export function reconcileRunningRetention(retainedIds, services) {
  const retained = new Set(retainedIds instanceof Set ? [...retainedIds].map(String) : []);
  const current = new Map((Array.isArray(services) ? services : [])
    .filter((service) => service?.id !== null && service?.id !== undefined)
    .map((service) => [String(service.id), service]));
  return new Set([...retained].filter((id) => current.has(id) && !isRunning(current.get(id))));
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
  return parsed.protocol + "//" + parsed.hostname + (port ? ":" + port : "");
}

export function formatTimestamp(value) {
  if (typeof value !== "string" || value.trim() === "") return "Not available";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Not available";
  return new Intl.DateTimeFormat("en-US", {
    dateStyle: "medium",
    timeStyle: "medium",
  }).format(date);
}

export function needsAttention(service) {
  const lifecycle = normalized(service?.lifecycle);
  const health = normalized(service?.health?.status);
  return lifecycle === "unhealthy" || lifecycle === "failed" || health === "unhealthy";
}

function text(value, fallback = "Not available") {
  if (value === null || value === undefined || value === "") return fallback;
  if (["string", "number", "boolean"].includes(typeof value)) return String(value);
  return fallback;
}

function enumLabel(value, labels, fallback = "Unknown") {
  const key = normalized(value);
  return labels[key] ?? (key ? "Value: " + text(value) : fallback);
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
  cell.colSpan = 8;
  row.append(cell);
  serviceRows.replaceChildren(row);
}

function addServiceCell(row, service) {
  const cell = appendCell(row);
  cell.append(makeElement("span", "service-name", text(service.label, "Unnamed service")));
  cell.append(makeElement("span", "service-id", text(service.id, "ID not available")));
}

function addStateCell(row, service) {
  const cell = appendCell(row);
  const stack = makeElement("div", "state-stack");
  const lifecycle = normalized(service.lifecycle);
  const badge = makeElement("span", "badge", enumLabel(service.lifecycle, LIFECYCLE_LABELS));
  badge.dataset.state = lifecycle || "unknown";
  stack.append(badge);
  if (runningFilterRetained.has(String(service.id)) && !isRunning(service)) {
    stack.append(makeElement("span", "restart-recovery", "Restart recovery"));
  }
  stack.append(makeElement("span", "desired-state", "Desired: " + enumLabel(service.desiredState, DESIRED_LABELS)));
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
    cell.append(makeElement("span", "empty-value", "No application URL"));
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
    link.setAttribute("aria-label", "Open application at " + label + " in a new tab");
    list.append(link);
    if (item.inferred === true) {
      link.title = "This URL is inferred from a registered port; a stopped service may not be reachable.";
    }
  }
  if (!list.childNodes.length) {
    cell.append(makeElement("span", "empty-value", "No valid application URL"));
  } else {
    cell.append(list);
  }
}

function addPidCell(row, service) {
  const cell = appendCell(row, "pid-value");
  const pid = service.rootPid;
  cell.textContent = typeof pid === "number" && Number.isFinite(pid) ? String(pid) : "None";
  if (!(typeof pid === "number" && Number.isFinite(pid))) cell.className += " empty-value";
}

function addProcessesCell(row, service) {
  const cell = appendCell(row);
  const pidsKnown = Array.isArray(service.ownedPids);
  const pids = pidsKnown ? service.ownedPids : [];
  const processesKnown = Array.isArray(service.ownedProcesses);
  const processes = processesKnown ? service.ownedProcesses : [];
  if (pidsKnown && !pids.length && processesKnown && !processes.length) {
    cell.append(makeElement("span", "empty-value", "None"));
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
  const summary = makeElement("summary", "", "Processes (" + (pidsKnown ? pids.length : "?") + ") · view details");
  details.append(summary);
  const pidLine = makeElement("p", "");
  pidLine.textContent = pidsKnown
    ? "All owned PIDs: " + (pids.length ? pids.map((pid) => text(pid)).join(", ") : "none")
    : "PID list is not available.";
  details.append(pidLine);
  if (!processesKnown) {
    details.append(makeElement("p", "", "Process list is not available."));
  } else if (processes.length) {
    const list = makeElement("ul", "");
    for (const process of processes) {
      const name = text(process?.name, "Unknown process");
      const pid = process?.pid;
      list.append(makeElement("li", "", name + " · PID " + text(pid)));
    }
    details.append(list);
  } else {
    details.append(makeElement("p", "", "No process identities."));
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

function actionState() {
  return {
    snapshotFresh,
    sessionAvailable: Boolean(dashboardSession),
    mutationPending: Boolean(pendingMutation),
    refreshRequired: mutationRefreshRequired,
    refreshInFlight,
  };
}

function addActionCell(row, service) {
  const cell = appendCell(row, "action-cell");
  const actions = actionsForService(service);
  const serviceName = text(service.label, text(service.id, "service"));
  if (!actions.length) {
    const button = makeElement("button", "service-action");
    button.type = "button";
    button.setAttribute("aria-describedby", "action-status");
    button.classList.add("action-unavailable");
    button.textContent = "Unavailable";
    button.setAttribute("aria-label", "No Start, Stop, or Restart action available for " + serviceName);
    button.title = "This service state or process ownership does not allow a safe action.";
    button.disabled = true;
    cell.append(button);
    return;
  }

  const actionNames = { start: "Start", stop: "Stop", restart: "Restart" };
  const pendingVerbs = { start: "Starting", stop: "Stopping", restart: "Restarting" };
  for (const action of actions) {
    const actionName = actionNames[action];
    const button = makeElement("button", "service-action action-" + action);
    const isPending = pendingMutation?.serviceId === service.id && pendingMutation.action === action;
    button.type = "button";
    button.setAttribute("aria-describedby", "action-status");
    if (isPending) {
      const pendingVerb = pendingVerbs[action];
      button.classList.add("action-pending");
      button.textContent = pendingVerb + "…";
      button.setAttribute("aria-label", pendingVerb + " " + serviceName);
      button.disabled = true;
    } else {
      button.textContent = actionName;
      button.setAttribute("aria-label", actionName + " " + serviceName);
      if (isActionDisabled(service, actionState(), action)) {
        button.disabled = true;
        button.title = !dashboardSession
          ? "Dashboard access is not available."
          : !snapshotFresh || mutationRefreshRequired
            ? "Refresh service status before taking action."
            : pendingMutation
              ? "Another service action is in progress."
              : "Service status is being refreshed.";
      } else {
        button.addEventListener("click", () => void mutateService(service, action));
      }
    }
    cell.append(button);
  }
}

function renderRows() {
  if (!hasSnapshot) return;
  for (const details of serviceRows.querySelectorAll("details[data-service-id]")) {
    if (details.open) expandedServices.add(details.dataset.serviceId);
    else expandedServices.delete(details.dataset.serviceId);
  }
  const filtered = filterServices(snapshot, searchInput.value, filterInput.value, runningFilterRetained);
  if (!snapshot.length) {
    addMessageRow("No services are registered.");
  } else if (!filtered.length) {
    addMessageRow("No services match your search or filter.");
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
      addActionCell(row, service);
      fragment.append(row);
    }
    serviceRows.replaceChildren(fragment);
  }
  resultsSummary.textContent = "Showing " + filtered.length + " of " + snapshot.length + " services.";
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

function setActionStatus(state, message) {
  actionStatus.dataset.state = state;
  actionStatus.textContent = message;
}

async function fetchJson(path, options = {}, timeoutMs = REQUEST_TIMEOUT_MS) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const headers = { Accept: "application/json", ...(options.headers ?? {}) };
    const response = await fetch(path, {
      method: "GET",
      ...options,
      headers,
      signal: controller.signal,
      cache: "no-store",
    });
    if (!response.ok) throw new Error("HTTP " + response.status);
    return await response.json();
  } finally {
    clearTimeout(timeout);
  }
}

async function readResponseJson(response) {
  try {
    return await response.json();
  } catch {
    return null;
  }
}

function errorMessage(error) {
  if (error?.name === "AbortError") return "The request timed out.";
  return error instanceof Error ? error.message : "An error occurred while loading data.";
}

function validSession(payload) {
  return typeof payload?.token === "string"
    && /^[0-9a-f]{64}$/i.test(payload.token)
    && typeof payload?.instanceId === "string"
    && payload.instanceId.trim().length > 0;
}

async function fetchDashboardSession(force = false) {
  if (dashboardSession && !force) return dashboardSession;
  if (sessionPromise) return sessionPromise;
  dashboardSession = null;
  sessionPromise = fetchJson("/dashboard/v1/session", {
    headers: { "X-Dashboard-Request": "1" },
  }).then((payload) => {
    if (!validSession(payload)) throw new Error("Invalid dashboard session response.");
    dashboardSession = { token: payload.token, instanceId: payload.instanceId };
    if (actionStatus.dataset.state === "unavailable") setActionStatus("idle", "");
    return dashboardSession;
  }).finally(() => {
    sessionPromise = null;
  });
  return sessionPromise;
}

function updateSupervisorHealth(result) {
  if (result.status === "rejected") {
    supervisorHealth.textContent = "Supervisor API is unavailable";
    supervisorHealth.dataset.state = "unhealthy";
    return;
  }
  const payload = result.value;
  const status = normalized(payload?.status);
  supervisorHealth.dataset.state = ["healthy", "ok", "ready", "up"].includes(status)
    ? "healthy"
    : ["unhealthy", "error", "down", "failed"].includes(status) ? "unhealthy" : "connected";
  supervisorHealth.textContent = status
    ? "Supervisor API · " + enumLabel(status, {
      healthy: "Healthy",
      ok: "Healthy",
      ready: "Ready",
      up: "Online",
      unhealthy: "Unhealthy",
      error: "Error",
      down: "Offline",
      failed: "Failed",
    }, text(status))
    : "Supervisor API · Connected";
}

function createRequestId() {
  const bytes = new Uint8Array(18);
  if (globalThis.crypto?.getRandomValues) globalThis.crypto.getRandomValues(bytes);
  else {
    const seed = Date.now() + Math.floor(Math.random() * 0x7fffffff);
    for (let index = 0; index < bytes.length; index += 1) bytes[index] = (seed >> (index % 4 * 8)) & 0xff;
  }
  const suffix = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return "dashboard-" + Date.now().toString(36) + "-" + suffix;
}

async function mutateService(service, action) {
  if (isActionDisabled(service, actionState(), action)) return;
  const serviceId = text(service.id, "");
  const serviceName = text(service.label, serviceId || "service");
  const actionLabel = { start: "Start", stop: "Stop", restart: "Restart" }[action];
  const requestId = createRequestId();
  if (action === "restart") runningFilterRetained.add(serviceId);
  pendingMutation = { serviceId, action };
  mutationRefreshRequired = true;
  snapshotFresh = false;
  refreshButton.disabled = true;
  setActionStatus("progress", "Sending " + actionLabel.toLowerCase() + " request for " + serviceName + "…");
  renderRows();

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), MUTATION_TIMEOUT_MS);
  try {
    const session = dashboardSession;
    const response = await fetch(
      "/dashboard/v1/services/" + encodeURIComponent(serviceId) + "/" + action,
      {
        method: "POST",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-Dashboard-Token": session.token,
        },
        body: JSON.stringify({ requestId }),
        signal: controller.signal,
        cache: "no-store",
      },
    );
    const payload = await readResponseJson(response);
    if (response.status === 401) {
      dashboardSession = null;
      setActionStatus("error", "The dashboard session expired. The " + actionLabel.toLowerCase()
        + " command was not retried; its outcome may be unknown. Refreshing the session and service status.");
      try {
        await fetchDashboardSession(true);
      } catch {
        setActionStatus("error", "The dashboard session expired. The " + actionLabel.toLowerCase()
          + " command was not retried and its outcome is unknown. Session access could not be renewed; refresh service status before acting.");
      }
    } else if (!response.ok) {
      const code = text(payload?.error?.code, "request_failed");
      const message = text(payload?.error?.message, "HTTP " + response.status);
      setActionStatus("error", actionLabel + " request for " + serviceName + " failed (" + code + "): "
        + message + " Refreshing service status.");
    } else if (!payload || payload.serviceId !== serviceId || payload.outcome === undefined) {
      setActionStatus("error", actionLabel + " request for " + serviceName
        + " returned an unreadable result. Its outcome is unknown; refreshing service status.");
    } else {
      setActionStatus("result", actionLabel + " request for " + serviceName + " returned outcome “"
        + text(payload.outcome) + "”. Refreshing service status.");
    }
  } catch (error) {
    if (error?.name === "AbortError") {
      setActionStatus("error", actionLabel + " request timed out after 180 seconds. Its outcome is unknown. "
        + "Refreshing service status; the command will not be retried.");
    } else {
      setActionStatus("error", actionLabel + " request for " + serviceName
        + " did not receive a response. Its outcome is unknown. Refreshing service status; the command will not be retried.");
    }
  } finally {
    clearTimeout(timeout);
    pendingMutation = null;
    mutationRefreshRequired = true;
    snapshotFresh = false;
    renderRows();
    await refresh();
  }
}

async function refresh() {
  if (refreshInFlight || pendingMutation) return;
  refreshInFlight = true;
  refreshButton.disabled = true;
  refreshButton.setAttribute("aria-busy", "true");
  if (!hasSnapshot) setRefreshState("loading", "Loading service data…");
  if (hasSnapshot) renderRows();
  const [servicesResult, healthResult, sessionResult] = await Promise.allSettled([
    fetchJson("/v1/services"),
    fetchJson("/v1/health"),
    fetchDashboardSession(),
  ]);
  updateSupervisorHealth(healthResult);

  if (sessionResult.status === "rejected") {
    dashboardSession = null;
    if (actionStatus.dataset.state === "idle" || actionStatus.dataset.state === "unavailable") {
      setActionStatus("unavailable", "Service controls are unavailable because dashboard access could not be loaded.");
    }
  } else if (actionStatus.dataset.state === "unavailable") {
    setActionStatus("idle", "");
  }

  if (servicesResult.status === "fulfilled" && Array.isArray(servicesResult.value?.services)) {
    snapshot = servicesResult.value.services;
    runningFilterRetained = reconcileRunningRetention(runningFilterRetained, snapshot);
    hasSnapshot = true;
    snapshotFresh = true;
    mutationRefreshRequired = false;
    updateCounts(snapshot);
    const fetchedAt = new Date().toISOString();
    const formatted = formatTimestamp(fetchedAt);
    updatedAt.textContent = formatted;
    if (formatted !== "Not available") updatedAt.dateTime = fetchedAt;
    else updatedAt.removeAttribute("datetime");
    renderRows();
    setRefreshState("success", formatted === "Not available"
      ? "Service data loaded; update time is not available."
      : "Service data refreshed at " + formatted + ".");
  } else {
    snapshotFresh = false;
    const error = servicesResult.status === "rejected"
      ? errorMessage(servicesResult.reason)
      : "Invalid service response format.";
    if (hasSnapshot) {
      setRefreshState("stale", "Refresh failed (" + error + "); showing the last successfully loaded data. Actions are disabled.");
      renderRows();
    } else {
      setRefreshState("error", "Service data is not available: " + error);
      addMessageRow("Service data is not available. Check the connection, then refresh.");
    }
  }

  refreshInFlight = false;
  refreshButton.disabled = false;
  refreshButton.removeAttribute("aria-busy");
  if (pendingMutation) refreshButton.disabled = true;
  renderRows();
}

let snapshot = [];
let hasSnapshot = false;
let snapshotFresh = false;
let refreshInFlight = false;
let mutationRefreshRequired = false;
let pendingMutation = null;
let dashboardSession = null;
let sessionPromise = null;
let refreshTimer = null;
let runningFilterRetained = new Set();
const expandedServices = new Set();
let countTotal;
let countRunning;
let countAttention;
let updatedAt;
let refreshButton;
let refreshStatus;
let actionStatus;
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
  actionStatus = document.getElementById("action-status");
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
      if (refreshTimer === null) refreshTimer = setInterval(() => {
        if (!document.hidden) void refresh();
      }, REFRESH_INTERVAL_MS);
    }
  });

  void refresh();
  refreshTimer = setInterval(() => {
    if (!document.hidden) void refresh();
  }, REFRESH_INTERVAL_MS);
}
