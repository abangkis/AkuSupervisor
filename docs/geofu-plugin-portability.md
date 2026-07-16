# Geofu plugin portability proof

This proof validates the Geofu Node.js/Rollup development server as an
independently supervised service without changing the Geofu source code. It is
the second generic Geofu-family slice after the Geofu BE executable proof.

## Registered contract

The canonical [`akuworkspace.services.json`](../config/akuworkspace.services.json)
registers `geofu-plugin` with only generic service fields:

- working directory `C:\WorkspaceCodex\GeofuWorkspace\Geofu`;
- command `C:\nvm4w\nodejs\npm.cmd` with arguments `run`, `dev`;
- declared port `8766`;
- HTTP JSON health at
  `http://127.0.0.1:8766/geofu/plugin.json` expecting `id: geofu`;
- manual restart policy; and
- a five-second shutdown grace period.

The version is intentionally not pinned in the health expectation. A plugin
release may change version while the stable identity contract remains `geofu`.
Registration does not start the service implicitly.

## Baseline verification

Before supervision, the repository-owned workflow passed:

```powershell
cd C:\WorkspaceCodex\GeofuWorkspace\Geofu
npm run verify
```

It built the self-contained plugin bundle, passed JavaScript syntax and package
contract validation, and passed 80 JavaScript tests. Port `8766` was free before
AkuSupervisor claimed it.

## Live validation

With the development watcher and canonical configuration active:

```powershell
.\target\dev\aku-supervisor.exe start geofu-plugin
.\target\dev\aku-supervisor.exe status --json
.\target\dev\aku-supervisor.exe logs geofu-plugin --stream stdout --tail 20
.\target\dev\aku-supervisor.exe stop geofu-plugin --json
```

The service reached `running / healthy`, returned plugin `geofu` version `0.4.0`
with entry `index.js`, and retained the npm/cmd/Node/Rollup tree as four owned
PIDs. The captured stdout contained the initial Rollup build and both the
versionless and versioned manifest URLs.

The successful lifecycle records were:

- sequence `294`: `start`, `stopped -> running`;
- sequence `295`: `stop`, `running -> stopped`.

Stop evidence reported `gracefulSignalSent: true`, `forced: false`, and an empty
`ownedPidsAfter`. Port `8766` no longer accepted a connection. This
live-validates the Node.js development-server boundary independently of
AkuSidecar on Windows. Promotion to a maintained cross-platform Node recipe
still requires a deterministic application shutdown test plus Linux/macOS
native signal evidence.

## Generic health-adapter finding

The first start retained a functioning process tree but timed out as unhealthy.
Node's HTTP server returned a valid manifest using
`Transfer-Encoding: chunked`; the original health adapter passed HTTP chunk
framing into the JSON parser. AkuSupervisor now decodes bounded HTTP/1.1 chunked
bodies case-insensitively and rejects invalid or truncated framing before JSON
matching. No Geofu workaround or source change was required.

This distinction is deliberate: a health-adapter compatibility defect belongs
in AkuSupervisor rather than in an otherwise valid managed application.

## Phase boundary

This proof covers the plugin-only development server. It does not itself
validate the GeoLibre Vite/Tauri host, install a plugin into a browser profile,
or claim Linux/macOS process ownership. The subsequent GeoLibre slice uses the
live plugin server through canonical LAN HTTPS development; a historical proof
also exercised a copied bundled-plugin snapshot in locked mode. Their daily and
deployment boundaries are defined in
[Geofu daily workflows](geofu-daily-workflows.md).
