# GeoLibre Portability Proof

Status: **Windows LAN HTTPS and locked supervision proof complete**

## Scope

This Phase 8 slice validates GeoLibre's repository-owned Geofu development
modes without changing GeoLibre source or teaching AkuSupervisor about Vite,
plugins, QA, or production deployment.

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

## Canonical profiles

The checked-in profile registers:

- `geolibre`: hardened LAN HTTPS mode on the repository-native
  `0.0.0.0:6060`;
- `geolibre-locked`: locked QA on the repository-native host, with its port
  overridden to 6061.

AkuSupervisor calls `geofu:lan` rather than recreating its certificate, proxy,
or deep-link policy. It sets only the deterministic 6060 port. The locked port
override is intentional: using 6061 keeps the single canonical configuration
valid and permits deliberate side-by-side comparison without changing
GeoLibre source.

LAN mode uses bounded `tcp-connect` readiness against `127.0.0.1:6060` because
the application listener is HTTPS and AkuSupervisor must not bypass its
certificate hardening. Locked mode keeps HTTP-status health against the static
`/favicon.png` asset. Both retain a 120-second startup deadline.

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

The earlier pre-hardening unlocked proof produced:

- sequence 316: unlocked start reached healthy on 6060 with ten owned
  npm/Node/Vite processes while `geofu-plugin` was healthy on 8766;
- sequence 317: unlocked stop sent the graceful signal, used no forced fallback,
  emptied the owned tree, and released port 6060;
- the repository-owned `npm run deploy:geolibre` built and copied bundled Geofu
  plugin version 0.4.0;
- sequence 318: locked QA reached healthy on 6061 with eight owned processes,
  and served `/plugins/geofu/plugin.json` with `id: geofu`;
- sequence 319: locked stop reported `forced: false`, an empty
  `ownedPidsAfter`, and released port 6061; and
- sequence 320 restored daily unlocked development to healthy on 6060.

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
