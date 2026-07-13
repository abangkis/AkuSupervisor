# AkuSupervisor

AkuSupervisor is a planned generic, configuration-driven supervisor for local development services on Windows.

This repository currently contains the initial project scaffold and planning documents. Implementation has not started yet.

Rust has been selected for the implementation pilot. The existing Node placeholder files are temporary and will be replaced during Roadmap Phase 0 after the Rust MSVC toolchain is ready.

## Planned structure

```text
AkuSupervisor/
  docs/
    generic-local-development-supervisor-spec.md
    implementation-roadmap.md
    mcp-integration-notes.md
  schemas/
    service-config.schema.json
  src/
    cli.mjs
    config.mjs
    control-server.mjs
    health.mjs
    journal.mjs
    process-tree.mjs
    supervisor.mjs
  test/
  package.json
```

## Project documents

- [Product specification](docs/generic-local-development-supervisor-spec.md)
- [Implementation roadmap](docs/implementation-roadmap.md)
- [Deferred MCP integration notes](docs/mcp-integration-notes.md)
