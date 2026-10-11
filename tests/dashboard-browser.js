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
} from "/app.js";

let passed = 0;
let failed = 0;
function check(name, run) {
  const item = document.createElement("li");
  try {
    run();
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
check("valid deduplicated HTTP port links only", () => equal(applicationUrls([11122, 11122, 0, -1, 65536, 1.5, "8080", null, 65535]), [
  { url: "http://127.0.0.1:11122/", port: 11122, inferred: true },
  { url: "http://127.0.0.1:65535/", port: 65535, inferred: true },
]));
check("missing ports do not invent links", () => { equal(applicationUrls(undefined), []); equal(applicationUrls([]), []); });
check("running filter", () => equal(filterServices(services, "", "running").map((service) => service.id), ["sidecar"]));
check("case-insensitive name, PID, and port search", () => {
  for (const query of ["AKUSIDECAR", "42", "11122"]) equal(filterServices(services, query).map((service) => service.id), ["sidecar"]);
});
check("full inferred URL search", () => equal(filterServices(services, "http://127.0.0.1:11122/").map((service) => service.id), ["sidecar"]));
check("attention keeps unknown distinct", () => {
  equal(needsAttention(services[1]), false);
  equal(needsAttention(services[2]), true);
  equal(isRunning(services[0]), true);
});
check("URL protocols and credentials rejected", () => {
  for (const url of ["javascript:alert(1)", "data:text/html,x", "file:///C:/x", "http://user:secret@localhost/"]) equal(safeHttpUrl(url), null);
  equal(safeHttpUrl("http://127.0.0.1:11122/"), "http://127.0.0.1:11122/");
});
check("unavailable timestamps stay unavailable in English", () => {
  for (const date of [undefined, null, "", "invalid"]) equal(formatTimestamp(date), "Not available");
});
check("timestamps use the English date locale", () => {
  const value = "2026-10-11T13:00:00Z";
  const expected = new Intl.DateTimeFormat("en-US", { dateStyle: "medium", timeStyle: "medium" }).format(new Date(value));
  equal(formatTimestamp(value), expected);
});
check("Start is offered for stopped or unowned failed services", () => {
  equal(actionForService({ lifecycle: "stopped", rootPid: null, ownedPids: [] }), "start");
  equal(actionForService({ lifecycle: "failed", rootPid: null, ownedPids: [] }), "start");
  equal(actionsForService({ lifecycle: "stopped", rootPid: null, ownedPids: [] }), ["start"]);
  equal(actionsForService({ lifecycle: "failed", rootPid: null, ownedPids: [] }), ["start"]);
  equal(actionForService({ lifecycle: "stopped", rootPid: null }), null);
  equal(actionForService({ lifecycle: "failed" }), null);
});
check("Stop is offered for owned running, unhealthy, or failed services; Restart only for running", () => {
  equal(actionForService({ lifecycle: "running", rootPid: 42, ownedPids: [] }), "stop");
  equal(actionsForService({ lifecycle: "running", rootPid: 42, ownedPids: [] }), ["stop", "restart"]);
  equal(actionForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [43] }), "stop");
  equal(actionsForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [43] }), ["stop"]);
  equal(actionForService({ lifecycle: "failed", rootPid: 44, ownedPids: [] }), "stop");
  equal(actionsForService({ lifecycle: "failed", rootPid: 44, ownedPids: [] }), ["stop"]);
  equal(actionForService({ lifecycle: "running", rootPid: null, ownedPids: [] }), null);
  equal(actionsForService({ lifecycle: "running", rootPid: null, ownedPids: [] }), []);
});
check("transitional, unknown, and inconsistent states have no action", () => {
  for (const lifecycle of ["starting", "stopping", "termination_pending", "unknown", ""]) {
    equal(actionForService({ lifecycle, rootPid: 42, ownedPids: [42] }), null);
    equal(actionsForService({ lifecycle, rootPid: 42, ownedPids: [42] }), []);
  }
  equal(actionForService({ lifecycle: "stopped", rootPid: 42, ownedPids: [42] }), null);
  equal(actionForService({ lifecycle: "unhealthy", rootPid: null, ownedPids: [] }), null);
});
check("actions fail closed for stale data, missing access, or pending work", () => {
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
check("Restart targets stay in Running while stopped, failed, or transitional and search still applies", () => {
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

document.getElementById("result").textContent = passed + " passed, " + failed + " failed";
document.documentElement.dataset.testResult = failed ? "failed" : "passed";
