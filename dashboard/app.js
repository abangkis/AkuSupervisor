import { ACTION_LABELS, text, isActionDisabled, reconcileRunningRetention, formatTimestamp } from "./model.js";
import { createDashboardApi } from "./api.js";
import { createDashboardView } from "./view.js";

const REFRESH_INTERVAL_MS = 5000;

// One page controller owns refresh, session renewal, and mutation ordering.
function mountDashboard(document) {
  const { fetchJson, sendServiceAction } = createDashboardApi();
  const view = createDashboardView(document, (service, action) => mutateService(service, action));
  const { updatedAt, refreshButton, actionStatus, searchInput, filterInput,
    addMessageRow, updateCounts, setRefreshState, setActionStatus, updateSupervisorHealth } = view;
  function renderRows() {
    view.renderRows({ ...actionState(), snapshot, hasSnapshot, pendingMutation, runningFilterRetained });
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

  function actionState() {
    return {
      snapshotFresh,
      sessionAvailable: Boolean(dashboardSession),
      mutationPending: Boolean(pendingMutation),
      refreshRequired: mutationRefreshRequired,
      refreshInFlight,
    };
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
    const actionLabel = ACTION_LABELS[action];
    const requestId = createRequestId();
    if (action === "restart") runningFilterRetained.add(serviceId);
    pendingMutation = { serviceId, action };
    mutationRefreshRequired = true;
    snapshotFresh = false;
    refreshButton.disabled = true;
    setActionStatus("progress", "Sending " + actionLabel.toLowerCase() + " request for " + serviceName + "…");
    renderRows();

    try {
      const session = dashboardSession;
      const { response, payload } = await sendServiceAction(serviceId, action, session.token, requestId);
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
    renderRows();
  }

  refreshButton.addEventListener("click", () => void refresh());
  searchInput.addEventListener("input", renderRows);
  filterInput.addEventListener("change", renderRows);

  function updatePolling() {
    if (refreshTimer !== null) clearInterval(refreshTimer);
    refreshTimer = null;
    if (!document.hidden) {
      void refresh();
      refreshTimer = setInterval(() => void refresh(), REFRESH_INTERVAL_MS);
    }
  }
  document.addEventListener("visibilitychange", updatePolling);
  updatePolling();
}

if (document.getElementById("service-rows")) mountDashboard(document);
