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

## Structure

```text
AkuSupervisor/
  docs/
    generic-local-development-supervisor-spec.md
    implementation-roadmap.md
    mcp-integration-notes.md
    platform-portability.md
    testing-guide.md
  config/
    akuworkspace.services.json
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
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Run the complete verification suite through the convenience script:

```powershell
.\scripts\test-phase2.ps1
```

For live AkuSupervisor development, stop any normally running instance with
`quit`, then start the safe watcher:

```powershell
.\scripts\dev.ps1
```

The watcher builds into a staging target while the current supervisor remains
available. A successful build requests normal graceful cleanup, replaces only
the constant `target\dev\aku-supervisor.exe`, starts it again, and restores the
services that were running before the restart. A failed build leaves the old
supervisor and its services untouched. It never replaces the normal
`target\aku-supervisor.exe` and never force-kills a timed-out process. Visual
Studio Code users can also run the `AkuSupervisor: development watcher` task.
The watcher owns stdin; use the control CLI from a second terminal for
`status`, `start`, `stop`, or `restart` while it is active.
It prefers the complete project-local Rust toolchain over any rustup shim found
on `PATH`, and prints the selected Cargo and Rust compiler paths before build.
See the [testing guide](docs/testing-guide.md#7-development-watcher).

For a visible process-tree and Ctrl+C cleanup demo, follow the
[testing guide](docs/testing-guide.md).

While `run` is active, use `status`, `start <service> [reason]`,
`stop <service> [reason]`, `restart <service> [reason]`, and `quit` in the same
terminal.

From a second terminal, use the control client while the foreground supervisor
continues running visibly:

```powershell
cargo run -- status
cargo run -- start akusidecar --reason "manual development start"
cargo run -- restart akusidecar --actor codex --reason "backend source changed"
cargo run -- events --after 0 --limit 20
cargo run -- logs akusidecar --stream stderr --tail 100
cargo run -- stop akusidecar --reason "manual development stop"
```

After the Gate 5 build has been loaded into Chrome once, either the user or
Codex can request the only browser-side mutation exposed by AkuSupervisor:

```powershell
.\target\aku-supervisor.exe bridge reload `
  --actor codex `
  --reason "load updated AkuBridge build" `
  --request-id "bridge-reload-20260714-001"
```

The command relays `reload_self` through the open AkuBrowser tab, invokes
`chrome.runtime.reload()`, refreshes only that local tab so the new content
script is injected, and succeeds only after Sidecar observes the expected new
build heartbeat. It does not expose arbitrary extension commands, Chrome
management, CDP, tab closure, or whole-browser restart. See the
[AkuBridge reload design](docs/aku-bridge-reload.md).

`user` is the default client actor. Codex must pass `--actor codex`. Every
mutation requires an explicit reason and can select only a service already
registered in configuration; executable paths and arguments are never accepted
by the control protocol. Use `--request-id <id>` when a caller may retry a
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

The checked-in AkuWorkspace pilot profile is
[`config/akuworkspace.services.json`](config/akuworkspace.services.json). It
registers AkuSidecar without overriding its persisted dashboard/SQLite
configuration. Copy it to the default user location for argument-free startup:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\AkuSupervisor"
Copy-Item .\config\akuworkspace.services.json "$env:LOCALAPPDATA\AkuSupervisor\services.json"
```

The profile's HTTP JSON health contract is validated as configuration. Runtime
health evaluation inside AkuSupervisor remains a later enhancement; Gate 4 used
the declared endpoint plus one real AkuSidecar reasoning invocation.

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Testing guide](docs/testing-guide.md)
- [Platform portability boundary](docs/platform-portability.md)
- [Deferred MCP integration notes](docs/mcp-integration-notes.md)
- [AkuBridge cooperative reload](docs/aku-bridge-reload.md)
