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

Runtime-specific fixtures and native compatibility reports belong to the
separate sibling project `AkuSupervisorConformance`. It consumes an explicitly
supplied AkuSupervisor executable and is not a submodule, build dependency, or
`promote-stable.ps1` gate. Normal AkuSupervisor users do not need to clone it or
install the runtimes represented by its fixtures.

## Validation matrix

| Language/runtime | Direct executable supervision | Maintained cooperative recipe | Validation status |
|---|---|---|---|
| Go | Supported by the generic command contract | Yes, below | Windows live-validated; Linux amd64 and macOS arm64 compile-checked |
| Node.js | Supported, including registered `.cmd` launchers | Yes for the current Windows adapter, below | Independent fixture, deterministic test, and Windows native gate pass in AkuSupervisorConformance with application-observed `SIGBREAK`, complete cleanup events, no forced fallback, and an empty owned tree |
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

### AI4U backend applicability

The sibling `C:\WorkspaceCodex\AI4UWorkspace\ai4u_be` server now implements the
Go recipe on its `koc/lite` branch. `cmd/main.go` creates one signal context for
`os.Interrupt` and `syscall.SIGTERM`, passes it to all seven background workers,
the GA scheduler, and the WebSocket hub, and drains `http.Server` through a
bounded `Shutdown`. Runtime cleanup waits for the WebSocket hub, then closes
the shared database pool. Early listener/server failures enter the same cleanup
path.

Deterministic application tests prove that an active HTTP request drains before
the listener closes and that root cancellation closes an active WebSocket
client, clears the hub, and stops its event loop.
Targeted lifecycle packages pass normally; broad `cmd/...` plus `pkg/...`
compilation and tests pass with repository-baseline vet checks disabled.
This is **application-tested** evidence, not yet a live AkuSupervisor claim: a
built AI4U BE executable must still be registered, started healthy, stopped by
targeted Ctrl+Break, and observed with `forced: false`, empty owned PIDs, and
matching application shutdown logs.

## Node.js service classifications

Node-based repositories do not all own a Node server. Classify the launched
command before adding signal handlers:

| Classification | Example | Cooperative code belongs in | AkuSupervisor expectation |
|---|---|---|---|
| Application-owned server | custom `node server.mjs`, Express, Fastify, or a repository-owned HTTP/watcher process | the server entrypoint | use the recipe below and prefer a direct `node.exe` launch |
| Tool-owned development server | Vite, webpack-dev-server, or another CLI launched through npm | the tool or an explicit repository-owned wrapper, not browser React code | supervise the immutable npm/process tree and validate actual native exit behavior |
| One-shot build | `vite build`, bundler, generator, migration | normally nowhere; it is not a long-lived service | use an explicit task/workflow, not a health-supervised service |

This distinction prevents a common false integration: adding
`process.on(...)` to browser-bundled frontend source does not control the Node
process that runs Vite.

## Node.js application-owned server recipe

AkuSupervisor sends targeted Ctrl+Break on Windows. Node delivers that as
`SIGBREAK`; `SIGTERM` is not a native Windows termination signal. Linux and
macOS adapters are expected to send `SIGTERM`, while `SIGINT` covers ordinary
interactive shutdown. A portable application-owned server must therefore
handle all three without running cleanup twice.

The final candidate contract is:

```javascript
import http from "node:http";
import process from "node:process";

const port = Number(process.env.PORT || 8091);
const applicationShutdownMs = 5_000;
let ready = false;
let shutdownPromise;

const server = http.createServer((request, response) => {
  if (request.url === "/health") {
    response.writeHead(ready ? 200 : 503, { "content-type": "application/json" });
    response.end(JSON.stringify({ status: ready ? "ready" : "stopping" }));
    return;
  }
  response.writeHead(200, { "content-type": "text/plain" });
  response.end("ok");
});

// Replace these with repository-owned cleanup operations. Long-lived
// WebSockets, watchers, consumers, child processes, worker threads, and timers
// must be closed here; database and queue clients should also be drained.
async function closeApplicationResources() {}

function closeHttpServer() {
  return new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
}

function shutdown(signal) {
  if (shutdownPromise) return shutdownPromise;
  ready = false;
  console.log(JSON.stringify({ event: "shutdown_started", signal }));

  let deadline;
  const graceful = (async () => {
    // Start refusing new HTTP connections, close resources that can keep the
    // HTTP server open (especially upgraded WebSockets), then await full drain.
    const httpClosed = closeHttpServer();
    await closeApplicationResources();
    await httpClosed;
  })();
  const timedOut = new Promise((_, reject) => {
    deadline = setTimeout(() => {
      // Node 18.2+ escape hatch for stuck HTTP connections. AkuSupervisor still
      // owns the outer deadline and will report forced=true if the tree remains.
      server.closeAllConnections?.();
      reject(new Error("application shutdown deadline exceeded"));
    }, applicationShutdownMs);
  });

  shutdownPromise = Promise.race([graceful, timedOut])
    .then(() => {
      process.exitCode = 0;
      console.log(JSON.stringify({ event: "shutdown_completed", signal }));
    })
    .catch((error) => {
      process.exitCode = 1;
      console.error(JSON.stringify({
        event: "shutdown_failed",
        signal,
        message: error.message,
      }));
      throw error;
    })
    .finally(() => clearTimeout(deadline));

  return shutdownPromise;
}

function handleSignal(signal) {
  void shutdown(signal).catch(() => {
    // Keep the process alive only for resources that have not stopped. The
    // Supervisor's bounded owned-tree fallback remains authoritative.
  });
}

for (const signal of ["SIGBREAK", "SIGINT", "SIGTERM"]) {
  process.once(signal, handleSignal);
}

server.listen(port, "127.0.0.1", () => {
  ready = true;
  console.log(JSON.stringify({ event: "server_ready", port }));
});
```

Do not call `process.exit(0)` immediately from the signal handler: it can cut
off request draining, buffered logs, and asynchronous resource cleanup. Once
all handles close, Node exits naturally with the assigned `exitCode`. The
application deadline must be shorter than `shutdownGraceMs`, leaving time for
final logs and natural process exit.

For reliable Windows cooperative delivery, prefer launching the application
entrypoint directly:

```json
{
  "label": "My Node server",
  "cwd": "C:\\Workspace\\MyNodeServer",
  "command": "C:\\Program Files\\nodejs\\node.exe",
  "args": ["server.mjs"],
  "environment": { "PORT": "8091" },
  "health": {
    "type": "http-status",
    "url": "http://127.0.0.1:8091/health",
    "expectedStatus": 200,
    "timeoutMs": 3000,
    "startupDeadlineMs": 20000
  },
  "ports": [8091],
  "restartPolicy": "manual",
  "shutdownGraceMs": 6000
}
```

An npm or `.cmd` launcher remains safely owned, but its presence makes
application-level signal delivery tool-dependent. Treat `forced: false` only
as proof that the owned tree exited before AkuSupervisor's forced fallback; it
does not by itself prove the application's cleanup handler ran. Certification
also requires deterministic `shutdown_started` and `shutdown_completed`
evidence plus resource-specific assertions.

### Minimum automated Node application test

Export the application factory and `shutdown` function so a test can invoke
the exact shutdown path without synthesizing an OS signal. The test must:

1. listen on an ephemeral loopback port and reach `/health`;
2. keep one request or long-lived resource active;
3. invoke `shutdown("TEST")` twice and prove one shared promise/cleanup run;
4. prove new connections are rejected and the active request is drained;
5. prove watchers, WebSockets, workers, children, timers, and clients registered
   by the fixture are closed; and
6. finish with no listener or live descendant.

That deterministic test validates application behavior. A separate native
AkuSupervisor integration gate must still deliver Ctrl+Break on Windows and
SIGTERM on every later POSIX adapter.

The dependency-free reference implementation now lives in the separate
`AkuSupervisorConformance/fixtures/node-application-owned` project. Its
deterministic test passes without `npm install`. The Windows runner accepts an
explicit AkuSupervisor binary path, creates an isolated configuration, and
produces a versioned JSON report without touching the user's normal profile.

## Current Node.js evidence and AI4U frontend applicability

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
proof. Because the Windows adapter sends Ctrl+Break and the application does
not currently record `SIGBREAK` cleanup completion, `forced: false` is retained
as owned-tree process-exit evidence rather than proof that its custom cleanup
handler executed.

The sibling `C:\WorkspaceCodex\AI4UWorkspace\ai4u_fe` repository is a React SPA
whose `npm run dev` command currently launches installed Vite 7.3.6. It
contains no application-owned Node server. Its installed Vite implementation
has a `server.close()` path that closes the file watcher, WebSocket server,
environments, and HTTP server, and it registers a `SIGTERM` listener. The React
source cannot add the Windows `SIGBREAK` handler that AkuSupervisor would need
to prove that exact cleanup path.

Therefore AI4U FE is classified as a **tool-owned development server**:

- supervise `npm.cmd run dev` as an immutable owned process tree;
- use HTTP/TCP readiness on Vite port 5173;
- accept forced fallback when native cooperative exit is not proven; and
- keep `npm run build` outside service supervision because it is a one-shot
  build.

A custom programmatic-Vite wrapper could own `createServer()` and apply the
candidate recipe, but that adds a repository-specific maintenance layer and is
not recommended unless application-level drain evidence becomes materially
valuable. No shutdown handler should be added to `ai4u_fe/src` for this purpose.

The independent Windows native gate now passes. AkuSupervisor retains the
primary thread handle returned by `CreateProcessW`, assigns the suspended
process to its Job Object, and removes exactly its own suspend count through
that handle. It does not enumerate or resume an antivirus/EDR-owned thread.
Supervised services also receive an inherited `NUL` stdin handle instead of
sharing the Supervisor's interactive or redirected stdin; stdout and stderr
remain captured by bounded log pipes.

The passing application evidence includes readiness, application-observed
`SIGBREAK`, all four required cleanup events, `forced: false`, an empty owned
tree, listener release, and preservation of an unrelated process. The recipe
is therefore maintained for the current Windows adapter. Linux and macOS remain
separate future compatibility tuples rather than implied support.

## Live acceptance gate for every recipe

A recipe is promoted from planned to maintained only after all of these pass:

1. The managed application has a deterministic automated shutdown test.
2. AkuSupervisor launches a direct executable under its native ownership
   adapter.
3. The configured readiness contract becomes healthy.
4. A stop request returns `shutdown.gracefulSignalSent: true`.
5. The same response returns `shutdown.forced: false` and an empty
   `ownedPidsAfter`.
6. Application logs prove its expected signal handler ran and every declared
   cleanup phase completed; `forced: false` alone is insufficient.
7. The lifecycle journal contains the identical shutdown evidence.
8. Declared ports are released and no unrelated process is affected.
9. Every claimed operating system has native signal and process-tree evidence;
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

AkuSupervisorConformance owns runtime dependencies and native reports. This
repository owns the generic contract and current compatibility summary only;
neither core builds nor stable promotion may acquire a dependency on the
conformance checkout.
