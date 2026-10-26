import {
  actionForService,
  actionsForService,
  applicationUrls,
  filterServices,
  formatTimestamp,
  isActionDisabled,
  isRunning,
  needsAttention,
  reconcileRunningRetention,
  safeHttpUrl,
} from "/model.js";
import { createDashboardView } from "/view.js";
import { createDashboardApi } from "/api.js";
import { mountDashboard } from "/app.js";

let passed = 0;
let failed = 0;
async function check(name, run) {
  const item = document.createElement("li");
  try {
    await run();
    item.textContent = "PASS " + name;
    passed += 1;
  } catch (error) {
    item.textContent = "FAIL " + name + ": " + error.message;
    failed += 1;
  }
  document.getElementById("checks").append(item);
}
function equal(actual, expected) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(JSON.stringify(actual) + " != " + JSON.stringify(expected));
  }
}

const services = [
  { id: "sidecar", label: "AkuSidecar", lifecycle: "running", rootPid: 42, ports: [11122], health: { status: "healthy" } },
  { id: "offline", lifecycle: "stopped", rootPid: null, ports: [], health: { status: "unknown" } },
  { id: "failure", lifecycle: "failed", ports: [8080], health: { status: "unhealthy" } },
];
await check("valid deduplicated HTTP port links only", () => equal(applicationUrls([11122, 11122, 0, -1, 65536, 1.5, "8080", null, 65535]), [
  { url: "http://127.0.0.1:11122/", port: 11122, inferred: true },
  { url: "http://127.0.0.1:65535/", port: 65535, inferred: true },
]));
await check("missing ports do not invent links", () => { equal(applicationUrls(undefined), []); equal(applicationUrls([]), []); });
await check("running filter", () => equal(filterServices(services, "", "running").map((service) => service.id), ["sidecar"]));
await check("case-insensitive name, PID, and port search", () => {
  for (const query of ["AKUSIDECAR", "42", "11122"]) equal(filterServices(services, query).map((service) => service.id), ["sidecar"]);
});
await check("full inferred URL search", () => equal(filterServices(services, "http://127.0.0.1:11122/").map((service) => service.id), ["sidecar"]));
await check("attention keeps unknown distinct", () => {
  equal(needsAttention(services[1]), false);
  equal(needsAttention(services[2]), true);
  equal(isRunning(services[0]), true);
});
await check("URL protocols and credentials rejected", () => {
  for (const url of ["javascript:alert(1)", "data:text/html,x", "file:///C:/x", "http://user:secret@localhost/"]) equal(safeHttpUrl(url), null);
  equal(safeHttpUrl("http://127.0.0.1:11122/"), "http://127.0.0.1:11122/");
});
await check("unavailable timestamps stay unavailable in English", () => {
  for (const date of [undefined, null, "", "invalid"]) equal(formatTimestamp(date), "Not available");
});
await check("timestamps use the English date locale", () => {
  const value = "2026-10-11T13:00:00Z";
  const expected = new Intl.DateTimeFormat("en-US", { dateStyle: "medium", timeStyle: "medium" }).format(new Date(value));
  equal(formatTimestamp(value), expected);
});
await check("Start is offered for stopped or unowned failed services", () => {
  equal(actionForService({ lifecycle: "stopped", rootPid: null, ownedPids: [] }), "start");
  equal(actionForService({ lifecycle: "failed", rootPid: null, ownedPids: [] }), "start");
  equal(actionsForService({ lifecycle: "stopped", rootPid: null, ownedPids: [] }), ["start"]);
  equal(actionsForService({ lifecycle: "failed", rootPid: null, ownedPids: [] }), ["start"]);
  equal(actionForService({ lifecycle: "stopped", rootPid: null }), null);
  equal(actionForService({ lifecycle: "failed" }), null);
});
await check("Stop is offered for owned running, unhealthy, or failed services; Restart only for running", () => {
  equal(actionForService({ lifecycle: "running", rootPid: 42, ownedPids: [] }), "stop");
  equal(actionsForService({ lifecycle: "running", rootPid: 42, ownedPids: [] }), ["stop", "restart"]);
  equal(actionForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [43] }), "stop");
  equal(actionsForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [43] }), ["stop"]);
  equal(actionForService({ lifecycle: "failed", rootPid: 44, ownedPids: [] }), "stop");
  equal(actionsForService({ lifecycle: "failed", rootPid: 44, ownedPids: [] }), ["stop"]);
  equal(actionForService({ lifecycle: "running", rootPid: null, ownedPids: [] }), null);
  equal(actionsForService({ lifecycle: "running", rootPid: null, ownedPids: [] }), []);
});
await check("transitional, unknown, and inconsistent states have no action", () => {
  for (const lifecycle of ["starting", "stopping", "termination_pending", "unknown", ""]) {
    equal(actionForService({ lifecycle, rootPid: 42, ownedPids: [42] }), null);
    equal(actionsForService({ lifecycle, rootPid: 42, ownedPids: [42] }), []);
  }
  equal(actionForService({ lifecycle: "stopped", rootPid: 42, ownedPids: [42] }), null);
  equal(actionForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [] }), null);
});
await check("actions fail closed for stale data, missing access, or pending work", () => {
  const service = { lifecycle: "running", rootPid: 42, ownedPids: [42] };
  const ready = { snapshotFresh: true, sessionAvailable: true };
  for (const action of actionsForService(service)) {
    equal(isActionDisabled(service, ready, action), false);
    for (const state of [
      { ...ready, snapshotFresh: false },
      { ...ready, sessionAvailable: false },
      { ...ready, mutationPending: true },
      { ...ready, refreshRequired: true },
      { ...ready, refreshInFlight: true },
    ]) equal(isActionDisabled(service, state, action), true);
  }
  const startService = { lifecycle: "stopped", rootPid: null, ownedPids: [] };
  equal(isActionDisabled(startService, ready, "start"), false);
  equal(isActionDisabled(startService, { ...ready, refreshRequired: true }, "start"), true);
});
await check("Restart targets stay in Running while stopped, failed, or transitional and search still applies", () => {
  const stopped = { id: "restart-stopped", label: "Recover stopped", lifecycle: "stopped", rootPid: null, ownedPids: [] };
  const failed = { id: "restart-failed", label: "Recover failed", lifecycle: "failed", rootPid: null, ownedPids: [] };
  const transitional = { id: "restart-pending", label: "Recover pending", lifecycle: "stopping", rootPid: 43, ownedPids: [43] };
  const unrelated = { id: "offline", label: "Unrelated stopped", lifecycle: "stopped", rootPid: null, ownedPids: [] };
  const running = { id: "healthy", label: "Healthy", lifecycle: "running", rootPid: 44, ownedPids: [44] };
  const current = [stopped, failed, transitional, unrelated, running];
  const retained = new Map([stopped.id, failed.id, transitional.id]
    .map((id) => [id, { reason: "restart", observedNonrunning: false }]));
  equal(filterServices(current, "", "running", retained).map((service) => service.id), [
    "restart-stopped", "restart-failed", "restart-pending", "healthy",
  ]);
  equal(filterServices(current, "recover failed", "running", retained).map((service) => service.id), ["restart-failed"]);
  equal([...reconcileRunningRetention(retained, current).keys()].sort(), ["restart-failed", "restart-pending", "restart-stopped"]);
  equal([...reconcileRunningRetention(retained, [failed, transitional, running]).keys()].sort(), ["restart-failed", "restart-pending"]);
  equal(reconcileRunningRetention(new Map([[stopped.id, { reason: "restart" }]]), [{ ...stopped, lifecycle: "running" }]).size, 0);
  equal(reconcileRunningRetention(new Map([[stopped.id, { reason: "restart" }]]), []).size, 0);
});
await check("Stop retention survives the first running snapshot and clears after a nonrunning-to-running transition", () => {
  const running = { id: "stop-target", lifecycle: "running", rootPid: 42, ownedPids: [42] };
  const stopped = { id: "stop-target", lifecycle: "stopped", rootPid: null, ownedPids: [] };
  const retained = new Map([[running.id, { reason: "stop", observedNonrunning: false }]]);
  const afterDelayedStopSnapshot = reconcileRunningRetention(retained, [running]);
  equal(afterDelayedStopSnapshot.has(running.id), true);
  equal(afterDelayedStopSnapshot.get(running.id).observedNonrunning, false);
  const afterStoppedSnapshot = reconcileRunningRetention(afterDelayedStopSnapshot, [stopped]);
  equal(afterStoppedSnapshot.get(running.id).observedNonrunning, true);
  equal(reconcileRunningRetention(afterStoppedSnapshot, [running]).size, 0);
  equal(reconcileRunningRetention(retained, []).size, 0);
});

// These tests use detached DOM and injected network adapters, never live services.
const markup = await (await fetch("/")).text();
const viewDocument = new DOMParser().parseFromString(markup, "text/html");
const actions = [];
const hidden = [];
const view = createDashboardView(viewDocument, (service, action) => actions.push([service.id, action]),
  (service) => hidden.push(service.id));
const service = {
  id: "view-fixture", label: "<img src=x onerror=alert(1)>", lifecycle: "running",
  desiredState: "running", rootPid: 42, ownedPids: [42], ownedProcesses: [{ pid: 42, name: "worker" }],
  ports: [8080], health: { status: "healthy" }, operatorHold: "none",
};
const ready = {
  hasSnapshot: true, snapshot: [service], snapshotFresh: true, sessionAvailable: true,
  runningFilterRetained: new Map(),
};
await check("view renders untrusted labels as text and safe application links", () => {
  view.renderRows(ready);
  equal(view.serviceRows.querySelector(".service-name").textContent, service.label);
  equal(view.serviceRows.querySelector("img"), null);
  const link = view.serviceRows.querySelector("a");
  equal(link.href, "http://127.0.0.1:8080/");
  equal(link.rel, "noopener noreferrer");
});
await check("view routes Restart to its controller and blocks pending or stale actions", () => {
  view.serviceRows.querySelector(".action-restart").click();
  equal(actions, [[service.id, "restart"]]);
  view.renderRows({ ...ready, pendingMutation: { serviceId: service.id, action: "restart" }, mutationPending: true });
  equal(view.serviceRows.querySelector(".action-restart").textContent, "Restarting…");
  equal([...view.serviceRows.querySelectorAll("button")].every((button) => button.disabled), true);
  view.renderRows({ ...ready, snapshotFresh: false });
  equal([...view.serviceRows.querySelectorAll("button")].every((button) => button.disabled), true);
});
await check("view retains Restart recovery in Running and offers Start after a failed restart", () => {
  view.filterInput.value = "running";
  view.renderRows({ ...ready, snapshot: [{ ...service, lifecycle: "stopped", rootPid: null, ownedPids: [] }],
    runningFilterRetained: new Map([[service.id, { reason: "restart" }]]) });
  equal(view.serviceRows.querySelector(".restart-recovery").textContent, "Restart recovery");
  equal(view.serviceRows.querySelector(".action-start").disabled, false);
  view.filterInput.value = "all";
});
await check("Stop retention is labeled in Running and offers a UI-only hide control", () => {
  const stopped = { ...service, lifecycle: "stopped", rootPid: null, ownedPids: [] };
  view.filterInput.value = "running";
  view.renderRows({ ...ready, snapshot: [stopped],
    runningFilterRetained: new Map([[service.id, { reason: "stop", observedNonrunning: true }]]) });
  equal(view.serviceRows.querySelector(".running-retained").textContent, "Kept in Running view");
  view.serviceRows.querySelector(".retained-view-control").click();
  equal(hidden, [service.id]);
  view.filterInput.value = "all";
});
await check("expanded process rows survive refresh but are released when their service is removed", () => {
  view.renderRows(ready);
  view.serviceRows.querySelector("details").open = true;
  view.renderRows(ready);
  equal(view.serviceRows.querySelector("details").open, true);
  view.renderRows({ ...ready, snapshot: [] });
  view.renderRows(ready);
  equal(view.serviceRows.querySelector("details").open, false);
});

function copyServices(items) {
  return items.map((item) => ({
    ...item,
    ownedPids: Array.isArray(item.ownedPids) ? [...item.ownedPids] : item.ownedPids,
    ownedProcesses: Array.isArray(item.ownedProcesses) ? item.ownedProcesses.map((process) => ({ ...process })) : item.ownedProcesses,
    health: item.health ? { ...item.health } : item.health,
    ports: Array.isArray(item.ports) ? [...item.ports] : item.ports,
  }));
}
function createControllerFixture(initialServices, handleAction, polling = false) {
  const controllerDocument = new DOMParser().parseFromString(markup, "text/html");
  Object.defineProperty(controllerDocument, "hidden", { value: false, configurable: true });
  let currentServices = copyServices(initialServices);
  let serviceReadError = null;
  const actionCalls = [];
  const pollCallbacks = [];
  const api = {
    async fetchJson(path) {
      if (path === "/v1/services") {
        if (serviceReadError) throw serviceReadError;
        return { services: copyServices(currentServices) };
      }
      if (path === "/v1/health") return { status: "healthy" };
      if (path === "/dashboard/v1/session") return { token: "a".repeat(64), instanceId: "controller-fixture" };
      throw new Error("Unexpected dashboard request: " + path);
    },
    async sendServiceAction(serviceId, action, token, requestId) {
      actionCalls.push({ serviceId, action, token, requestId });
      return handleAction({
        serviceId,
        action,
        setServices(items) { currentServices = copyServices(items); },
        get services() { return copyServices(currentServices); },
      });
    },
  };
  const controller = mountDashboard(controllerDocument, {
    api,
    polling,
    setInterval(callback) { pollCallbacks.push(callback); return callback; },
    clearInterval() {},
  });
  return {
    document: controllerDocument,
    controller,
    actionCalls,
    poll() { pollCallbacks[0]?.(); },
    setServices(items) { currentServices = copyServices(items); },
    setServiceReadError(error) { serviceReadError = error; },
    get services() { return copyServices(currentServices); },
  };
}
function actionResult(serviceId, outcome = "applied") {
  return {
    response: new Response("{}", { status: 200 }),
    payload: { serviceId, outcome },
  };
}
function renderedIds(controllerDocument) {
  return [...controllerDocument.querySelectorAll("#service-rows .service-id")].map((node) => node.textContent);
}
function setFilter(fixture, mode) {
  const input = fixture.document.getElementById("service-filter");
  input.value = mode;
  input.dispatchEvent(new Event("change", { bubbles: true }));
}
function setSearch(fixture, value) {
  const input = fixture.document.getElementById("search-input");
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}
async function settleController() {
  await new Promise((resolve) => setTimeout(resolve, 0));
}
function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const targetRunning = {
  id: "target", label: "Target service", lifecycle: "running", desiredState: "running",
  rootPid: 42, ownedPids: [42], ownedProcesses: [{ pid: 42, name: "target" }],
  health: { status: "healthy" }, operatorHold: "none", ports: [],
};
const targetStopped = {
  ...targetRunning, lifecycle: "stopped", desiredState: "stopped", rootPid: null,
  ownedPids: [], ownedProcesses: [], health: { status: "unknown" },
};
const unrelatedStopped = {
  id: "unrelated", label: "Unrelated stopped", lifecycle: "stopped", desiredState: "stopped",
  rootPid: null, ownedPids: [], ownedProcesses: [], health: { status: "unknown" }, operatorHold: "none", ports: [],
};

await check("controller retains a stopped Running row through refresh and filter changes, then safely starts it", async () => {
  const fixture = createControllerFixture([targetRunning, unrelatedStopped], async ({ serviceId, action, setServices, services }) => {
    if (serviceId === targetRunning.id && action === "stop") setServices([targetStopped, services.find((item) => item.id === unrelatedStopped.id)]);
    if (serviceId === targetRunning.id && action === "start") setServices([targetRunning, services.find((item) => item.id === unrelatedStopped.id)]);
    return actionResult(serviceId);
  }, true);
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    equal(renderedIds(fixture.document), ["target"]);
    equal(fixture.document.getElementById("count-running").textContent, "1");
    setSearch(fixture, "Target");
    equal(renderedIds(fixture.document), ["target"]);
    setSearch(fixture, "Unrelated");
    equal(renderedIds(fixture.document), []);
    equal(fixture.document.getElementById("count-running").textContent, "1");
    setSearch(fixture, "");
    fixture.document.querySelector(".action-stop").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    equal(renderedIds(fixture.document), ["target"]);
    equal(fixture.document.querySelector(".running-retained").textContent, "Kept in Running view");
    equal(fixture.document.querySelector(".action-start").disabled, false);
    equal(fixture.document.getElementById("count-running").textContent, "0");
    fixture.poll();
    await settleController();
    equal(fixture.document.querySelector(".running-retained").textContent, "Kept in Running view");
    setSearch(fixture, "Unrelated");
    equal(renderedIds(fixture.document), []);
    equal(fixture.document.getElementById("count-running").textContent, "0");
    setSearch(fixture, "Target");
    equal(renderedIds(fixture.document), ["target"]);
    setSearch(fixture, "");
    setFilter(fixture, "all");
    equal(renderedIds(fixture.document), ["target", "unrelated"]);
    setFilter(fixture, "running");
    equal(renderedIds(fixture.document), ["target"]);
    fixture.document.querySelector(".action-start").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["stop", "start"]);
    equal(fixture.document.querySelector('[data-state="running"]').textContent, "Running");
    equal(fixture.document.querySelector(".running-retained"), null);
    equal(fixture.document.getElementById("count-running").textContent, "1");
  } finally {
    fixture.controller.destroy();
  }
});

await check("controller keeps Stop retention through timeout and delayed Stop snapshots without retry", async () => {
  const fixture = createControllerFixture([targetRunning], async () => {
    throw new DOMException("delayed response", "AbortError");
  });
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    fixture.document.querySelector(".action-stop").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    equal(fixture.document.getElementById("action-status").dataset.state, "error");
    equal(renderedIds(fixture.document), ["target"]);
    fixture.setServices([targetStopped]);
    await fixture.controller.refresh();
    equal(renderedIds(fixture.document), ["target"]);
    equal(fixture.document.querySelector(".running-retained").textContent, "Kept in Running view");
    fixture.setServices([targetRunning]);
    await fixture.controller.refresh();
    equal(fixture.document.querySelector(".running-retained"), null);
    fixture.setServices([targetStopped]);
    await fixture.controller.refresh();
    equal(renderedIds(fixture.document), []);
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
  } finally {
    fixture.controller.destroy();
  }
});

await check("controller keeps Stop retention after an HTTP failure and still-running refresh", async () => {
  const fixture = createControllerFixture([targetRunning], async () => ({
    response: new Response("{}", { status: 503 }),
    payload: { error: { code: "temporarily_unavailable", message: "Try later" } },
  }));
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    fixture.document.querySelector(".action-stop").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    equal(renderedIds(fixture.document), ["target"]);
    equal(fixture.document.getElementById("action-status").dataset.state, "error");
    fixture.setServices([targetStopped]);
    await fixture.controller.refresh();
    equal(renderedIds(fixture.document), ["target"]);
    equal(fixture.document.querySelector(".running-retained").textContent, "Kept in Running view");
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
  } finally {
    fixture.controller.destroy();
  }
});

await check("controller prevents a second mutation while pending and after a stale refresh", async () => {
  const request = deferred();
  const fixture = createControllerFixture([targetRunning], () => request.promise);
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    fixture.document.querySelector(".action-stop").click();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    equal([...fixture.document.querySelectorAll(".action-cell button")].every((button) => button.disabled), true);
    fixture.document.querySelector(".action-stop").click();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    request.resolve(actionResult(targetRunning.id));
    await settleController();
    fixture.setServiceReadError(new Error("offline"));
    await fixture.controller.refresh();
    equal([...fixture.document.querySelectorAll(".action-cell button")].every((button) => button.disabled), true);
    fixture.document.querySelector(".action-stop").click();
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
  } finally {
    fixture.controller.destroy();
  }
});

await check("controller preserves Restart recovery through a failed restart and Start", async () => {
  const fixture = createControllerFixture([targetRunning], async ({ serviceId, action, setServices }) => {
    if (action === "restart") {
      setServices([targetStopped]);
      return { response: new Response("{}", { status: 503 }), payload: { error: { code: "restart_failed" } } };
    }
    if (action === "start") setServices([targetRunning]);
    return actionResult(serviceId);
  });
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    fixture.document.querySelector(".action-restart").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["restart"]);
    equal(fixture.document.querySelector(".restart-recovery").textContent, "Restart recovery");
    equal(fixture.document.querySelector(".action-start").disabled, false);
    fixture.document.querySelector(".action-start").click();
    await settleController();
    equal(fixture.actionCalls.map((call) => call.action), ["restart", "start"]);
    equal(fixture.document.querySelector(".restart-recovery"), null);
    equal(renderedIds(fixture.document), ["target"]);
  } finally {
    fixture.controller.destroy();
  }
});

await check("controller hides retained rows locally, clears removed IDs, and resets retention on remount", async () => {
  const fixture = createControllerFixture([targetRunning], async ({ serviceId, action, setServices }) => {
    if (action === "stop") setServices([targetStopped]);
    return actionResult(serviceId);
  });
  try {
    await fixture.controller.ready;
    setFilter(fixture, "running");
    fixture.document.querySelector(".action-stop").click();
    await settleController();
    equal(renderedIds(fixture.document), ["target"]);
    fixture.document.querySelector(".retained-view-control").click();
    equal(renderedIds(fixture.document), []);
    equal(fixture.actionCalls.map((call) => call.action), ["stop"]);
    await fixture.controller.refresh();
    equal(renderedIds(fixture.document), []);
  } finally {
    fixture.controller.destroy();
  }
  const removed = createControllerFixture([targetRunning], async ({ serviceId, action, setServices }) => {
    if (action === "stop") setServices([targetStopped]);
    return actionResult(serviceId);
  });
  try {
    await removed.controller.ready;
    setFilter(removed, "running");
    removed.document.querySelector(".action-stop").click();
    await settleController();
    equal(renderedIds(removed.document), ["target"]);
    removed.setServices([]);
    await removed.controller.refresh();
    removed.setServices([targetStopped]);
    await removed.controller.refresh();
    equal(renderedIds(removed.document), []);
  } finally {
    removed.controller.destroy();
  }
  const remounted = createControllerFixture([targetStopped], async ({ serviceId }) => actionResult(serviceId));
  try {
    await remounted.controller.ready;
    setFilter(remounted, "running");
    equal(renderedIds(remounted.document), []);
  } finally {
    remounted.controller.destroy();
  }
});

await check("API preserves scoped mutation envelope and does not retry an expired session", async () => {
  const calls = [];
  const api = createDashboardApi(async (path, options) => {
    calls.push([path, options]);
    return new Response('{"error":{"code":"expired_session"}}', { status: 401 });
  });
  const result = await api.sendServiceAction("fixture/id", "restart", "fixture-token", "request-1");
  equal(result.response.status, 401);
  equal(calls.length, 1);
  equal(calls[0][0], "/dashboard/v1/services/fixture%2Fid/restart");
  equal(calls[0][1].method, "POST");
  equal(calls[0][1].headers["X-Dashboard-Token"], "fixture-token");
  equal(JSON.parse(calls[0][1].body), { requestId: "request-1" });
  equal(calls[0][1].cache, "no-store");
});
await check("API rejects failed snapshot reads and preserves unreadable mutation outcomes", async () => {
  const api = createDashboardApi(async () => new Response("not JSON", { status: 503 }));
  let message = "";
  try { await api.fetchJson("/v1/services"); } catch (error) { message = error.message; }
  equal(message, "HTTP 503");
  const result = await api.sendServiceAction("fixture", "stop", "token", "request-2");
  equal(result.response.status, 503);
  equal(result.payload, null);
});
await check("API propagates network errors without replaying a mutation", async () => {
  let calls = 0;
  const api = createDashboardApi(async () => { calls += 1; throw new Error("disconnected"); });
  let message = "";
  try { await api.sendServiceAction("fixture", "start", "token", "request-3"); }
  catch (error) { message = error.message; }
  equal(message, "disconnected");
  equal(calls, 1);
});

document.getElementById("result").textContent = passed + " passed, " + failed + " failed";
document.documentElement.dataset.testResult = failed ? "failed" : "passed";
