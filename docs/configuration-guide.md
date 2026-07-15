# AkuSupervisor configuration guide

AkuSupervisor does not require a managed program to import an SDK, implement
an AkuSupervisor protocol, or change its source code. The minimum contract is a
directly launchable command that AkuSupervisor can retain inside its native
process-ownership boundary.

Application cooperation improves shutdown quality but is optional. When the
program handles the platform termination signal, AkuSupervisor records a
graceful stop. When it ignores the signal or cannot receive it through a
launcher wrapper, AkuSupervisor waits `shutdownGraceMs` and terminates only its
owned process tree. The latter is bounded cleanup, but applications with
in-memory or transactional state should assess their own forced-exit risk.
Teams that can modify the target should use the maintained
[cooperative shutdown recipes](cooperative-shutdown-recipes.md).

## Complete immutable-program example

The copyable template is
[`config/examples/immutable-windows.services.json`](../config/examples/immutable-windows.services.json):

```json
{
  "version": 1,
  "control": {
    "host": "127.0.0.1",
    "port": 47900,
    "tokenFile": ".runtime/immutable-example/control-token",
    "mcp": {
      "enabled": false,
      "allowedOrigins": []
    }
  },
  "services": {
    "legacy-api": {
      "label": "Immutable Legacy API",
      "cwd": "C:\\Tools\\LegacyApi",
      "command": "C:\\Tools\\LegacyApi\\legacy-api.exe",
      "args": ["--port", "8090"],
      "environment": {},
      "health": { "type": "process" },
      "ports": [8090],
      "restartPolicy": "manual",
      "shutdownGraceMs": 5000
    }
  }
}
```

Replace the example paths, arguments, port, and service ID. Both `cwd` and
`command` must exist when AkuSupervisor validates the selected configuration.
The command and every argument remain separate JSON values; AkuSupervisor does
not accept an arbitrary shell command from a lifecycle request.

## Control fields

| Field | Requirement |
|---|---|
| `version` | Must be `1` for the current contract. |
| `control.host` | Must be the explicit loopback address `127.0.0.1`. |
| `control.port` | Nonzero and unique among concurrently running Supervisor instances. |
| `control.tokenFile` | Relative path below `.runtime`; the platform adapter creates and protects the token. |
| `control.mcp.enabled` | Optional read-only MCP endpoint on the same authenticated listener. |
| `control.mcp.allowedOrigins` | Exact trusted browser origins; native stdio MCP normally leaves this empty. |

`cooperativeActions` is not required for ordinary programs. It currently
contains only the AkuWorkspace-specific AkuBridge reload adapter and should be
omitted from generic profiles.

## Service fields

| Field | Requirement |
|---|---|
| `label` | Human-readable service name. |
| `cwd` | Absolute, existing working directory. |
| `command` | Absolute, existing executable or supported Windows command wrapper such as `npm.cmd`. |
| `args` | Fixed argument array; it is never replaced by API input. |
| `environment` | Fixed string-to-string overrides. Prefer an empty object when the program already has persisted configuration. |
| `health` | One of the bounded health contracts below. |
| `ports` | Ports expected to be free before start. An external owner is reported and never killed. |
| `restartPolicy` | `manual` or the bounded `on-failure` policy. |
| `shutdownGraceMs` | Time allowed after the native graceful signal before owned-tree forced cleanup. |

## Health choices

Use process health when the immutable program has no readiness endpoint:

```json
{ "type": "process" }
```

Use HTTP status when an existing endpoint is available without source changes:

```json
{
  "type": "http-status",
  "url": "http://127.0.0.1:8090/health",
  "expectedStatus": 200,
  "timeoutMs": 3000,
  "startupDeadlineMs": 20000
}
```

Use shallow HTTP JSON matching for a stronger existing contract:

```json
{
  "type": "http-json",
  "url": "http://127.0.0.1:8090/health",
  "timeoutMs": 3000,
  "startupDeadlineMs": 20000,
  "expect": {
    "status": "ok",
    "schemaVersion": 1
  }
}
```

`startupDeadlineMs` is also the lifecycle budget used by the CLI while waiting
for start/restart. A slow but valid startup therefore does not inherit the
ordinary five-second control-plane response timeout.

## Shutdown compatibility

| Target behavior | Required source change | AkuSupervisor result |
|---|---|---|
| Handles the native signal and exits within grace | No, when already supported | `gracefulSignalSent: true`, `forced: false` |
| Ignores or does not implement the signal | No | Wait for grace, then terminate only the owned tree; `forced: true` |
| Runs through a build/toolchain wrapper such as `go run` | No, but signal delivery may stop at the wrapper | Ownership still bounds cleanup; prefer a built executable for reliable graceful behavior |
| Starts an external OS service or detached daemon that is not retained as a child | Usually needs a different adapter | Do not claim ownership through a mere start command; use a future native service adapter |

The canonical
[`config/akuworkspace.services.json`](../config/akuworkspace.services.json)
contains both relevant real cases behind one control boundary: Geofu BE is a
cooperative direct executable, while AkuSidecar uses a registered Windows
command wrapper and HTTP JSON health.

## Run and inspect

```powershell
.\target\aku-supervisor.exe --config C:\path\to\services.json
```

From a second terminal:

```powershell
.\target\aku-supervisor.exe start legacy-api `
  --actor user `
  --reason "local development" `
  --config C:\path\to\services.json

.\target\aku-supervisor.exe stop legacy-api `
  --actor user `
  --reason "development complete" `
  --json `
  --config C:\path\to\services.json

.\target\aku-supervisor.exe events --limit 20 `
  --json `
  --config C:\path\to\services.json
```

For a stop or restart, inspect `response.shutdown` and the matching journal
record. `ownedPidsAfter` must be empty. `forced` tells whether the target
cooperated; it does not change AkuSupervisor's ownership guarantee.

Profiles containing absolute paths are operating-system and machine specific.
Use separate Windows, Linux, and macOS profile files even after the native
platform adapters are implemented.
