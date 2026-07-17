# GeoLibre Portability Proof

Status: **Windows LAN HTTPS canonical proof complete; locked supervision retained as historical boundary evidence**

## Scope

This Phase 8 slice validates GeoLibre without changing its source or teaching
AkuSupervisor about Vite, plugins, QA, or production deployment. Daily LAN
development remains canonical; the earlier locked proof is retained as
historical evidence but no longer belongs to the default development profile.

The proof covers:

- hardened LAN development through `npm run geofu:lan`;
- locked production-style QA through `npm run geofu:locked-dev`;
- repository-owned HTTPS certificate and same-origin proxy setup;
- TCP/HTTP readiness, npm/Node/Vite process-tree ownership, logs, journal, and
  bounded shutdown; and
- the handoff boundary to plugin copy and production deploy tasks.

It does not run AWS uploads, CloudFront invalidation, EC2 switching, or browser
acceptance testing.

## Repository assessment

GeoLibre defines:

```text
geofu:dev        -> geofu:unlocked-dev
geofu:lan        -> node scripts/geofu-lan-dev.mjs
geofu:locked-dev -> node scripts/geofu-locked-dev.mjs
```

All wrappers spawn the desktop workspace's Vite development server and inherit
forwarded arguments. Vite honors
`GEOLIBRE_DEV_HOST` and `GEOLIBRE_DEV_PORT`, and enables `strictPort`.

LAN mode requires `.certs/geofu-lan.json`, loads its PFX, binds `0.0.0.0`, and
proxies the loopback Geofu manifest and catalog behind the trusted LAN HTTPS
origin. Locked mode disables plugin management and allows only the bundled
`geofu` plugin.

## Local profile and historical proof

The local AkuWorkspace profile registers only the daily development mode:

- `geolibre`: hardened LAN HTTPS mode on the repository-native
  `0.0.0.0:6060`.

AkuSupervisor calls `geofu:lan` rather than recreating its certificate, proxy,
or deep-link policy. It sets only the deterministic 6060 port.

LAN mode uses bounded `tcp-connect` readiness against `127.0.0.1:6060` because
the application listener is HTTPS and AkuSupervisor must not bypass its
certificate hardening. It retains a 120-second startup deadline.

The historical proof also launched `geofu:locked-dev` on an isolated 6061
override and validated HTTP readiness and clean ownership. That result proves
genericity, but the mode was removed from the local development profile after its
deployment-oriented role was clarified.

LAN health proves only that Vite's TLS listener is accepting connections.
Complete workflow readiness requires `geofu-be`, `geofu-plugin`, and `geolibre`
to report healthy because their processes and endpoints remain independently
owned behind the GeoLibre same-origin proxies.

## Test baseline

The complete frontend command executed 2,587 tests on Windows:

- 2,584 passed;
- 2 failed for environment-specific reasons; and
- 1 was skipped.

The two failures were not GeoLibre/Geofu lifecycle failures:

- the Indonesian Windows locale formatted `2.5` as `2,5` in an attribute-stats
  assertion; and
- the generated Linux metainfo test could not locate `bash` on `PATH` even
  though Git Bash is installed outside the default command path.

A focused profile and plugin suite then passed 74 of 74 tests outside the
restricted spawn sandbox. It covered unlocked and locked build flags, external
plugin assets, archive unpacking, integrity pinning, trust, plugin management,
and plugin UI surfaces. No GeoLibre source file was changed.

## Live validation

Start the watcher and daily services:

```powershell
.\scripts\dev.ps1 akusidecar geofu-plugin geolibre
```

Then inspect from a second terminal:

```powershell
.\target\dev\aku-supervisor.exe status --json
.\target\dev\aku-supervisor.exe logs geolibre --stream stdout --tail 50
```

The earlier pre-hardening and locked boundary proof produced:

- sequence 316: unlocked start reached healthy on 6060 with ten owned
  npm/Node/Vite processes while `geofu-plugin` was healthy on 8766;
- sequence 317: unlocked stop sent the graceful signal, used no forced fallback,
  emptied the owned tree, and released port 6060;
- the repository-owned `npm run deploy:geolibre` built and copied bundled Geofu
  plugin version 0.4.0 as an explicit deployment-oriented task;
- sequence 318: locked QA reached healthy on 6061 with eight owned processes,
  and served `/plugins/geofu/plugin.json` with `id: geofu`;
- sequence 319: locked stop reported `forced: false`, an empty
  `ownedPidsAfter`, and released port 6061; and
- sequence 320 restored daily unlocked development to healthy on 6060.

Sequences 318 and 319 are historical portability evidence, not instructions to
register locked mode in the default AkuSupervisor profile.

The hardening migration was then live-validated against the repository-owned
certificate configuration for `192.168.1.9:6060`:

- sequence 430 started `geofu:lan`, retained eight npm/Node/Vite PIDs, opened
  `0.0.0.0:6060`, and reached healthy through the loopback TCP probe; and
- sequence 431 stopped it with `forced: false`, an empty `ownedPidsAfter`, and
  no listener left on port 6060.

AkuSidecar, Geofu BE, and the live Geofu plugin remained healthy throughout the
mode proof. The GeoLibre and Geofu repositories had no tracked source changes;
the locked plugin payload remains a generated, ignored deployment artifact.

## Deployment boundary

The adjacent artifact and production flow is maintained in
[Geofu daily workflows](geofu-daily-workflows.md). AkuSupervisor observes and
controls only local long-running development processes. Plugin copy, BE release
upload, remote EC2 current switching, FE build/upload, and invalidation remain
explicit operator-run steps.
