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
| Daily LAN development | `geofu-plugin`, `geolibre`, and optionally `geofu-be` | One-time `npm run geofu:lan:cert -- <LAN-IP>` after the LAN address or certificate changes | GeoLibre serves trusted local HTTPS and proxies the live plugin and catalog through the same LAN origin |
| Locked QA / production-style development | `geolibre-locked` and optionally `geofu-be` | Run `npm run deploy:geolibre` from Geofu before starting or restarting locked GeoLibre | GeoLibre uses the copied bundled plugin and does not need the live plugin server |
| BE deployment | None | `./scripts/deploy-be.ps1 -NoVerifySsl`, then `sudo /usr/local/bin/switch-geofu-current` on EC2 | Builds/uploads a release and then changes remote current state; both operations are outside local process supervision |
| FE deployment | None | `npm run deploy-fe` from Geofu | Copies the plugin, builds locked GeoLibre, uploads assets, and invalidates CloudFront |

## Daily LAN development

The repository-native commands are:

```text
Geofu:    npm run dev
GeoLibre: npm run geofu:lan
```

Before the first supervised LAN start, generate and trust the repository-owned
development certificate from the GeoLibre checkout:

```powershell
cd C:\WorkspaceCodex\GeofuWorkspace\GeoLibre
npm run geofu:lan:cert -- 192.168.1.9
```

Rerun that explicit setup if the workstation LAN IP changes. The generated
`.certs/geofu-lan.json` and PFX remain GeoLibre-owned local secrets; they are not
copied into AkuSupervisor configuration or logs.

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
  --reason "GeoLibre LAN HTTPS development"
```

`geolibre` health proves that the HTTPS listener accepts a loopback TCP
connection on port 6060. The GeoLibre wrapper publishes the plugin and catalog
through same-origin HTTPS proxy paths while their source services remain
independently supervised on ports 8766 and 8765. Inspect all three service
states with `status`; AkuSupervisor does not collapse that relationship into
one misleading health value.

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

The repository-native profiles default both modes to port 6060. The canonical
AkuSupervisor profile keeps daily LAN development on 6060 and explicitly
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
certificate loading, `predev` JupyterLite preparation, and Vite dependency
optimization. `geofu:lan` itself binds `0.0.0.0`, enables HTTPS from the local
PFX, and constructs the LAN deep link. AkuSupervisor sets only port 6060 and
uses a loopback TCP readiness check so supervision neither overrides the
hardening nor needs access to the certificate passphrase. Locked QA retains its
HTTP static-asset health check on 6061. If any requested startup service still
fails, the watcher remains active and retains other successful services for
inspection.

Still deferred are dependency graphs, arbitrary pre/post hooks, one-shot task
execution, remote deployment control, and automatic production promotion.
