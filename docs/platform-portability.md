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
- `ShutdownSignal`: exposes a read-only shutdown request to the lifecycle loop.

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
| Supervisor shutdown | console handler | POSIX signal adapter | POSIX signal adapter |
| Secure token entropy | CNG `BCryptGenRandom` | `getrandom(2)` or equivalent OS API | `SecRandomCopyBytes` or equivalent OS API |
| Token-file permissions | restricted current-user DACL (hardening pending) | mode `0600` (future) | mode `0600` (future) |
| HTTP control/client | shared `std::net` adapter | same shared adapter | same shared adapter |

The Linux and macOS entries are design candidates, not implemented promises.
Each backend must independently pass the same ownership safety contract before
it is considered supported.

## Current portability assessment

| Area | Assessment | Required before Linux/macOS support |
|---|---|---|
| Domain and lifecycle application | Portable and architecture-tested | Keep OS imports forbidden |
| HTTP control server/client | Portable `std::net` implementation | Run the same protocol tests on native CI |
| Token format, persistence, comparison | Portable common implementation | Add native permission-hardening contract |
| Secure entropy | Correctly isolated in Windows CNG adapter | Add independently tested Linux and macOS providers |
| Configuration discovery | Per-OS default paths already separated | Add native path tests and native service profiles |
| Process ownership and port observation | Correctly isolated, Windows only | Implement and prove each native ownership backend |
| Foreground host composition | Partially portable; shared interaction code is currently compiled with concrete `WindowsRegistry` and `ConsoleShutdown` types | Extract a generic foreground host driven by platform-neutral registry, shutdown, and runtime-secret ports before implementing the second OS |
| Checked-in AkuWorkspace profile | Intentionally Windows-specific data, not core logic | Supply separate profiles; do not add path rewriting to the core |

The foreground-host extraction is deliberately scheduled before the second OS,
not after duplicating the Windows loop. Linux or macOS support must not copy the
interactive/API lifecycle logic into a parallel platform file.

## Source layout

```text
src/
  domain/                  # lifecycle and authority rules; no OS imports
  application/
    platform_ports.rs      # platform-neutral interfaces and DTOs
    service_runtime.rs     # serialized lifecycle owner
  adapters/                # config, journal, future CLI and HTTP
  platform/
    windows/               # implemented Win32 adapter
    linux/                 # reserved Linux adapter boundary
    macos/                 # reserved macOS adapter boundary
```

`windows-sys` is declared under Cargo's `cfg(windows)` dependency section, so
it is not selected for Linux or macOS builds.
