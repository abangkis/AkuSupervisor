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

## 5. Current limitation

The local-control checkpoint does not yet expose persistent event retrieval,
bounded log commands, request idempotency, or runtime health evaluation.
AkuSidecar live validation remains Phase 4 and will include a real reasoning
invocation, not only `/api/health`.
