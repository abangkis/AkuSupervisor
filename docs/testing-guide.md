# AkuSupervisor Testing Guide

Current test scope: **Phase 2 ownership and Phase 3 local-control checkpoint**

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

Then startup requires no application arguments:

```powershell
cargo run
```

For this AkuWorkspace pilot, the source profile is checked in at:

```text
C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor\config\akuworkspace.services.json
```

It registers `akusidecar` using `C:\nvm4w\nodejs\npm.cmd run dev` from the
AkuSidecar repository. Startup prints both the absolute selected path and its
source, for example:

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

The configured `/api/health` expectation is reserved for Phase 4. At the current
checkpoint, `running` means the owned process tree was launched; it does not yet
mean the HTTP JSON expectation passed.

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
cargo run -- start akusidecar --reason "manual supervised start"
cargo run -- restart akusidecar --actor codex --reason "source changed"
cargo run -- stop akusidecar --reason "manual supervised stop"
```

The default actor is `user`; Codex uses `--actor codex`. `--reason` is mandatory
for every mutation. The client discovers the same configuration and token file
as the server, then prints the absolute configuration path, API address, and
bounded JSON response. It never accepts executable, argument, environment, or
working-directory input.

Read-only service status is loopback-visible. Start, stop, and restart require a
valid bearer token read from the runtime file. The token itself is never printed.

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

The token file must have a protected current-user-only Windows DACL. The
default suite verifies this in the normal host context because a restricted
sandbox is not permitted to change file DACLs.

Service output is captured beneath `.runtime/services`. Each active file is
limited to 5 MB and keeps five rotated generations. `logs` reads only the
active file and bounds output to 1,000 lines.

## 6. Remaining runtime-health boundary

AkuSidecar live validation passed with `/api/health`, a real `codex-sdk`
reasoning invocation, hard restart, old-tree cleanup, and SQLite preservation.
AkuSupervisor does not yet turn configured HTTP health into automatic
lifecycle decisions; that remains outside the completed MVP gates.

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

The terminal prints the constant executable path
`target\dev\aku-supervisor.exe`. The script polls `src/**/*.rs`, `Cargo.toml`,
and `Cargo.lock`, with a short debounce. Its rebuild sequence is:

1. compile incrementally in `target\dev-build` while the old process stays up;
2. leave the old process and services untouched when compilation fails;
3. remember which registered services are running after compilation succeeds;
4. write the opt-in local `shutdown-request` signal;
5. wait for the foreground Supervisor to close its API and owned Job Objects;
6. copy the staged build over the constant development executable;
7. launch it and restore the previously running services.

Before the first build, the watcher prints its selected `Cargo:` and `Rustc:`
paths. It deliberately prefers the complete toolchain under
`target/rustup-home/toolchains` over a `cargo` or `rustc` rustup shim on
`PATH`. This prevents an incomplete user-level rustup selection from changing
the repository's development compiler.

The signal exists only when the watcher sets
`AKU_SUPERVISOR_DEV_SHUTDOWN_FILE`. AkuSupervisor accepts only an absolute path
whose filename is exactly `shutdown-request`, consumes at most 1 KiB, and uses
the ordinary cleanup path. No development shutdown HTTP endpoint or arbitrary
command execution is exposed.

Press Ctrl+C in the watcher terminal to request the same graceful shutdown. If
cleanup exceeds the configured timeout, the watcher refuses to kill the
process or replace its executable. The checked-in VS Code task
`AkuSupervisor: development watcher` launches the same script.

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

Use `-Config` for a non-default profile, `-Actor codex` when Codex owns the
release action, and `-RequestId` only when an external release system supplies
a guaranteed-fresh ID. Otherwise the script creates a unique bounded ID.

Before copying, the development executable runs `bridge validate`. Its JSON
contract contains `schemaVersion`, `status`, `exitCode`, the structured actor,
request ID, terminal operation, and five named checks. The six required audit
stages are `requested`, `relay_created`, `delivered`, `accepted`,
`heartbeat_observed`, and `completed`. A reused request ID, mismatched build,
missing audit stage, active operation, malformed JSON, or nonzero exit leaves
the stable executable byte-for-byte unchanged.

The validator and audit evaluator are platform-neutral. The PowerShell copy is
the Windows release adapter; Linux and macOS can enforce the same JSON and exit
contract in their native promotion scripts.
