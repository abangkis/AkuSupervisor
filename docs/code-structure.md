# Code and dashboard structure

AkuSupervisor keeps lifecycle policy in the domain and application layers. Adapters
translate operator input, browser requests, and operating-system observations into
those existing contracts. A new UI action must use application control rather than
introducing a second service lifecycle implementation.

## Rust control adapter

- `src/adapters/control_http.rs` owns the listener, HTTP request handling, routes,
  authenticated dispatch, and idempotency. Its existing public client exports are
  retained so CLI and MCP callers do not depend on internal file layout.
- `control_http/client.rs` owns bounded client requests, response decoding, live-log
  readers, and client errors. Ordinary JSON and MCP responses share framing and
  JSON decoding; the MCP notification path still accepts an empty HTTP 202.
- `control_http/dashboard.rs` owns the same-origin browser session and scoped
  Start/Stop/Restart policy. It forwards authorized requests to the existing
  mutation dispatcher, preserving actor identity, holds, audit, and idempotency.
- `control_http/tests.rs` exercises the real listener with isolated fake control
  backends, including browser security and the exact embedded asset bytes.
- `dashboard_assets.rs` is the explicit route-to-embedded-bytes allowlist. The
  production server and disposable browser fixture use this same list. It never
  serves arbitrary filesystem paths.

## Single-page dashboard

The dashboard remains native browser ES modules, HTML, and CSS embedded in Rust.
There is no Node.js runtime, package manager, bundler, or additional dependency.

| File | Responsibility |
| --- | --- |
| `dashboard/index.html` | Semantic page shell, summary, filters, accessible service table |
| `dashboard/app.css` | Shared page and responsive presentation |
| `dashboard/model.js` | Pure lifecycle/action eligibility, filtering, retention, labels, URLs |
| `dashboard/view.js` | DOM rendering and expanded-row state; action callbacks to the controller |
| `dashboard/api.js` | Bounded HTTP requests and scoped mutation envelopes; injectable transport for tests |
| `dashboard/app.js` | Page-local state, refresh/session coordination, mutation ordering, visibility-aware polling |

Dependency direction is controller to model/view/API; the model has no DOM or
network side effects. The view receives a render snapshot and emits action intent.
The controller decides whether an intent can proceed. The API adapter never
automatically retries a service mutation after a timeout, unreadable response, or
expired session.

Restart retention is session-local: a service remains visible in the Running
filter while recovering from a requested restart. A successful running snapshot
or removal from the registry clears its retention. Search still applies. Expanded
process details survive refresh and their state is discarded when a service is
removed.

When adding an asset, add its explicit route to `dashboard_assets.rs` and extend
the HTTP asset regression. Cargo watches the dashboard directory; the development
watcher recursively fingerprints its HTML, CSS, and JavaScript files. Unknown
asset routes remain unavailable.

## MCP operational scripts

`scripts/mcp-host-common.ps1` contains read-only helpers for section matching,
configured host selection, and executable contract reports. The installer and
status entrypoints import these helpers, retaining their own output and error
formats. Missing or duplicate MCP sections do not resolve to a host; conflicting
read-only/registration host paths are rejected. The installer still requires its
hash-bound approval workflow and is the only one of these entrypoints that can
apply configuration changes.

## Verification and cleanup boundaries

Use `scripts/test-phase2.ps1` for formatting, strict Clippy, and fixture tests.
Browser checks are described in [the testing guide](testing-guide.md#dashboard-browser-regression).
Fixture binaries, logs, and verification screenshots belong in `target/`.

Compatibility readers for old configuration and journal records are active data
contracts, not unused code. Platform modules, examples, promotion scripts, and
registration recovery logic also remain intentional boundaries. Do not remove
them solely because normal dashboard operation does not call them.
