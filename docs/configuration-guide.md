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
  "observability": {
    "consoleEvents": "lifecycle"
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

## Handoff-aware application example: `instanceEpoch`

AkuSupervisor proves that a newly owned process tree satisfies its configured
health response. An application with additional in-memory clients may expose a
per-process epoch so those clients can distinguish recovery to the same
instance from recovery into a replacement instance.

The current Go AkuSidecar implements this pattern. It creates one random,
non-persisted value at process construction and returns it from health and
bootstrap:

```go
instanceEpoch := domain.NewID("epoch")

// GET /api/health and GET /api/bootstrap include the same process value.
writeJSON(response, http.StatusOK, map[string]any{
    "status":        "ok",
    "instanceEpoch": instanceEpoch,
})
```

After a development watcher handoff, an existing client compares the new
bootstrap epoch with its previous value. A change invalidates only ephemeral
client readiness, not SQLite state or authentication material. The client then
performs its own bounded integration handshake before allowing new work.

`instanceEpoch` is an application example, not a Supervisor configuration
field and not a service dependency. AkuSupervisor must not interpret the value,
wait for AkuBridge, or acquire browser authority. Its generic health contract
continues to match the declared shallow fields, while the application owns its
deeper readiness recovery.

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

## Observability fields

The append-only lifecycle journal is always enabled and cannot be disabled by
configuration. `observability.consoleEvents` controls only the canonical event
summary mirrored to the visible Supervisor console after the corresponding
journal record has been durably written.

| Value | Console behavior |
|---|---|
| `off` | Successful lifecycle events remain journal-only. Failures still appear on stderr. |
| `lifecycle` | Default. One concise line shows sequence, service, action, state transition, actor, and outcome. |
| `verbose` | Adds reason, PID counts, error category, exit code, and graceful/forced shutdown evidence. |

Example default output:

```text
[2026-07-16T10:42:13.527Z] [event #244] geofu-be start: stopped -> running (user/cli, success)
[2026-07-16T10:45:01.104Z] [event #245] geofu-be stop: running -> stopped (user/cli, graceful)
```

The leading timestamp is the persisted event time rendered as RFC 3339 UTC.
The explicit `Z` keeps console evidence comparable across Windows, Linux,
macOS, CI, and machines with different local time zones.

This is Supervisor activity, not managed-service stdout/stderr. Service output
continues to use the files beneath `.runtime/services` and the `logs` command.

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

Use a bounded loopback TCP connection when a service exposes a non-HTTP or TLS
listener that AkuSupervisor should not weaken or terminate at the transport
boundary:

```json
{
  "type": "tcp-connect",
  "host": "127.0.0.1",
  "port": 8090,
  "timeoutMs": 3000,
  "startupDeadlineMs": 20000
}
```

The host must be an explicit loopback IP. This proves listener readiness while
keeping health traffic local; it does not claim an application-level response.

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
contains four services behind one control boundary: Geofu BE is a cooperative
direct executable; AkuSidecar uses a registered Windows command wrapper; the
Geofu plugin uses an npm wrapper with a long-lived Rollup watcher; and GeoLibre
runs its daily LAN HTTPS Vite host. All use bounded health and retained
process-tree ownership.

The canonical profile uses the hardened `geofu:lan` HTTPS workflow on 6060.
The LAN mode also needs the
independently supervised `geofu-plugin` service for complete workflow
readiness. See
[Geofu daily workflows](geofu-daily-workflows.md) for startup order and the
boundary between long-running services and one-shot deployment tasks.

`geofu:lan` owns HTTPS certificate loading, the `0.0.0.0` bind, and same-origin
proxies for the Geofu plugin and catalog. AkuSupervisor keeps only the explicit
6060 port override and uses `tcp-connect` against `127.0.0.1:6060`, proving the
TLS listener is accepting connections without bypassing certificate hardening.
`geofu:locked-dev`, plugin copy, and deployment commands are intentionally not
registered in the default development profile.

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
