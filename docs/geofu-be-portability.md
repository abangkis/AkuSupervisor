# Geofu BE portability proof

Status: **Windows portability proof passed**

Date: 2026-07-15

## Scope

This proof registers exactly one independently runnable service from a second
project: `Geofu_be/cmd/geofu-server`. It tests whether AkuSupervisor's existing
configuration, process ownership, health, logs, journal, and read-only MCP
contracts work without adding Geofu-specific behavior to the lifecycle core.

The proof does not add GeoLibre, the Geofu frontend, pipeline execution,
dependency graphs, deployment orchestration, MCP mutations, or another
operating-system adapter.

Changing Geofu BE was not a prerequisite for AkuSupervisor ownership. The
initial unmodified `go run` trial was started, health-checked, and completely
removed through the bounded Job Object fallback. Geofu's signal handler was
added only to validate the stronger cooperative-shutdown path and avoid a
forced exit for a server that can safely drain HTTP work.

## Profile

The service is part of the local AkuWorkspace operational profile at
`%LOCALAPPDATA%\AkuSupervisor\services.json`. The earlier isolated proof profile
was removed after validation. No repository copy is maintained: registration
MCP and the local profile are the single operational source of truth.

- Supervisor control API: `127.0.0.1:11121`
- Service ID: `geofu-be`
- Working directory: `C:\WorkspaceCodex\GeofuWorkspace\Geofu_be`
- Command: `Geofu_be\output\geofu-server.exe`
- Service port: `8765`
- Health contract: `catalog.json` has top-level `schemaVersion: 1`
- Restart policy: `manual`
- Shutdown grace: 5 seconds

The profile launches a built executable instead of `go run`: a stable
process-group root receives the platform shutdown signal directly and avoids
coupling lifecycle behavior to a toolchain wrapper.

## Artifact precondition

Geofu BE validates `output/package_server` before listening. The profile never
builds or refreshes pipeline artifacts. Before live testing, run from Geofu_be:

```powershell
go run ./cmd/geofu-validate
go build -o .\output\geofu-server.exe .\cmd\geofu-server
```

Missing or invalid package artifacts must fail startup without weakening
Supervisor health or ownership rules. A missing server executable is a profile
precondition failure and must not be replaced with an implicit build step.

## Live gate

The current GoLand-owned server must first be stopped through GoLand so port
8765 is free. Do not kill it by PID and do not let AkuSupervisor claim it.

Start the Supervisor with the default local profile:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
.\target\aku-supervisor.exe
```

From a second terminal:

```powershell
.\target\aku-supervisor.exe start geofu-be `
  --actor user `
  --reason "Geofu BE portability proof"

.\target\aku-supervisor.exe status --json

.\target\aku-supervisor.exe logs geofu-be --stream stderr --tail 100
```

Acceptance evidence:

- start reaches `running / healthy` within 30 seconds;
- `catalog.json` is served and matches the shallow health contract;
- the owned PID set contains only the built server and its real descendants;
- stdout/stderr remain bounded and readable;
- MCP lists and inspects `geofu-be` without mutation authority;
- stop removes every owned PID and leaves no listener on port 8765;
- the stop report shows graceful completion without forced Job termination;
- an unrelated process is never stopped; and
- both repositories remain clean.

If the stop report is forced, the next change belongs in Geofu BE: add explicit
Windows signal handling and `http.Server.Shutdown`. Do not weaken
AkuSupervisor's bounded Job Object fallback.

## Known adapter boundary

The lifecycle core, read-only MCP surface, and stable-promotion script are
generic. AkuSidecar/AkuBridge validation lives only in the explicitly optional
`validate-akuworkspace-integration.ps1` adapter and is not part of this Geofu
proof.

## Validation evidence

The Windows live gate passed on 2026-07-15:

- the first final-profile start took 27.794 seconds and still returned success,
  proving the CLI response timeout follows the configured lifecycle budget
  instead of the normal five-second control-plane timeout;
- `catalog.json` returned `schemaVersion: 1` and the service snapshot was
  `running / healthy`;
- logs were readable through the bounded control endpoint;
- MCP initialize, `tools/list`, and the read-only
  `supervisor_list_services` call all succeeded;
- the final stop completed in 45 milliseconds, the server log recorded the
  shutdown signal and `stopped gracefully`, and port 8765 was released; and
- the stop API and lifecycle journal both recorded
  `gracefulSignalSent: true`, `forced: false`, the owned PID before stop, and an
  empty owned PID set afterward.

An earlier `go run` profile deliberately exposed an important boundary: its
toolchain wrapper prevented reliable cooperative Ctrl+Break delivery and made
AkuSupervisor use the five-second forced Job Object fallback. The final profile
therefore launches the built Geofu executable. AkuSupervisor remains
responsible for ownership and fallback; Geofu remains responsible for graceful
HTTP shutdown.

The service contract is generic, but the current local profile contains Windows
paths. Linux and macOS need native local profiles plus the Phase 9 platform
adapters; no support is claimed for those operating systems yet.
