# AkuSupervisor

AkuSupervisor is a generic, configuration-driven supervisor for local development services.

Roadmap Gates 0 through 4 are complete, making this the first usable
AkuWorkspace MVP. The visible foreground supervisor provides validated
configuration, status, start, stop, restart, operator holds, exit cleanup,
authenticated local control, durable lifecycle events, bounded service logs,
and idempotent mutations. AkuSidecar has passed live start, reasoning, hard
restart, old-tree cleanup, and SQLite-preservation validation.
Gate 5 adds one authenticated cooperative action for AkuBridge self-reload and
has passed live Chrome validation without closing Chrome or its source tabs.

Rust is the implementation language, targeting `x86_64-pc-windows-msvc` for the
initial AkuWorkspace pilot. Platform-neutral application ports and separate
Windows, Linux, and macOS adapter boundaries keep future OS ports isolated from
the lifecycle core. Only the Windows adapter is implemented today.

## Why AkuSupervisor instead of a generic watcher

AkuSupervisor exists for a narrower problem than a general production process
manager: a local, authenticated, browser-integrated development stack can have
persisted work in flight, multi-process Windows trees, and cooperative actions
that must complete without taking over the user's screen. Merely observing that
a launcher PID is alive is not enough.

The first post-onboarding AkuBrowser update made this boundary concrete. A
Node file watcher restarted AkuSidecar while Codex SDK reasoning was in flight.
The HTTP interruption was brief, but the persisted session was left waiting for
recovery. AkuSupervisor replaced the complete old tree, preserved SQLite, and
made the recovery auditable; after the Sidecar watcher was removed, the same
session resumed and completed X plus LinkedIn without a replacement run.

| Approach | Strongest fit | Missing boundary for AkuWorkspace |
|---|---|---|
| `node --watch`, nodemon, or an npm watcher | Fast restart of a disposable development process after source changes | A restart can interrupt a persisted job at an arbitrary stage. It normally has no health contract, complete Windows-tree ownership, durable audit, or cooperative browser action |
| PM2 or another application process manager | Mature daemon restart, log, clustering, and Node-oriented production operations | It does not by itself define AkuBridge reload acceptance, source-aware health, SQLite-preserving development handoff, or the project's authenticated local control and MCP inspection boundary |
| Docker/Compose restart and health policies | Reproducible isolation, deployable images, and service-level restart | The signed-in host Chrome profile and unpacked extension live outside the container. Container restart does not coordinate that browser state, and container isolation is heavier than the current local pilot requires |
| `systemd`, Windows Service/NSSM, `supervisord`, or an IDE task runner | OS service startup or convenient local task launch | Generic lifecycle ownership does not provide AkuWorkspace's staged development handoff, bounded old-tree cleanup, cooperative extension reload, or one canonical cross-service audit trail |
| AkuSupervisor | Visible local ownership of AkuSidecar and AkuBridge development operations | It is intentionally not a container platform, cluster scheduler, or remote production orchestrator; only the Windows platform adapter is implemented today |

AkuSupervisor's differentiators are therefore the combination of:

- configuration validation before process mutation;
- ownership and cleanup of the complete launcher/child process tree;
- bounded graceful stop/restart with operator holds and a small recovery policy;
- HTTP JSON health that distinguishes transport readiness from process presence;
- durable lifecycle events and bounded stdout/stderr access;
- an authenticated local control API with stateless read-only MCP inspection;
- cooperative AkuBridge reload with requested, delivered, accepted, heartbeat,
  and completion evidence; and
- a development handoff that builds a staged Supervisor, keeps the old instance
  available until the build succeeds, restores previously running services,
  and keeps stable promotion as a separate explicit release action.

The AkuSupervisor development watcher is not the same as placing a managed
service under an in-process file watcher. It rebuilds AkuSupervisor itself into
a staging target and performs a bounded service-aware handoff. AkuSidecar now
runs without Node file watching; backend changes are restarted explicitly
through AkuSupervisor, while Vite retains frontend HMR.

The handoff intentionally replaces the Sidecar process. AkuSidecar 0.6.9 is a
worked example of how a managed application can make that boundary safe: it
publishes a new `instanceEpoch` on every start, allowing its browser client to
discard stale in-memory Bridge readiness and re-handshake without asking
AkuSupervisor to understand Chrome. This is application-level recovery layered
on top of Supervisor lifecycle ownership, not a service dependency graph.

Audit identity distinguishes these paths without changing cleanup:

- Ctrl+C, interactive `quit`, or stopping the watcher is `user/cli`; and
- a successful development build/configuration handoff is
  `recovery/supervisor`.

Both still use the same bounded graceful stop and complete owned-tree cleanup.
There is no detach-on-exit mode; closing the foreground owner means stopping
the services it owns.

Use the simpler tool when its boundary is sufficient. AkuSupervisor is justified
when local browser state, persisted jobs, full-tree cleanup, and cooperative
cross-component evidence must be treated as one lifecycle contract.

## Structure

```text
AkuSupervisor/
  docs/
    generic-local-development-supervisor-spec.md
    geofu-daily-workflows.md
    geolibre-portability.md
    geofu-plugin-portability.md
    implementation-roadmap.md
    mcp-integration-notes.md
    platform-portability.md
    testing-guide.md
  config/
    akuworkspace.services.json
    examples/
      immutable-windows.services.json
  examples/
    phase2_process_tree_demo.rs
  scripts/
    test-phase2.ps1
  schemas/
    service-config.schema.json
  src/
    adapters/
    application/
    domain/
    platform/
      windows/
      linux/
      macos/
    cli.rs
    lib.rs
    main.rs
  tests/
    cli_smoke.rs
  Cargo.toml
```

## Development

```powershell
cargo run -- --help
cargo run
cargo run -- --config C:\path\to\services.json
cargo run -- status
cargo run -- events --limit 20
cargo run -- logs akusidecar --stream stdout --tail 100
cargo run -- restart akusidecar --actor codex --reason "source changed"
cargo run -- bridge reload --actor codex --reason "load updated unpacked extension" --request-id "bridge-reload-001"
cargo run -- bridge status --request-id "bridge-reload-001" --json
cargo run -- bridge validate --actor codex --request-id "bridge-release-001"
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

For configuration fields and copyable examples—including programs whose
codebase cannot be changed—see the
[configuration guide](docs/configuration-guide.md). Source integration is not
mandatory: an uncooperative program is stopped through the bounded owned-tree
fallback and reports `forced: true`.

Run the complete verification suite through the convenience script:

```powershell
.\scripts\test-phase2.ps1
```

For live AkuSupervisor development, stop any normally running instance with
`quit`, then start the safe watcher:

```powershell
.\scripts\dev.ps1
```

The default starts only AkuSupervisor. To start one or more configured services
as soon as the development Supervisor is ready, pass their service IDs as
positional arguments. For the current AkuWorkspace profile use:

```powershell
.\scripts\dev.ps1 akusidecar
```

`AkuSupervisor` is not a managed service ID: it is always the process launched
by this script. Unknown service IDs fail before the build and list the IDs
available in the selected configuration. Service arguments do not replace the
normal watcher banner or transition guidance. Each successful automatic start
prints an additional ownership message, and Ctrl+C uses the same bounded
graceful Supervisor cleanup for the service and its process tree.

The watcher builds into a staging target while the current supervisor remains
available. A successful build requests normal graceful cleanup, replaces only
the constant `target\dev\aku-supervisor.exe`, starts it again, and restores the
services that were running before the restart. A failed build leaves the old
supervisor and its services untouched. It never replaces the normal
`target\aku-supervisor.exe` and never force-kills a timed-out process. Visual
Studio Code users can also run the `AkuSupervisor: development watcher` task.
Startup and every successful handoff also require exclusive write access to the
development executable. A portless Supervisor instance that is still holding
the file therefore fails closed with its PID when discoverable, instead of
reaching `Copy-Item` or silently creating a second instance.
If the staged and development executables are already byte-identical, no copy is
needed and a read-only MCP proxy holding the image does not block watcher
startup. When the bytes differ, the watcher immediately reports that it is
waiting, includes the matching PID when discoverable, and still fails closed
without killing the owner.
The watcher also observes its selected configuration file; a valid config
change receives the same graceful handoff without restarting the watcher.
The watcher owns stdin; use the control CLI from a second terminal for
`status`, `start`, `stop`, or `restart` while it is active.
It prefers the complete project-local Rust toolchain over any rustup shim found
on `PATH`, and prints the selected Cargo and Rust compiler paths before build.
At startup and after each successful rebuild it also prints `Stable status` as
`CURRENT`, `OUTDATED`, or `MISSING`, followed by the exact transition commands.
See the [testing guide](docs/testing-guide.md#7-development-watcher).

Promoting the constant development binary to the normal stable path is a
separate, fail-closed release action:

```powershell
.\scripts\promote-stable.ps1
```

Run promotion at a release checkpoint, after the current development build has
passed its tests and live validation and you want future normal launches to use
it. Do not run it after every watcher rebuild or while a feature is still being
implemented.

Running without the watcher does not always require another promotion. If the
watcher reports `Stable status: CURRENT`, stop it with Ctrl+C and run
`.\target\aku-supervisor.exe`. If it reports `OUTDATED` or `MISSING` and the
latest development build should become normal, use this order:

1. keep the watcher and managed services running;
2. run `.\scripts\promote-stable.ps1` from a second terminal;
3. return to the watcher and press Ctrl+C for graceful cleanup; and
4. run `.\target\aku-supervisor.exe`.

Promotion is performed before stopping the watcher because its AkuBridge
release validation needs the supervised Sidecar and extension bridge alive.
If the script reports `relay_page_stale`, keep the watcher and services running,
reload only the existing `http://127.0.0.1:47821` AkuBrowser tab, wait until it
shows both AkuSidecar and AkuBridge ready, and rerun promotion. This restores
the page's cooperative relay poller; it does not require restarting Chrome.
Before spending that release gate, the promotion script acquires exclusive
access to the stable executable. If a normal Supervisor or long-lived
`mcp-proxy` is using `target\aku-supervisor.exe`, promotion fails immediately,
prints candidate PIDs, and does not invoke AkuBridge. Keep the watcher and its
supervised AkuSidecar running; recycle only the process using the stable path,
then rerun promotion. The lock is checked again immediately before copy.

After the lock preflight, the script checks that the configured `akusidecar`
service is both running and healthy. It does not start the service implicitly;
when it is stopped, the script prints the exact supervised start command and
leaves stable unchanged.

The script runs `target\dev\aku-supervisor.exe bridge validate` with a fresh
request ID before copying anything. Validation emits one JSON document and
requires cooperative completion, all six audit stages, matching actor/request
identity, matching expected/observed heartbeat builds, and no active zombie
operation. Exit code `0` means passed; `1` means validation/execution failed;
CLI usage errors remain exit code `2`. A failed or malformed result leaves
`target\aku-supervisor.exe` unchanged.

For a visible process-tree and Ctrl+C cleanup demo, follow the
[testing guide](docs/testing-guide.md).

While `run` is active, use `status`, `start <service> [reason]`,
`stop <service> [reason]`, `restart <service> [reason]`, and `quit` in the same
terminal.

From a second terminal, use the control client while the foreground supervisor
continues running visibly:

```powershell
cargo run -- status
cargo run -- start akusidecar
cargo run -- restart akusidecar --actor codex --reason "backend source changed"
cargo run -- events --after 0 --limit 20
cargo run -- logs akusidecar --stream stderr --tail 100
cargo run -- stop akusidecar --reason "manual development stop"
```

When `control.mcp.enabled` is `true`, the same running Supervisor exposes an
authenticated, stateless, read-only MCP endpoint at
`http://127.0.0.1:<control-port>/mcp`. It advertises exactly four tools:

```text
supervisor_list_services
supervisor_get_service
supervisor_get_recent_events
supervisor_read_logs
```

MCP cannot start, stop, restart, reload, or bootstrap anything. It uses the
existing runtime bearer token, rejects tokens in URLs, rejects any `Origin`
not explicitly listed in `control.mcp.allowedOrigins`, and caps tool results.
Native MCP clients normally omit `Origin`; an empty allow-list therefore keeps
browser-originated requests closed. Validate the active endpoint without
printing the token:

```powershell
.\scripts\test-mcp.ps1
```

Disabling or removing `control.mcp` removes `/mcp` while leaving CLI, HTTP,
service ownership, health monitoring, and cleanup unchanged. See
[MCP integration notes](docs/mcp-integration-notes.md).

Codex can use the protected runtime token without copying it into Codex config
through the stdio compatibility proxy:

```toml
[mcp_servers.aku_supervisor]
command = "C:\\WorkspaceCodex\\AkuWorkspace\\AkuSupervisor\\target\\aku-supervisor.exe"
args = ["mcp-proxy"]
enabled_tools = [
  "supervisor_list_services",
  "supervisor_get_service",
  "supervisor_get_recent_events",
  "supervisor_read_logs",
]
```

The proxy reads newline-delimited MCP from stdin, reads the existing ACL-
protected token file, and forwards only to the already-running `/mcp` endpoint.
It never starts AkuSupervisor or a managed service. AkuWorkspace checks this in
at `.codex/config.toml`; restart Codex or begin a new task after changing MCP
configuration. During development it may temporarily point at
`target\dev\aku-supervisor.exe`; after promotion it should return to the stable
`target\aku-supervisor.exe` shown above.

After the Gate 5 build has been loaded into Chrome once, either the user or
Codex can request the only browser-side mutation exposed by AkuSupervisor:

```powershell
.\target\aku-supervisor.exe bridge reload `
  --actor codex `
  --reason "load updated AkuBridge build" `
  --request-id "bridge-reload-20260714-001"
```

The command relays `reload_self` through the open AkuBrowser tab, stores one
short-lived originating-tab marker, invokes `chrome.runtime.reload()`, and lets
the new worker invoke `chrome.tabs.reload()` for only that local tab. It
succeeds only after Sidecar observes the expected new build heartbeat.
Delivery uses a long poll that starts only after the local page completes a
compatible AkuBridge capability handshake. An AkuBrowser URL opened in a
browser without the extension remains passive and cannot consume the action.
The eligible background AkuBrowser tab therefore does not depend on a
one-second page timer. The CLI waits by default; use `--no-wait` and later
`bridge status --request-id <id>` when asynchronous control is preferable.
Transport failures are retried with bounded backoff only for reads and
idempotent requests; mutations without a request ID are never retried.
Use `--json` on any remote command for one machine-readable JSON envelope with
the configuration path, control API, and response. It does not expose arbitrary
extension commands, Chrome management, CDP, tab closure, or whole-browser restart. See the
[AkuBridge reload design](docs/aku-bridge-reload.md).

`user` is the default client actor. A user CLI mutation without `--reason`
receives a bounded reason such as `user CLI start request` at the CLI boundary;
an explicit reason remains available when more context is useful. Codex must
pass `--actor codex` and an explicit `--reason`. Every request sent to the
control protocol therefore contains a reason and can select only a service
already registered in configuration; executable paths and arguments are never
accepted by the control protocol. Audit and operation responses preserve Codex as
`{"actorType":"agent","actorId":"codex"}` while older string-valued journal
records remain readable. Use `--request-id <id>` when a caller may retry a
mutation; an identical retry replays the original response, while reusing the
ID for different input is rejected. A user stop creates a hold that blocks a later Codex
start or restart until a user explicitly starts or restarts the service.

Without `--config`, AkuSupervisor resolves configuration in this order:

1. `AKU_SUPERVISOR_CONFIG` environment variable;
2. `%LOCALAPPDATA%\AkuSupervisor\services.json` on Windows.

If the selected file does not exist, startup fails clearly and does not create
an empty configuration. On successful startup, the terminal prints the
absolute configuration path and whether it came from `--config`,
`AKU_SUPERVISOR_CONFIG`, or the default user location.

Startup also prints the loopback API address, token-file path, lifecycle
journal, and service-log directory. A 256-bit token is generated on first
startup using Windows CNG and its file receives a protected current-user-only
DACL. The token value is never printed and must not be pasted into commands;
the client reads it from the configured runtime file.

Runtime artifacts use this layout:

```text
.runtime/
  control-token
  supervisor.jsonl
  cooperative-actions.jsonl
  services/
    akusidecar.stdout.log
    akusidecar.stderr.log
```

Each output stream rotates continuously at 5 MB and retains five generations.
The `events` command returns at most 200 records per request; `logs` returns at
most 1,000 lines from the active generation.

The lifecycle journal is always complete. The optional root setting
`observability.consoleEvents` controls whether its persisted records are also
mirrored to the visible Supervisor terminal: `off`, `lifecycle` (the default),
or `verbose`. Failures remain visible on stderr even in `off` mode. The default
prints one bounded line such as:

```text
[event #244] geofu-be start: stopped -> running (user/cli, success)
```

See the [configuration guide](docs/configuration-guide.md) for the distinction
between Supervisor lifecycle events and managed-service stdout/stderr logs.

The Windows proof for the npm/Rollup service, including the generic HTTP
chunked-transfer health fix and graceful owned-tree shutdown evidence, is in
the [Geofu plugin portability proof](docs/geofu-plugin-portability.md).
The development, locked-QA, and production deployment boundaries across all
three Geofu-family repositories are mapped in the
[Geofu daily workflows](docs/geofu-daily-workflows.md).

The checked-in canonical AkuWorkspace profile is
[`config/akuworkspace.services.json`](config/akuworkspace.services.json). It
registers AkuSidecar, Geofu BE, the Geofu plugin development server, and two
distinct GeoLibre modes behind one control API, runtime token, and
read-only MCP boundary. All services remain `manual`; registration does not
start any Geofu-family service implicitly. `geolibre` is the normal unlocked
development mode; `geolibre-locked` is the explicit bundled-plugin QA mode.
AkuSidecar is the Go `1.0.0-dev.1` fresh boundary: the profile starts its Go
watcher, requires Bridge v2 plus `codex-app-server`, and uses the new SQLite schema.
Copy the profile to the default user location for argument-free startup:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\AkuSupervisor"
Copy-Item .\config\akuworkspace.services.json "$env:LOCALAPPDATA\AkuSupervisor\services.json"
```

Keep the service profile's `environment` map empty for normal AkuBrowser use.
Provider, models, efforts, timeout, sources, and bounded engine settings belong
in the strict Sidecar configuration and AkuBrowser Settings surfaces.
AkuSidecar no longer accepts legacy environment aliases.

The profile's health contract is enforced at runtime. Start and restart wait up
to `startupDeadlineMs`; afterward a one-second monitor keeps `processReady`,
`transportReady`, health status, detail, and check time current. A process that
starts successfully but misses its health contract remains Supervisor-owned and
enters `unhealthy`, distinct from `spawn_failed`. A later successful probe
returns it to `running`.

The same monitor reconciles unexpected process-tree exits. It releases an owner
only after the complete owned tree is empty and the launcher exit status is
available; a launcher that exits while a watcher/server descendant remains is
not treated as terminal. Snapshots expose `desiredState`, `startedAtUnixMs`,
`lastExitCode`, `lastExitAtUnixMs`, and `restartCount`.

`restartPolicy: "manual"` leaves an exited service in `failed` and permits a
later authenticated start. `restartPolicy: "on-failure"` journals the exit and
attempts at most one recovery restart after a nonzero exit. A second exit inside
the 60-second stability window stays `failed`; an explicit stop always wins a
race with recovery.

Successful stop and restart responses include a platform-neutral `shutdown`
object. It exposes `ownedPidsBefore`, `ownedPidsAfter`,
`gracefulSignalSent`, an optional `gracefulSignalError`, and `forced`. The same
object is persisted on the lifecycle journal record, so operators and MCP
clients can distinguish cooperative shutdown from the bounded forced fallback
without inferring it from elapsed time or service logs.

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Configuration guide](docs/configuration-guide.md)
- [Cooperative shutdown recipes](docs/cooperative-shutdown-recipes.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Testing guide](docs/testing-guide.md)
- [Platform portability boundary](docs/platform-portability.md)
- [MCP integration notes](docs/mcp-integration-notes.md)
- [AkuBridge cooperative reload](docs/aku-bridge-reload.md)
- [Geofu BE portability proof](docs/geofu-be-portability.md)
- [Geofu plugin portability proof](docs/geofu-plugin-portability.md)
- [GeoLibre portability proof](docs/geolibre-portability.md)
- [Geofu daily development and deployment workflows](docs/geofu-daily-workflows.md)
