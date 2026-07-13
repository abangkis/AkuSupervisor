# AkuSupervisor

AkuSupervisor is a planned generic, configuration-driven supervisor for local development services on Windows.

This repository currently contains only the initial project scaffold and the product specification. Implementation has not started yet.

## Planned structure

```text
AkuSupervisor/
  docs/
    generic-local-development-supervisor-spec.md
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

See [the supervisor specification](docs/generic-local-development-supervisor-spec.md) for the goals, constraints, acceptance criteria, and delivery plan.
