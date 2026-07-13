# AkuSupervisor

AkuSupervisor is a generic, configuration-driven supervisor for local development services.

Roadmap Phase 2 and its Windows process-ownership safety gate are complete.
Phase 3 now has a visible foreground CLI checkpoint with validated
configuration, status, start, stop, restart, operator holds, and exit cleanup.
It also exposes a loopback HTTP control checkpoint with bearer-authenticated
mutations and a bounded client CLI for control from another terminal or Codex.
Persistent journal/events, bounded logs, and request idempotency remain.

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
cargo run -- restart akusidecar --actor codex --reason "source changed"
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Run the complete verification suite through the convenience script:

```powershell
.\scripts\test-phase2.ps1
```

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
cargo run -- stop akusidecar --reason "manual development stop"
```

`user` is the default client actor. Codex must pass `--actor codex`. Every
mutation requires an explicit reason and can select only a service already
registered in configuration; executable paths and arguments are never accepted
by the control protocol. A user stop creates a hold that blocks a later Codex
start or restart until a user explicitly starts or restarts the service.

Without `--config`, AkuSupervisor resolves configuration in this order:

1. `AKU_SUPERVISOR_CONFIG` environment variable;
2. `%LOCALAPPDATA%\AkuSupervisor\services.json` on Windows.

If the selected file does not exist, startup fails clearly and does not create
an empty configuration. On successful startup, the terminal prints the
absolute configuration path and whether it came from `--config`,
`AKU_SUPERVISOR_CONFIG`, or the default user location.

Startup also prints the loopback API address and token-file path. A 256-bit
token is generated on first startup using Windows CNG. The token value is never
printed and must not be pasted into commands; the client reads it from the
configured runtime file.

The checked-in AkuWorkspace pilot profile is
[`config/akuworkspace.services.json`](config/akuworkspace.services.json). It
registers AkuSidecar without overriding its persisted dashboard/SQLite
configuration. Copy it to the default user location for argument-free startup:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\AkuSupervisor"
Copy-Item .\config\akuworkspace.services.json "$env:LOCALAPPDATA\AkuSupervisor\services.json"
```

The profile's HTTP JSON health contract is validated as configuration, but the
current Phase 3 foreground checkpoint does not yet evaluate health at runtime.

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Testing guide](docs/testing-guide.md)
- [Platform portability boundary](docs/platform-portability.md)
- [Deferred MCP integration notes](docs/mcp-integration-notes.md)
