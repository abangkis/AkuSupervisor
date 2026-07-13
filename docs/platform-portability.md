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

## Adapter mapping

| Capability | Windows now | Linux candidate | macOS candidate |
|---|---|---|---|
| Ownership boundary | Job Object | process group plus proven descendant strategy; evaluate `pidfd`, subreaper, or scoped cgroup | process group plus explicit descendant observation |
| Graceful stop | targeted Ctrl+Break | `SIGTERM` to owned process group | `SIGTERM` to owned process group |
| Forced stop | `TerminateJobObject` | bounded `SIGKILL` through the chosen ownership boundary | bounded `SIGKILL` to verified owned group |
| Exit observation | process handle / Job query | `pidfd` or wait primitives | `kqueue` / wait primitives |
| Port diagnostics | IP Helper TCP tables | netlink or `/proc` adapter | `sysctl` or `libproc` adapter |
| Supervisor shutdown | console handler | POSIX signal adapter | POSIX signal adapter |

The Linux and macOS entries are design candidates, not implemented promises.
Each backend must independently pass the same ownership safety contract before
it is considered supported.

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
