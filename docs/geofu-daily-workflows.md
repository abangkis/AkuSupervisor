# Geofu Daily Development and Deployment Workflows

## Purpose and boundary

This document maps the real Geofu, GeoLibre, and Geofu BE daily workflows onto
AkuSupervisor without turning the development supervisor into a production
deployment orchestrator.

AkuSupervisor owns long-running local development processes. Artifact copy,
build, upload, cache invalidation, and remote EC2 switch commands remain
explicit operator-run tasks. They may later motivate a separate bounded task
model, but they are not disguised as services or lifecycle hooks.

## Workflow matrix

| Mode | Long-running services owned by AkuSupervisor | Explicit task outside AkuSupervisor | Important behavior |
|---|---|---|---|
| Daily unlocked development | `geofu-plugin`, `geolibre`, and optionally `geofu-be` | None after dependencies are installed and the BE executable is built | GeoLibre loads the live plugin manifest from port 8766; edits rebuild through Rollup and Vite |
| Locked QA / production-style development | `geolibre-locked` and optionally `geofu-be` | Run `npm run deploy:geolibre` from Geofu before starting or restarting locked GeoLibre | GeoLibre uses the copied bundled plugin and does not need the live plugin server |
| BE deployment | None | `./scripts/deploy-be.ps1 -NoVerifySsl`, then `sudo /usr/local/bin/switch-geofu-current` on EC2 | Builds/uploads a release and then changes remote current state; both operations are outside local process supervision |
| FE deployment | None | `npm run deploy-fe` from Geofu | Copies the plugin, builds locked GeoLibre, uploads assets, and invalidates CloudFront |

## Daily unlocked development

The repository-native commands are:

```text
Geofu:    npm run dev
GeoLibre: npm run geofu:dev
           -> npm run geofu:unlocked-dev
```

The canonical profile exposes those processes as `geofu-plugin` and
`geolibre`. When starting the watcher, service arguments also express the
operator's intended startup order without creating a hidden dependency graph:

```powershell
.\scripts\dev.ps1 akusidecar geofu-be geofu-plugin geolibre
```

If the backend is not needed for the current work, omit `geofu-be`:

```powershell
.\scripts\dev.ps1 akusidecar geofu-plugin geolibre
```

The equivalent explicit commands from a second terminal are:

```powershell
.\target\dev\aku-supervisor.exe start geofu-plugin `
  --reason "Geofu live plugin development"
.\target\dev\aku-supervisor.exe start geolibre `
  --reason "GeoLibre unlocked development"
```

`geolibre` health proves that Vite serves its static favicon on port 6060.
Complete unlocked workflow readiness still requires `geofu-plugin` to be healthy because
the plugin manifest is served independently on port 8766. Inspect both services
with `status`; AkuSupervisor does not collapse that relationship into one
misleading health value.

## Locked QA development

Locked mode is deliberately a separate service because it has different plugin
trust and discovery rules:

```text
GeoLibre: npm run geofu:locked-dev
```

Before locked mode can observe a new plugin build, copy the plugin into
GeoLibre's ignored bundled-plugin directory:

```powershell
cd C:\WorkspaceCodex\GeofuWorkspace\Geofu
npm run deploy:geolibre
```

Then start the locked profile:

```powershell
cd C:\WorkspaceCodex\AkuWorkspace\AkuSupervisor
.\target\dev\aku-supervisor.exe start geolibre-locked `
  --reason "locked QA against bundled Geofu plugin"
```

The repository-native command defaults both modes to port 6060. The canonical
AkuSupervisor profile keeps daily unlocked development on 6060 and explicitly
maps locked QA to 6061. This preserves the configuration rule that every
declared port has one owner, avoids an ambiguous duplicate declaration, and
allows side-by-side comparison when useful. The locked URL under supervision is
`http://127.0.0.1:6061/`.

For an ordinary mode switch, stop the active mode before starting the other:

```powershell
.\target\dev\aku-supervisor.exe stop geolibre `
  --reason "switch to locked QA"
.\target\dev\aku-supervisor.exe start geolibre-locked `
  --reason "locked QA against bundled Geofu plugin"
```

After another plugin change, stop locked GeoLibre, rerun `deploy:geolibre`, and
start it again so bundled-plugin discovery sees the copied artifact. The copy
command is not a service: it terminates, mutates generated files in another
checkout, and has no long-running process for AkuSupervisor to own.

## Production deployment boundary

The current operator flow is:

```powershell
# Geofu BE checkout
.\scripts\deploy-be.ps1 -NoVerifySsl
```

```bash
# EC2 host
sudo /usr/local/bin/switch-geofu-current
```

```powershell
# Geofu checkout
npm run deploy-fe
```

AkuSupervisor does not run these commands. In particular:

- `-NoVerifySsl` is a deployment-network choice, not a process-health option;
- the EC2 switch changes remote production state and requires separate access,
  audit, rollback, and approval boundaries;
- `deploy-fe` performs build, copy, S3 upload, and CloudFront invalidation rather
  than supervising a persistent local server; and
- success of a local service health check cannot prove a production deployment.

## Resilience evidence provided by these workflows

The profiles exercise different application styles without project-specific
lifecycle code:

- a direct Go executable with cooperative shutdown (`geofu-be`);
- an npm wrapper with a Node HTTP server and Rollup watcher (`geofu-plugin`);
- nested npm/Node/Vite wrappers with a slower pre-development step (`geolibre`);
- two repository modes separated into deterministic supervised ports; and
- a live external plugin versus a copied bundled-plugin snapshot.

The 120-second GeoLibre startup deadline accommodates its repository-owned
`predev` JupyterLite preparation and Vite dependency optimization. It does not
weaken ordinary control-plane timeouts. The profiles leave
`GEOLIBRE_DEV_HOST` unset so the checked-out GeoLibre branch retains its native
host binding. In the current `geofu-viewer-v2.1` branch that is `0.0.0.0`, so
the UI remains reachable through both loopback and the workstation's LAN
address. AkuSupervisor only sets the ports to 6060 and 6061.

The health request uses `/favicon.png`, not the application root. This keeps
listener readiness independent from Vite's potentially long first dependency
optimization. If any requested startup service still fails, the watcher remains
active and retains other successful services for inspection.

Still deferred are dependency graphs, arbitrary pre/post hooks, one-shot task
execution, remote deployment control, and automatic production promotion.
