# AkuSupervisor Testing Guide

Current test scope: **Lifecycle ownership, authenticated control, read-only MCP,
and AkuBridge cooperative reload**

The visible CLI loads a validated configuration and accepts lifecycle commands
in its own terminal. The same registry is reachable from a separate bounded CLI
through the loopback HTTP adapter; mutations require the runtime bearer token.

## 1. Automated verification

Open the integrated PowerShell terminal in Visual Studio:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
.\scripts\test-phase2.ps1
```

The script runs formatting, strict Clippy, and all test targets. The final line
must begin with `PASS:`.

The important behavioral tests prove that:

- one owned parent and descendant are stopped together;
- an unrelated process remains alive;
- dropping the owner closes its Job Object and cleans up the tree;
- a port occupant is reported without being stopped;
- sixteen concurrent starts create only one owner; and
- console interruption cleans the owned tree before the fixture supervisor exits;
- the foreground CLI completes start, status, restart, stop, and quit against a
  real owned fixture tree;
- a separate client process controls that same registry through loopback HTTP;
- an invalid bearer token never reaches the lifecycle core; and
- a user stop hold rejects a later Codex start without changing state.

## 2. Foreground supervisor

Given a valid service configuration:

```powershell
cargo run -- --config C:\path\to\services.json
```

For normal daily use, place the file at:

```text
%LOCALAPPDATA%\AkuSupervisor\services.json
```

The [configuration guide](configuration-guide.md) contains the complete field
contract plus examples for immutable programs, HTTP health, cooperative direct
executables, and command wrappers.

Then startup requires no application arguments:

```powershell
cargo run
```

For this AkuWorkspace pilot, the source profile is checked in at:

```text
C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor\config\akuworkspace.services.json
```

It registers `akusidecar`, the built `geofu-be` executable, the `geofu-plugin`
npm/Rollup watcher, and the `geolibre` unlocked and `geolibre-locked` QA modes
as independently controlled manual services behind one control API.
Registration does not start any service implicitly. Unlocked GeoLibre uses port
6060 through the repository-owned `geofu:lan` HTTPS wrapper and loopback TCP
readiness; locked QA uses the configured HTTP override 6061. The LAN wrapper
requires its one-time certificate setup before supervised startup.
Startup prints both the absolute selected path and its source, for example:

```text
Configuration: C:\Users\Force\AppData\Local\AkuSupervisor\services.json
Configuration source: default user configuration
Control API: http://127.0.0.1:47820
Control token: C:\Users\Force\AppData\Local\AkuSupervisor\.runtime\control-token
```

`AKU_SUPERVISOR_CONFIG` may override the default location. An explicit
`--config` path has the highest priority.

To verify loading without starting AkuSidecar, enter `quit` at the prompt. To
perform the first manual lifecycle test, use:

```text
status
start akusidecar initial supervised launch
status
stop akusidecar manual test complete
quit
```

The configured `/api/health` expectation is enforced during start and restart.
`running` means both the owned process tree and configured health expectation
passed. A spawned but mismatching service remains owned and reports
`unhealthy`, so it can be diagnosed, stopped, or restarted safely.

The terminal displays the current service table. Available commands are:

```text
status
start <service> [reason]
stop <service> [reason]
restart <service> [reason]
help
quit
```

Executable, arguments, working directory, and environment always come from the
validated configuration. Interactive input can select only a registered
service and supply an auditable reason.

## 3. Separate terminal or Codex control

Leave `cargo run` active in the first terminal. In a second terminal:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
cargo run -- status
cargo run -- simple-status
cargo run -- start akusidecar
cargo run -- restart akusidecar --actor codex --reason "source changed"
cargo run -- stop akusidecar --reason "manual supervised stop"
```

`simple-status` prints the same compact service table used by the watcher;
`status` retains the detailed human JSON view and `status --json` remains the
machine-readable envelope. The default actor is `user`. For a user CLI
mutation, `--reason` is optional;
when omitted the client supplies a bounded audit reason such as
`user CLI start request`. Codex uses `--actor codex` and must supply an explicit
`--reason`. The client discovers the same configuration and token file as the
server, then prints the absolute configuration path, API address, and bounded
JSON response. It never accepts executable, argument, environment, or
working-directory input.

Read-only service status is loopback-visible. Start, stop, and restart require a
valid bearer token read from the runtime file. The token itself is never printed.

## 3.1 Read-only MCP

With the Supervisor active and `control.mcp.enabled=true`, run:

```powershell
.\scripts\test-mcp.ps1
```

The script discovers the same configuration and token file as the Supervisor,
but never prints the token. It verifies protocol `2025-11-25`, the exact four-
tool read-only surface, service/event/log reads, absence of mutation tools, and
HTTP `403` for an untrusted `Origin`. A passing MCP check does not mean MCP can
start AkuSupervisor: the endpoint exists only inside an already-running,
user-visible Supervisor.

The Windows integration suite also launches `aku-supervisor mcp-proxy`, sends
initialize and tools/list as newline-delimited stdio messages, and verifies the
same four-tool response. The proxy test proves compatibility without granting
the restricted child any service-ownership or bootstrap role.

For AkuBridge reload, the CLI waits for the terminal heartbeat by default:

```powershell
.\target\aku-supervisor.exe bridge reload --actor codex `
  --reason "load updated extension" --request-id "bridge-reload-1"
```

To separate submission from observation, add `--no-wait`, then query:

```powershell
.\target\aku-supervisor.exe bridge status --request-id "bridge-reload-1" --json
```

`--json` emits one JSON envelope and no human metadata on stdout. During an
active reload, the same request ID must replay its current snapshot and a new
request ID must fail with `action_in_progress`.

## 4. Visible manual process-tree demo

Build the demo, then execute it directly so Cargo is not an intermediary for
the Ctrl+C signal:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
cargo build --example phase2_process_tree_demo
.\target\debug\examples\phase2_process_tree_demo.exe
```

The demo prints one supervisor PID and at least two owned PIDs. It waits for 30
seconds, or you can press Ctrl+C. A successful cleanup ends with output similar
to:

```text
Cleanup complete.
Before: [1234, 5678]
After : []
Forced: false
```

`After: []` is the essential assertion. `Forced` may be either `false` or
`true`: it records whether the child cooperated before the bounded forced path.

To inspect the processes while the demo is waiting, open a second PowerShell
terminal and use the PIDs printed by the demo:

```powershell
Get-Process -Id <PID1>,<PID2>
```

After cleanup, the same command should report that those processes no longer
exist. Do not substitute unrelated PIDs into any termination command; the demo
itself only operates on its Job Object.

## 5. Gate 3 operational checks

With the visible supervisor running, use a second terminal:

```powershell
.\target\aku-supervisor.exe events --limit 20
.\target\aku-supervisor.exe logs akusidecar --stream stdout --tail 100
.\target\aku-supervisor.exe restart akusidecar --actor codex `
  --reason "idempotency check" --request-id "manual-restart-1"
```

Repeating the last command with the same ID and body must replay its original
response without creating another lifecycle event. Reusing that ID with a
different reason must fail with HTTP `409`.

With `observability.consoleEvents` set to its default `lifecycle`, the visible
Supervisor/watcher terminal must also receive exactly one concise line for the
new canonical event. The line includes the same journal sequence, service,
action, state transition, actor, and result. Set the value to `verbose` to add
reason, PID counts, error category, exit code, and shutdown evidence, or `off`
to keep successful events journal-only. Failures remain visible in every mode.

A successful stop or restart that replaced an owned tree must include
`response.shutdown` in `--json` output. Confirm that `ownedPidsAfter` is empty
and inspect `gracefulSignalSent` plus `forced`; the matching `events` record
must contain the same shutdown object. `forced: true` is bounded cleanup
evidence, not a successful cooperative shutdown claim.

The token file must have a protected current-user-only Windows DACL. The
default suite verifies this in the normal host context because a restricted
sandbox is not permitted to change file DACLs.

Service output is captured beneath `.runtime/services`. Each active file is
limited to 5 MB and keeps five rotated generations. `logs` reads only the
active file and bounds output to 1,000 lines.

## 6. Runtime health

AkuSupervisor supports `process`, `http-status`, and shallow `http-json`
checks. HTTP health URLs must use an explicit loopback IP and port. Start and
restart retry until `startupDeadlineMs`; a one-second background monitor then
updates lifecycle and cached health independently of `status` reads.

Verify the live snapshot from a second terminal:

```powershell
.\target\dev\aku-supervisor.exe status --json
```

Inspect `response.services[].health`: `processReady` proves the owned tree is
present, `transportReady` proves an HTTP response was obtained, and `status`
proves the configured expectation matched. `checkedAtUnixMs` should advance
between reads even when no lifecycle command is issued. HTTP health is still a
transport/contract check; AkuSidecar's real Codex reasoning invocation remains
an application-level readiness validation.

The HTTP adapter also decodes bounded `Transfer-Encoding: chunked` responses.
The canonical `geofu-plugin` service exercises this path through Node's native
HTTP server at `http://127.0.0.1:8766/geofu/plugin.json`; its health expectation
matches the stable `id: geofu` field rather than a release-specific version.

The LAN GeoLibre profile uses `tcp-connect` readiness on 6060 because its Vite
listener is HTTPS; the probe must not bypass or duplicate GeoLibre's certificate
policy. Locked QA uses `http-status` against `/favicon.png` on 6061. For a full
daily-dev check, `geofu-be`, `geofu-plugin`, and `geolibre` must report healthy.
Validate that the locked profile honors its 6061 override and does not steal the
LAN listener.

Locked QA additionally requires the operator-run `npm run deploy:geolibre`
copy before start or restart. Deployment commands are not test services; the
complete boundary is documented in
[Geofu daily workflows](geofu-daily-workflows.md).

### Process-exit supervision

The same one-second monitor distinguishes a failed health expectation from a
terminal process tree. A root/launcher exit is not terminal while a verified
owned descendant remains. Once the complete tree is empty, AkuSupervisor:

1. captures the launcher exit code;
2. releases the stale owner and changes lifecycle to `failed`;
3. records a `process_exit` journal event before recovery;
4. leaves `manual` services available for an authenticated start; and
5. performs at most one `on-failure` recovery per unstable episode.

Use `status --json` and inspect `desiredState`, `startedAtUnixMs`,
`lastExitCode`, `lastExitAtUnixMs`, and `restartCount`. The deterministic crash
fixture is covered by:

```powershell
cargo test --test process_supervision --features test-fixtures
```

It proves manual recovery, one automatic restart, second-crash suppression,
owner release, and the exit/recovery audit records. A user or agent stop that
arrives before the planned recovery changes desired state to `stopped` and
suppresses that recovery.

## 7. Development watcher

The repository includes a Windows development watcher that does not require a
globally installed watcher package. First stop the normally running stable
Supervisor by typing `quit`; this one-time handoff prevents two owners from
competing for the same control port. Then run:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
.\scripts\dev.ps1
```

If a non-default profile is required:

```powershell
.\scripts\dev.ps1 -Config C:\path\to\services.json
```

To start AkuSidecar automatically after the Supervisor becomes ready:

```powershell
.\scripts\dev.ps1 akusidecar
```

Additional configured service IDs may be supplied positionally. With no service
argument, the watcher retains its original Supervisor-only behavior. The script
validates every requested ID against the selected configuration before building;
`akusupervisor` is not a service ID because `dev.ps1` already starts it.

For every later source or configuration change, the watcher first builds the
staged binary and uses it to validate the selected configuration before asking
the running Supervisor to stop. If validation fails, the current Supervisor and
its services remain active and the watcher prints the structured validation
path, code, and message. A malformed edit therefore cannot turn a safe live
handoff into avoidable service downtime.

Requested startup services are independent. If one service misses its startup
contract, `dev.ps1` warns and continues to own the Supervisor and every service
that did start; it does not tear down the complete development stack. Inspect
the retained service with `status` and `logs`, then stop or restart it normally.
After automatic startup succeeds, the standard watcher banner is still printed
and an additional message confirms that the service is owned by the development
Supervisor. Ctrl+C requests the ordinary graceful Supervisor shutdown, waits
for owned-service cleanup, and prints a completion message; it never kills the
service tree directly.

After an automatic build/configuration handoff, inspect the lifecycle journal:
cleanup from the replaced instance uses actor `recovery/supervisor` and a
`development watcher handoff:` reason. Ctrl+C, interactive `quit`, and stopping
the watcher use `user/cli`. In every case, `ownedPidsAfter` must be empty.

For applications following the `instanceEpoch` example, also verify that the
new process returns a different epoch, the existing client discards its old
integration-ready state, and the bounded application handshake completes
without changing Supervisor ownership or health policy.

The terminal prints the constant executable path
`target\dev\aku-supervisor.exe`. The script polls `src/**/*.rs`, `Cargo.toml`,
and `Cargo.lock`, with a short debounce. Its rebuild sequence is:

1. compile incrementally in `target\dev-build` while the old process stays up;
2. leave the old process and services untouched when compilation fails;
3. remember which registered services are running after compilation succeeds;
4. write the opt-in local `shutdown-request` signal;
5. wait for the foreground Supervisor to close its API and owned Job Objects;
6. prove the constant development executable is exclusively replaceable;
7. copy the staged build over the constant development executable;
8. launch it and restore the previously running services.

The startup and post-rebuild banner distinguishes the two executable roles:

- `Active executable` is always `target\dev\aku-supervisor.exe` while the
  watcher is running;
- `Normal stable executable` is `target\aku-supervisor.exe`; and
- `Stable status` is `CURRENT` only when both files have the same SHA-256.

To leave development mode, follow the banner. When status is `CURRENT`, press
Ctrl+C and launch the stable executable. When status is `OUTDATED` or
`MISSING`, keep the watcher running, promote from a second terminal, then press
Ctrl+C and launch stable. Core promotion has no Sidecar or browser prerequisite.

Before the first build, the watcher prints its selected `Cargo:` and `Rustc:`
paths. It deliberately prefers the complete toolchain under
`target/rustup-home/toolchains` over a `cargo` or `rustc` rustup shim on
`PATH`. This prevents an incomplete user-level rustup selection from changing
the repository's development compiler. `scripts\test-phase2.ps1` uses the
same shared resolver, so both the watcher and the release test gate select one
verified toolchain instead of merely trusting that a rustup proxy exists.

The signal exists only when the watcher sets
`AKU_SUPERVISOR_DEV_SHUTDOWN_FILE`. AkuSupervisor accepts only an absolute path
whose filename is exactly `shutdown-request`, consumes at most 1 KiB, and uses
the ordinary cleanup path. No development shutdown HTTP endpoint or arbitrary
command execution is exposed.

Press Ctrl+C in the watcher terminal to request the same graceful shutdown. If
cleanup exceeds the configured timeout, the watcher refuses to kill the
process or replace its executable. The checked-in VS Code task
`AkuSupervisor: development watcher` launches the same script.

The control port is not the only ownership signal. At startup and after a
graceful exit, the watcher opens `target/dev/aku-supervisor.exe` with exclusive
read/write sharing before replacing it. If an older, portless Supervisor still
holds the image, startup fails closed and reports the matching PID when Windows
allows its path to be inspected. Stop only that confirmed PID manually, then
start the watcher again; the script never force-kills it automatically.

An existing read-only MCP proxy may legitimately hold the development image.
After compiling, the watcher compares SHA-256 hashes: if staged and development
bytes are identical it skips the unnecessary copy and continues; if they
differ, it prints the wait and PID diagnostics immediately and retains the
fail-closed replacement rule.

Watcher mode intentionally does not give the child process direct ownership of
stdin, because editor background tasks and automation hosts may immediately
send EOF. The terminal remains the visible process/log surface. Use another
terminal for manual control while the watcher is active:

```powershell
.\target\dev\aku-supervisor.exe status
.\target\dev\aku-supervisor.exe restart akusidecar --actor user `
  --reason "manual change while watcher is active"
```

This runner is a Windows adapter. The Rust file-signal adapter itself uses only
portable standard-library APIs, so a later Linux/macOS runner can preserve the
same build-first and graceful-handoff contract using `watchexec`, a shell
script, or a native host adapter.

## 8. Stable release promotion

The development watcher never overwrites `target\aku-supervisor.exe`. Promote
the current constant development build only through:

```powershell
.\scripts\promote-stable.ps1
```

Promotion is a release checkpoint, not part of the inner development loop. Run
it once the feature set is complete, tests and live checks pass, and the stable
path should become the default for subsequent normal launches.

The script runs a bounded `--version` preflight, skips the copy when stable and
development hashes are already identical, then requires exclusive access to
`target\aku-supervisor.exe`. A
normal stable Supervisor or long-lived MCP proxy can keep that Windows image
locked. In that case the script prints candidate PIDs and leaves stable
unchanged. Stop or recycle only the process using the stable path, then rerun
promotion. After copying, the script verifies the promoted SHA-256.

Core promotion deliberately does not inspect Supervisor status and does not
contact AkuSidecar or AkuBridge. The optional AkuWorkspace integration gate is:

```powershell
.\scripts\validate-akuworkspace-integration.ps1
```

Run this separately when a change touches cooperative reload or another
AkuSidecar/AkuBridge boundary. The integration script never copies the stable
binary. It requires `akusidecar` to already be `running / healthy`, then invokes
the machine-readable `bridge validate` command.

For that optional gate, `relay_page_stale` means Sidecar remained healthy but no open AkuBrowser page
requested the queued cooperative action before its deadline. Keep the watcher
and services running, reload only the existing
`http://127.0.0.1:47821` tab, wait for both ready indicators, and rerun
integration validation. Do not stop Chrome, reload the extension manually, or
close the watcher.

Use `-Config` on the integration script for a non-default profile, `-Actor
codex` when Codex owns the validation, and `-RequestId` only when an external
system supplies a guaranteed-fresh ID. Otherwise it creates a unique bounded
ID.

The separate integration script runs `bridge validate`. Its JSON
contract contains `schemaVersion`, `status`, `exitCode`, the structured actor,
request ID, terminal operation, and five named checks. The six required audit
stages are `requested`, `relay_created`, `delivered`, `accepted`,
`heartbeat_observed`, and `completed`. A reused request ID, mismatched build,
missing audit stage, active operation, malformed JSON, or nonzero exit fails
the integration check without touching the stable executable.

The validator and audit evaluator are platform-neutral but AkuWorkspace-specific.
The PowerShell copy remains the independent Windows core release adapter.

## 9. Human-gated service registration

The registration contract is covered by unit and CLI tests in the ordinary
suite. A safe smoke test against the real profile performs discovery only:

```powershell
.\target\dev\aku-supervisor.exe registration capabilities --json

'{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' |
  .\target\dev\aku-supervisor.exe registration-mcp
```

The tool list must contain six registration tools and no approval tool. The
capabilities result must show the selected absolute configuration path,
current SHA-256 revision, `autoStart: false`, and
`approvalAvailableThroughMcp: false`.

Do not create a disposable draft against the real AkuWorkspace profile merely
to test persistence. The automated fixtures create an isolated valid profile
and prove:

- same-request prepare idempotency and conflicting request-ID rejection;
- base-revision conflict rejection;
- commit rejection before approval;
- hash-bound approval and atomic register commit;
- registered-but-stopped result with no auto-start;
- commit recovery/idempotency;
- update rejection when stopped state cannot be proved; and
- secret-like environment-key rejection.

For an intentional real registration, follow the MCP-provided workflow and
read the entire CLI approval output. After commit, let the development watcher
complete its configuration handoff, confirm the service appears as `stopped`
with `simple-status`, and start it only through a separate lifecycle command.
