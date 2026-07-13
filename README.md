# AkuSupervisor

AkuSupervisor is a generic, configuration-driven supervisor for local development services.

Roadmap Phase 2 and its Windows process-ownership safety gate are complete.
The current binary still exposes only help and version information; the visible
lifecycle CLI begins in Phase 3.

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
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Run the complete Phase 2 check through the convenience script:

```powershell
.\scripts\test-phase2.ps1
```

For a visible process-tree and Ctrl+C cleanup demo, follow the
[testing guide](docs/testing-guide.md).

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Testing guide](docs/testing-guide.md)
- [Platform portability boundary](docs/platform-portability.md)
- [Deferred MCP integration notes](docs/mcp-integration-notes.md)
