import {
  ACTION_LABELS, PENDING_LABELS, LIFECYCLE_LABELS, DESIRED_LABELS, HEALTH_LABELS, HOLD_LABELS,
  normalized, text, enumLabel, isRunning, actionsForService, isActionDisabled,
  applicationUrls, safeHttpUrl, appUrlLabel, filterServices, needsAttention,
} from "./model.js";

// Rendering owns DOM and expanded rows; action orchestration stays in app.js.
export function createDashboardView(document, onAction, onHideRetained) {
  const countTotal = document.getElementById("count-total");
  const countRunning = document.getElementById("count-running");
  const countAttention = document.getElementById("count-attention");
  const updatedAt = document.getElementById("updated-at");
  const refreshButton = document.getElementById("refresh-button");
  const refreshStatus = document.getElementById("refresh-status");
  const actionStatus = document.getElementById("action-status");
  const serviceRows = document.getElementById("service-rows");
  const searchInput = document.getElementById("search-input");
  const filterInput = document.getElementById("service-filter");
  const supervisorHealth = document.getElementById("supervisor-health");
  const resultsSummary = document.getElementById("results-summary");

  const expandedServices = new Set();

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

  function addStateCell(row, service, state) {
    const cell = appendCell(row);
    const stack = makeElement("div", "state-stack");
    const lifecycle = normalized(service.lifecycle);
    const badge = makeElement("span", "badge", enumLabel(service.lifecycle, LIFECYCLE_LABELS));
    badge.dataset.state = lifecycle || "unknown";
    stack.append(badge);
    const retention = state.runningFilterRetained.get(String(service.id));
    if (retention && !isRunning(service)) {
      const isRestartRecovery = retention.reason === "restart";
      stack.append(makeElement("span", isRestartRecovery ? "restart-recovery" : "running-retained",
        isRestartRecovery ? "Restart recovery" : "Kept in Running view"));
      if (filterInput.value === "running") {
        const hide = makeElement("button", "retained-view-control", "Hide from this view");
        hide.type = "button";
        hide.setAttribute("aria-label", "Hide " + text(service.label, text(service.id, "service"))
          + " from the Running view");
        hide.addEventListener("click", () => onHideRetained?.(service));
        stack.append(hide);
      }
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

  function addActionCell(row, service, state) {
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

    for (const action of actions) {
      const actionName = ACTION_LABELS[action];
      const button = makeElement("button", "service-action action-" + action);
      const isPending = state.pendingMutation?.serviceId === service.id && state.pendingMutation.action === action;
      button.type = "button";
      button.setAttribute("aria-describedby", "action-status");
      if (isPending) {
        const pendingVerb = PENDING_LABELS[action];
        button.classList.add("action-pending");
        button.textContent = pendingVerb + "…";
        button.setAttribute("aria-label", pendingVerb + " " + serviceName);
        button.disabled = true;
      } else {
        button.textContent = actionName;
        button.setAttribute("aria-label", actionName + " " + serviceName);
        if (isActionDisabled(service, state, action)) {
          button.disabled = true;
          button.title = !state.sessionAvailable
            ? "Dashboard access is not available."
            : !state.snapshotFresh || state.refreshRequired
              ? "Refresh service status before taking action."
              : state.pendingMutation
                ? "Another service action is in progress."
                : "Service status is being refreshed.";
        } else {
          button.addEventListener("click", () => void onAction(service, action));
        }
      }
      cell.append(button);
    }
  }

  function renderRows(state) {
    const { hasSnapshot, snapshot, runningFilterRetained } = state;
    if (!hasSnapshot) return;
    for (const details of serviceRows.querySelectorAll("details[data-service-id]")) {
      if (details.open) expandedServices.add(details.dataset.serviceId);
      else expandedServices.delete(details.dataset.serviceId);
    }
    const currentIds = new Set(snapshot.map((service) => service.id));
    for (const id of expandedServices) if (!currentIds.has(id)) expandedServices.delete(id);
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
        addStateCell(row, service, state);
        addHealthCell(row, service);
        addUrlsCell(row, service);
        addPidCell(row, service);
        addProcessesCell(row, service);
        addHoldCell(row, service);
        addActionCell(row, service, state);
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


  return { updatedAt, refreshButton, actionStatus, serviceRows, searchInput, filterInput,
    renderRows, addMessageRow, updateCounts, setRefreshState, setActionStatus, updateSupervisorHealth };
}
