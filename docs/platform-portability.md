# Platform Portability Boundary

Status: **Windows implemented; Linux and macOS adapters reserved**

## Layer rule

AkuSupervisor uses an inward-facing dependency direction:

```text
CLI / HTTP / configuration adapters
              |
              v
     application + domain
              |
              v
  platform-neutral Rust traits
              |
       +------+------+
       |             |
   Windows       Linux / macOS
 implemented       future
```

The domain and application layers may use only platform-neutral contracts from
`src/application/platform_ports.rs`. They must not import `windows-sys`,
`std::os::windows`, or `platform::windows`.

The current contracts are:

- `LaunchSpec`: executable, arguments, working directory, and environment kept
  as separate values;
- `ProcessTreeSpawner`: creates one authoritative owned process tree;
- `ManagedProcessTree`: observes and stops only that owned tree;
- `PortInspector`: produces diagnostics without termination authority; and
- `HealthCheckSpec` and `HealthProbe`: keep lifecycle health evaluation
  independent of process ownership and operating-system APIs; and
- `ShutdownSignal`: exposes a read-only shutdown request to the lifecycle loop.
- `ServiceLogSink`: receives already-persisted stdout/stderr bytes through a
  bounded platform-neutral contract; and
- `LiveLogHub`: assembles lines, keeps a bounded replay ring, and fans out to
  bounded subscribers without importing native process APIs;
- `RuntimeInstanceLease`: uses the standard-library cross-platform file-lock
  contract for single-instance ownership and persisted diagnostics; and
- `SupervisorShutdown`: carries one authenticated, idempotent shutdown intent
  into the shared foreground cleanup loop.

An architecture test enforces the import boundary.

The authenticated control plane follows the same rule. Its HTTP parser/client,
token format, token-file discovery, actor mapping, and `SupervisorControl`
interface use only the Rust standard library and platform-neutral types. The
client commands therefore do not require a Windows process adapter.

## Runtime-secret boundary

Token generation is deliberately split from token storage and comparison:

```text
RuntimeToken (shared adapter)
  - 256-bit lowercase hexadecimal contract
  - create-new persistence and validation
  - redacted Debug output
  - constant-time bearer comparison
             ^
             | injected secure generator
             |
  +----------+-----------+----------------+
  |                      |                |
Windows CNG          Linux native     macOS native
implemented          future           future
BCryptGenRandom      getrandom(2)     SecRandomCopyBytes
```

`BCryptGenRandom` exists only in `src/platform/windows/secure_random.rs` and is
selected through the Windows composition root. Neither the HTTP adapter nor
`RuntimeToken` imports CNG or `windows-sys`. Linux and macOS must provide native
entropy adapters with the same 32-byte contract; they must not emulate entropy
using timestamps, process IDs, or a language PRNG.

One security item remains platform-specific and must be completed before a
non-Windows backend is accepted: token-file access control. The common layer
uses atomic `create_new` behavior, while each native adapter must harden the
result for the current user (a restricted DACL on Windows and mode `0600` on
Linux/macOS) and test that contract. Inherited directory permissions alone are
not considered a portable guarantee.

## Adapter mapping

| Capability | Windows now | Linux candidate | macOS candidate |
|---|---|---|---|
| Ownership boundary | Job Object | process group plus proven descendant strategy; evaluate `pidfd`, subreaper, or scoped cgroup | process group plus explicit descendant observation |
| Graceful stop | targeted Ctrl+Break | `SIGTERM` to owned process group | `SIGTERM` to owned process group |
| Forced stop | `TerminateJobObject` | bounded `SIGKILL` through the chosen ownership boundary | bounded `SIGKILL` to verified owned group |
| Exit observation | process handle / Job query | `pidfd` or wait primitives | `kqueue` / wait primitives |
| Port diagnostics | IP Helper TCP tables | netlink or `/proc` adapter | `sysctl` or `libproc` adapter |
| Supervisor shutdown | console handler plus shared authenticated control request | POSIX signal adapter plus the same control request | POSIX signal adapter plus the same control request |
| Per-service shutdown evidence | `TreeStopReport` from Job Object adapter | Same shared report from process-group/cgroup adapter | Same shared report from process-group adapter |
| Secure token entropy | CNG `BCryptGenRandom` | `getrandom(2)` or equivalent OS API | `SecRandomCopyBytes` or equivalent OS API |
| Token-file permissions | protected current-user-only DACL | mode `0600` (future) | mode `0600` (future) |
| HTTP control/client | shared `std::net` adapter | same shared adapter | same shared adapter |
| Loopback HTTP health | shared `std::net` adapter | same shared adapter | same shared adapter |
| Development reload request | bounded local file signal plus PowerShell runner | same file-signal contract plus future native runner | same file-signal contract plus future native runner |
| Runtime instance lease | shared `std::fs::File` lock plus PowerShell watcher lease | same Rust lock plus native watcher lease holder | same Rust lock plus native watcher lease holder |
| Live service logs | Windows pipe pump persists then publishes to shared `LiveLogHub` | Unix pipe pump uses the same `ServiceLogSink` | Unix pipe pump uses the same `ServiceLogSink` |

The Linux and macOS entries are design candidates, not implemented promises.
Each backend must independently pass the same ownership safety contract before
it is considered supported.

## Current portability assessment

| Area | Assessment | Required before Linux/macOS support |
|---|---|---|
| Domain and lifecycle application | Portable and architecture-tested | Keep OS imports forbidden |
| HTTP control server/client | Portable `std::net` implementation | Run the same protocol tests on native CI |
| Runtime health state and matchers | Portable application policy plus shared loopback HTTP adapter | Run startup deadline, failure, and recovery fixtures on native CI |
| Exit reconciliation and restart policy | Portable application policy; Windows supplies Job membership and process exit status | Prove that launcher exit never overrides living owned descendants, then run equivalent crash-loop fixtures |
| Token format, persistence, comparison | Portable common implementation | Add native permission-hardening contract |
| Secure entropy | Correctly isolated in Windows CNG adapter | Add independently tested Linux and macOS providers |
| Configuration discovery | Per-OS default paths already separated | Add native path tests and native service profiles |
| Process ownership and port observation | Correctly isolated, Windows only | Implement and prove each native ownership backend |
| Live service-registry reconciliation | Portable `RwLock` topology swap plus typed configuration adapter; unchanged runtime entries are retained | Run the same PID-preservation and active-target rejection fixtures on native CI |
| Foreground host composition | Compile-time host composition is extracted; the shared foreground imports only `platform::host`, whose active Windows backend supplies registry, shutdown, secure entropy, and token permissions | Implement the same host-facing constructors and aliases for a second OS without copying the foreground loop |
| Checked-in AkuWorkspace profile | Intentionally Windows-specific data, not core logic | Supply separate profiles; do not add path rewriting to the core |
| Development watcher | Portable Rust file-signal adapter; Windows PowerShell orchestration | Add a native runner that preserves build-first, graceful handoff, and service restoration |
| Runtime identity and remote shutdown | Portable standard-library lease, JSON identity, authenticated HTTP route, and shared cleanup intent | Run lock interoperability and shutdown fixtures natively | Run lock interoperability and shutdown fixtures natively |
| Live-log hub and NDJSON protocol | Portable standard-library implementation; only pipe capture is native | Connect the native process spawner to the existing `ServiceLogSink`; do not fork the protocol |

The foreground-host extraction now precedes the second OS implementation.
Linux or macOS support must populate the compile-time `platform::host`
composition and must not copy the interactive/API lifecycle logic into a
parallel platform file.

The development watcher follows the same boundary. The opt-in
`development_shutdown` adapter uses portable `std::fs` APIs and accepts only a
bounded, fixed-name request file. `scripts/dev.ps1` is a Windows host adapter,
not application logic. A Linux or macOS runner may use `watchexec`, shell
notifications, or a native watcher, but must retain these invariants: compile
before stopping the active binary, keep it alive on build failure, request its
normal cleanup path, preserve a constant executable path, and restore only the
services that were observed running immediately before handoff.

Configuration registration does not depend on PowerShell file watching. The
foreground Rust adapter periodically reads the atomically replaced typed
configuration and calls the platform-neutral registry reconciliation method.
Linux and macOS may later replace polling with native notifications, but the
semantic boundary must remain identical: service-only changes are live,
unchanged owners are retained, active changed targets fail closed, and
non-service host settings require an explicit restart.

## Source layout

```text
src/
  domain/                  # lifecycle and authority rules; no OS imports
  application/
    platform_ports.rs      # platform-neutral interfaces and DTOs
    service_runtime.rs     # serialized lifecycle owner
  adapters/                # config, journal, future CLI and HTTP
  platform/
    host.rs                # compile-time active host composition
    windows/               # implemented Win32 adapter
    linux/                 # reserved Linux adapter boundary
    macos/                 # reserved macOS adapter boundary
```

`windows-sys` is declared under Cargo's `cfg(windows)` dependency section, so
it is not selected for Linux or macOS builds.
