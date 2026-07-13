# AkuBridge Cooperative Reload

Status: **Completed - Gate 5 passed on 2026-07-14**

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

The action is separate from service lifecycle control. It cannot supply a
command, executable, URL, tab ID, Chrome profile, or arbitrary extension
message. Its audit records are written to
`.runtime/cooperative-actions.jsonl`, separately from `supervisor.jsonl`.

## Relay sequence

1. AkuSupervisor authenticates the caller with its current-user control token
   and writes a `requested` audit record before any external effect.
2. The platform-neutral Sidecar relay adapter reads the existing local bridge
   identity and requests one `reload_self` action from AkuSidecar.
3. The AkuBrowser page claims the in-memory action and posts the narrow message
   through its existing same-origin tab bridge.
4. The service worker validates the sender origin and acknowledges the action
   to Sidecar with the bridge token and contract headers.
5. The service worker calls `chrome.runtime.reload()`.
6. The local AkuBrowser page refreshes once. Other tabs, Chrome, the profile,
   and login sessions remain running.
7. The newly injected bridge publishes its capability heartbeat. Sidecar marks
   the action complete only if `buildId` equals the build required by the
   current Sidecar.
8. AkuSupervisor writes the terminal audit record and returns the observed
   build identity.

An extension that is disabled, unreachable, missing the handler, or does not
publish the expected heartbeat fails closed after a bounded deadline. No
Computer Use fallback runs automatically.

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

Live evidence on 2026-07-14:

- the first handler-capable unpacked build was bootstrapped once manually;
- AkuBridge announced `aku-bridge-0.5.15-source-fidelity-v17` with
  `reload_self` in its capabilities;
- `aku-supervisor bridge reload` completed in one request and observed a
  heartbeat timestamp newer than the pre-action heartbeat;
- the cooperative journal stored consecutive `requested` and `completed`
  records with the same request ID and relay action ID;
- the AkuBrowser, X, LinkedIn, and extensions tabs retained the same Chrome tab
  IDs; and
- AkuSidecar remained healthy, running, and owned by AkuSupervisor after the
  reload.
