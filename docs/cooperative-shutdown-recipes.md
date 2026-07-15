# Cooperative shutdown recipes

AkuSupervisor can own and clean up any directly launched executable without an
application SDK. These recipes are optional application-side integrations for
teams that can modify their code and want `forced: false`, request draining,
and application-specific cleanup before the owned-tree fallback runs.

Language support and recipe validation are separate:

- **Supervisor support** means AkuSupervisor can launch and own the executable;
  it is language-independent.
- **Recipe validation** means a language-specific signal/shutdown pattern has
  passed an automated application test and a live AkuSupervisor stop that
  reported `gracefulSignalSent: true`, `forced: false`, and
  `ownedPidsAfter: []`.

## Validation matrix

| Language/runtime | Direct executable supervision | Maintained cooperative recipe | Validation status |
|---|---|---|---|
| Go | Supported by the generic command contract | Yes, below | Windows live-validated; Linux amd64 and macOS arm64 compile-checked |
| Node.js | Supported, including registered `.cmd` launchers | Planned | Geofu npm/Rollup service is Windows live-validated with `forced: false`; automated shutdown and Linux/macOS recipe evidence remain |
| Rust | Supported by the generic command contract | Planned | AkuSupervisor itself uses Rust, but a reusable managed-application recipe is not yet certified |
| Kotlin/JVM | Supported when launched as an owned Java process | Planned | Signal/JVM shutdown-hook behavior still requires native live validation |
| Other executables | Supported when the process remains inside the native ownership boundary | Not yet maintained | Use the immutable-program contract and expect forced fallback when the target does not cooperate |

Do not interpret a planned recipe as unsupported supervision. It means only
that the language-specific path to `forced: false` has not yet passed the full
cross-platform evidence gate.

## Go `net/http` recipe

This is the reusable shape validated with Geofu BE. On Windows, Go maps console
Ctrl+Break to `syscall.SIGTERM`; on Linux and macOS the same registration covers
ordinary SIGTERM, while `os.Interrupt` covers the interactive interrupt path.

```go
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusOK)
	})
	server := &http.Server{
		Addr:              "127.0.0.1:8090",
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	listener, err := net.Listen("tcp", server.Addr)
	if err != nil {
		log.Fatal(err)
	}

	if err := serveUntilSignal(server, listener, 5*time.Second); err != nil {
		log.Fatal(err)
	}
}

func serveUntilSignal(
	server *http.Server,
	listener net.Listener,
	shutdownTimeout time.Duration,
) error {
	shutdownSignals := make(chan os.Signal, 1)
	signal.Notify(shutdownSignals, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(shutdownSignals)

	serverErrors := make(chan error, 1)
	go func() {
		serverErrors <- server.Serve(listener)
	}()

	select {
	case err := <-serverErrors:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	case received := <-shutdownSignals:
		log.Printf("shutdown signal received: %s", received)
	}

	shutdownContext, cancel := context.WithTimeout(
		context.Background(),
		shutdownTimeout,
	)
	defer cancel()
	if err := server.Shutdown(shutdownContext); err != nil {
		return fmt.Errorf("graceful HTTP shutdown failed: %w", err)
	}

	if err := <-serverErrors; err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	log.Print("server stopped gracefully")
	return nil
}
```

Replace the sample `ServeMux` with the program's existing HTTP handler. The
shutdown timeout should be no greater than the service profile's
`shutdownGraceMs`; leave a small margin for process exit and log flushing.

### Build and profile

Build the server before supervising it:

```powershell
go build -o .\output\my-server.exe .\cmd\my-server
```

Point `command` at that executable instead of `go.exe run`. A toolchain wrapper
may remain owned and therefore safely removable, but it can prevent the actual
server from receiving the targeted console signal.

```json
{
  "label": "My Go server",
  "cwd": "C:\\Workspace\\MyServer",
  "command": "C:\\Workspace\\MyServer\\output\\my-server.exe",
  "args": ["--host", "127.0.0.1", "--port", "8090"],
  "environment": {},
  "health": {
    "type": "http-status",
    "url": "http://127.0.0.1:8090/health",
    "expectedStatus": 200,
    "timeoutMs": 3000,
    "startupDeadlineMs": 20000
  },
  "ports": [8090],
  "restartPolicy": "manual",
  "shutdownGraceMs": 6000
}
```

The application timeout in this example should be at most five seconds, while
the Supervisor grace is six seconds.

### Minimum automated application test

The application test should prove that the handler is reachable, inject a
signal through a controlled channel or platform fixture, wait for the serve
function to return, and confirm that the listener no longer accepts requests.
The checked-in pilot reference currently lives in the sibling checkout at
`C:\WorkspaceCodex\GeofuWorkspace\Geofu_be\cmd\geofu-server\main_test.go`.
Its production implementation is the adjacent `main.go`.

## Node.js current evidence

The sibling Geofu plugin server at
`C:\WorkspaceCodex\GeofuWorkspace\Geofu\scripts\serve-geofu-plugin.mjs` handles
`SIGINT` and `SIGTERM`, closes its HTTP server, and closes its Rollup watcher.
AkuSupervisor owns it through `npm.cmd run dev`; a Windows live stop reported
`gracefulSignalSent: true`, `forced: false`, and an empty four-process owned
tree. See the [Geofu plugin portability proof](geofu-plugin-portability.md).

This remains validation evidence rather than a maintained recipe because the
application repository does not yet have a deterministic test that injects its
shutdown path and proves both the listener and watcher close. Linux and macOS
also need native live evidence before the matrix can claim cross-platform
recipe support. No source change was required for the Windows supervision
proof.

## Live acceptance gate for every recipe

A recipe is promoted from planned to maintained only after all of these pass:

1. The managed application has a deterministic automated shutdown test.
2. AkuSupervisor launches a direct executable under its native ownership
   adapter.
3. The configured readiness contract becomes healthy.
4. A stop request returns `shutdown.gracefulSignalSent: true`.
5. The same response returns `shutdown.forced: false` and an empty
   `ownedPidsAfter`.
6. The lifecycle journal contains the identical shutdown evidence.
7. Declared ports are released and no unrelated process is affected.
8. Every claimed operating system has native signal and process-tree evidence;
   compilation alone is recorded only as compile coverage.

## Maintenance policy

Keep one section per language/runtime. Each section must document:

- the exact native signal AkuSupervisor sends on every claimed OS;
- whether the recipe requires a direct executable or supports a launcher;
- the application shutdown deadline relative to `shutdownGraceMs`;
- an automated test pattern;
- the last live-validated OS/runtime versions and evidence; and
- known limitations such as JVM hooks, worker processes, child-process groups,
  or toolchain wrappers.

Update the validation matrix only when the full live acceptance gate passes.
Do not claim language support based solely on a code snippet.
