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
  const retained = new Set([stopped.id, failed.id, transitional.id]);
  equal(filterServices(current, "", "running", retained).map((service) => service.id), [
    "restart-stopped", "restart-failed", "restart-pending", "healthy",
  ]);
  equal(filterServices(current, "recover failed", "running", retained).map((service) => service.id), ["restart-failed"]);
  equal([...reconcileRunningRetention(retained, current)].sort(), ["restart-failed", "restart-pending", "restart-stopped"]);
  equal([...reconcileRunningRetention(retained, [failed, transitional, running])].sort(), ["restart-failed", "restart-pending"]);
  equal(reconcileRunningRetention(new Set([stopped.id]), [{ ...stopped, lifecycle: "running" }]).size, 0);
  equal(reconcileRunningRetention(new Set([stopped.id]), []).size, 0);
});

// These tests use detached DOM and injected network adapters, never live services.
const markup = await (await fetch("/")).text();
const viewDocument = new DOMParser().parseFromString(markup, "text/html");
const actions = [];
const view = createDashboardView(viewDocument, (service, action) => actions.push([service.id, action]));
const service = {
  id: "view-fixture", label: "<img src=x onerror=alert(1)>", lifecycle: "running",
  desiredState: "running", rootPid: 42, ownedPids: [42], ownedProcesses: [{ pid: 42, name: "worker" }],
  ports: [8080], health: { status: "healthy" }, operatorHold: "none",
};
const ready = {
  hasSnapshot: true, snapshot: [service], snapshotFresh: true, sessionAvailable: true,
  runningFilterRetained: new Set(),
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
    runningFilterRetained: new Set([service.id]) });
  equal(view.serviceRows.querySelector(".restart-recovery").textContent, "Restart recovery");
  equal(view.serviceRows.querySelector(".action-start").disabled, false);
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
