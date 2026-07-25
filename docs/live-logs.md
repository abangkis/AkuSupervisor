# Live service logs

`aku-supervisor live-logs <service>` is the native development-time follower
for output already captured by AkuSupervisor.

```powershell
.\target\dev\aku-supervisor.exe live-logs ai4u-be
.\target\dev\aku-supervisor.exe live-logs ai4u-be --stream stderr
.\target\dev\aku-supervisor.exe live-logs ai4u-be --tail 0 --json
```

The default follows stdout and stderr together and starts with the last 50
events retained by the current Supervisor. `--stream stdout` and
`--stream stderr` select one stream. `--tail` accepts 0 through 1,000. Ctrl+C
terminates only this client process.

## Data path

```text
child stdout/stderr pipe
          |
          v
native process log pump
          |
          +-- 1. synchronous rotating-file write (authoritative)
          |
          +-- 2. bounded publish through ServiceLogSink
                              |
                              v
                         LiveLogHub
                         /    |    \
                   viewer  viewer  replay ring
```

The two outputs are not written through competing equal-priority queues. A
chunk becomes eligible for live publication only after its persistent write
succeeds. The hub never performs network I/O while holding its state lock and
uses a bounded queue per viewer. If a viewer is too slow, the hub drops only
that viewer's live events and later emits a `gap` record with the dropped
count. Service output and durable persistence continue normally.

The hub belongs to the Supervisor, not to one child process. A subscription
therefore stays open while its service is stopped and receives new output after
the service starts again. Unchanged services and subscribers survive live
configuration reconciliation.

## Protocol

The authenticated endpoint is:

```text
GET /v1/services/{service}/logs/live?stream=both&tail=50
Accept: application/x-ndjson
Authorization: Bearer <runtime token>
```

`after=<sequence>` supports bounded reconnection replay. The response is
close-delimited NDJSON and contains versioned `line`, `gap`, and `heartbeat`
records. Line events include:

- `hubInstanceId`, which changes when AkuSupervisor restarts;
- one shared `sequence` across stdout and stderr;
- `serviceId` and `stream`;
- `observedAtUnixMs`; and
- `text`.

The shared sequence specifies the order in which the two capture pumps reached
the hub. It cannot reconstruct a stronger operating-system emission order.
Historical lines loaded from separate persistent files are best-effort merged;
new live lines are deterministically sequenced at observation time.

Heartbeats keep an idle connection observable across long service pauses or
machine sleep. The CLI reconnects after a closed connection and requests
events after its latest observed sequence. Persistent files remain the source
for complete forensic history and `logs` remains the bounded snapshot command.

## Portability

`LiveLogHub`, the NDJSON server/client, filters, replay logic, and
backpressure policy use only platform-neutral Rust and `std::net`. Windows owns
the current child-pipe capture adapter. A Linux or macOS process spawner must
persist its captured bytes and call the same `ServiceLogSink`; it must not
create an OS-specific live-log protocol.
