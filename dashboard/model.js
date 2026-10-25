// Pure service presentation rules. No DOM, network, or startup side effects.
export const LIFECYCLE_LABELS = {
  stopped: "Stopped",
  starting: "Starting",
  running: "Running",
  stopping: "Stopping",
  termination_pending: "Termination pending",
  unhealthy: "Unhealthy",
  failed: "Failed",
};

export const DESIRED_LABELS = { running: "Running", stopped: "Stopped" };
export const HEALTH_LABELS = { unknown: "Unknown", healthy: "Healthy", unhealthy: "Unhealthy" };
export const HOLD_LABELS = { none: "None", running: "Hold while running", stopped: "Hold while stopped" };

export function normalized(value) {
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

export function text(value, fallback = "Not available") {
  if (value === null || value === undefined || value === "") return fallback;
  if (["string", "number", "boolean"].includes(typeof value)) return String(value);
  return fallback;
}

export function enumLabel(value, labels, fallback = "Unknown") {
  const key = normalized(value);
  return labels[key] ?? (key ? "Value: " + text(value) : fallback);
}

export const ACTION_LABELS = { start: "Start", stop: "Stop", restart: "Restart" };
export const PENDING_LABELS = { start: "Starting", stop: "Stopping", restart: "Restarting" };
