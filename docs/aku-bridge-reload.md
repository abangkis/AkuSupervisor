# AkuBridge Cooperative Reload

Status: **Completed - Gate 5 passed on 2026-07-14**

Reliability follow-up: **Gate 5.1 passed on 2026-07-14**

## Contract

AkuSupervisor exposes exactly one browser cooperative action:

```text
reload_self(target = aku-bridge)
```

The authenticated control route is
`POST /v1/cooperative-actions/aku-bridge/reload-self`. The CLI is
`aku-supervisor bridge reload`. Both require a bounded reason, actor, and
request ID. The request ID provides replay protection at the Supervisor and
Sidecar boundaries.

The POST starts a single-flight background operation and returns `202` while it
is running. Progress remains queryable at
`GET /v1/cooperative-actions/aku-bridge/requests/{requestId}` or through
`aku-supervisor bridge status --request-id <id>`. The CLI waits for terminal
status by default and supports `--no-wait`. A second request ID is rejected
with `action_in_progress`; replaying the same request ID returns its current or
terminal snapshot without a second reload.

The action is separate from service lifecycle control. It cannot supply a
command, executable, URL, tab ID, Chrome profile, or arbitrary extension
message. Its audit records are written to
`.runtime/cooperative-actions.jsonl`, separately from `supervisor.jsonl`.

## Relay sequence

1. AkuSupervisor authenticates the caller with its current-user control token
   and writes a `requested` audit record before any external effect.
2. The platform-neutral Sidecar relay adapter reads the existing local bridge
   identity and requests one `reload_self` action from AkuSidecar.
3. Only an AkuBrowser page that has completed a compatible AkuBridge
   capability handshake starts the bounded long poll, claims the in-memory
   action, and posts the narrow message through its existing same-origin tab
   bridge. The same local URL opened in a browser without AkuBridge remains
   passive. A disconnected HTTP waiter is cancelled immediately and cannot
   consume a later action.
4. The service worker validates the sender origin and acknowledges the action
   to Sidecar with the bridge token and contract headers.
5. The service worker stores only the originating tab ID plus a short expiry in
   extension-local storage, acknowledges acceptance, and calls
   `chrome.runtime.reload()`.
6. The new service worker consumes and removes that marker, then calls
   `chrome.tabs.reload()` for the originating AkuBrowser tab. The new content
   script therefore belongs to the new runtime and can publish its heartbeat.
   No page timer is used; other tabs, Chrome, the profile, and login sessions
   remain running.
7. The newly injected bridge publishes its capability heartbeat. Sidecar marks
   the action complete only if `buildId` equals the build required by the
   current Sidecar.
8. AkuSupervisor writes the terminal audit record and returns the observed
   build identity.

An extension that is disabled, unreachable, missing the handler, or does not
publish the expected heartbeat fails closed after a bounded deadline. No
Computer Use fallback runs automatically.

The retained failure categories identify the last boundary that was not
proven: `relay_page_stale`, `relay_not_delivered`,
`extension_not_accepted`, `reload_heartbeat_timeout`, and `build_mismatch`.
Audit transitions include `requested`, `relay_created`, `delivered`,
`accepted`, `heartbeat_observed`, and the terminal state, with the relay action
ID retained on failures whenever Sidecar created one. If a reload completes
between two Supervisor polls, Sidecar timestamps allow the missing delivery
and acceptance milestones to be written deterministically.

The AkuBrowser client retries bootstrap after a transient Sidecar restart,
observes the new Sidecar `instanceEpoch` on ordinary API responses, and uses a
versioned module URL during development. It discards Bridge-ready state from
the replaced process and performs a fresh bounded handshake. Long polls are
tied to the HTTP connection lifetime so a reloaded page cannot leave a waiter
that steals the next action. The epoch is application readiness evidence;
AkuSupervisor does not interpret it or add a service dependency.

Codex identity is preserved in operation and audit data as
`{"actorType":"agent","actorId":"codex"}` rather than being flattened to a
generic agent. Legacy string actor values remain readable when old journals are
reopened.

## One-time bootstrap

An already loaded extension build cannot execute code that exists only in a
new unpacked-extension source tree. Therefore, the first build containing this
handler must be loaded once with Chrome's normal extension reload control.
After that bootstrap, future source builds can load through `reload_self`, as
long as each build follows the existing version/runtime-revision identity
contract.

## Portability boundary

The application exposes `CooperativeActionControl`; the lifecycle domain does
not know about Chrome or HTTP. The Sidecar transport is an adapter using a
configured pathless loopback origin. Windows CNG and token-file ACLs protect
the outer Supervisor authentication boundary, but no Windows security type
appears in the cooperative action interface. Linux and macOS implementations
can retain the same application contract while supplying their native secure
token storage and permissions.

## Live validation gate

Gate 5 passes only after a real Chrome validation proves:

- the command returns `completed` with the expected build ID;
- Sidecar records a post-acceptance heartbeat from the new build;
- Chrome, source tabs, profile, and login state survive;
- `cooperative-actions.jsonl` contains requested and completed records; and
- disabled/unreachable behavior times out without broader browser control.

The repeatable machine gate is:

```powershell
.\target\dev\aku-supervisor.exe bridge validate `
  --actor codex `
  --request-id bridge-release-20260714-001
```

It always emits JSON. Exit `0` requires the terminal cooperative reload, the
six ordered audit stages, matching structured actor and request ID, equal
non-null expected/observed heartbeat build IDs, and a null active-operation
snapshot. Exit `1` is a validation or execution failure. The stable promotion
script refuses to copy the development executable unless both exit code and
JSON status pass.

Live evidence on 2026-07-14:

- the first handler-capable unpacked build was bootstrapped once manually;
- AkuBridge announced `aku-bridge-0.5.18-source-fidelity-v20` with
  `reload_self` in its capabilities;
- `aku-supervisor bridge reload` completed in one request and observed a
  heartbeat timestamp newer than the pre-action heartbeat;
- the cooperative journal stored consecutive `requested` and `completed`
  records with the same request ID and relay action ID;
- the AkuBrowser, X, LinkedIn, and extensions tabs retained the same Chrome tab
  IDs; and
- AkuSidecar remained healthy, running, and owned by AkuSupervisor after the
  reload.

Reliability evidence on the same date:

- request `gate51-live-20260714-013` completed autonomously in under one second;
- expected and observed build IDs both equalled
  `aku-bridge-0.5.18-source-fidelity-v20`;
- the heartbeat age was 82 ms when the command returned; and
- audit records preserved the structured Codex actor and one relay action ID.
- after the AkuBrowser tab remained untouched in the background for more than
  five minutes, request `gate51-live-20260714-014` completed in 1.7 seconds;
- its journal recorded all six stages in order with relay action ID
  `78830120-c3ac-4a53-bf87-506d612016a4`; and
- post-command health remained `healthy` with a 1,025 ms heartbeat age.

## Current operational checkpoint

The historical Gate 5 evidence above records the build that introduced the
reload protocol. The current AkuWorkspace compatibility target is AkuBridge
`0.5.33`, runtime `source-fidelity-v35`, with `x-dom-v13` and
`linkedin-dom-v10`. AkuSidecar also requires the `report_capture_quality`
capability. Component package versions are otherwise independent.

On 2026-07-14, `bridge validate` again completed the ordered
`requested -> relay_created -> delivered -> accepted -> heartbeat_observed -> completed`
audit and observed `aku-bridge-0.5.29-source-fidelity-v31` with no zombie
operation. Subsequent LinkedIn capture from a long-backgrounded source tab
completed with bounded scroll restoration. Chrome's page-timer throttling is
handled inside AkuBridge and does not change AkuSupervisor's cooperative reload
authority.

The later request `quality-architecture-20260714-2121` completed in one second
and observed exactly `aku-bridge-0.5.33-source-fidelity-v35`. The subsequent
signed-in unified X + LinkedIn run completed both sources with the new generic
quality-report and Sidecar-admission contract. This confirms cooperative reload
can deploy a capability/version boundary change without browser control.
