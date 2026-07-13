# AkuSupervisor

AkuSupervisor is a generic, configuration-driven supervisor for local development services on Windows.

The Rust foundation and Roadmap Phase 1 domain contracts are complete. The current binary still exposes only help and version information; process lifecycle execution begins in Roadmap Phase 2.

Rust is the implementation language, targeting `x86_64-pc-windows-msvc` for the initial AkuWorkspace pilot.

## Structure

```text
AkuSupervisor/
  docs/
    generic-local-development-supervisor-spec.md
    implementation-roadmap.md
    mcp-integration-notes.md
  schemas/
    service-config.schema.json
  src/
    adapters/
    application/
    domain/
    platform/
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

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Deferred MCP integration notes](docs/mcp-integration-notes.md)
