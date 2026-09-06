# Documentation

## For users

| Doc | Contents |
|-----|----------|
| [Getting started](users/getting-started.md) | Docker Compose or local binary; discover and login with open-iscsi |
| [Configuration](users/configuration.md) | Precedence, field reference, env/CLI |
| [MPIO setup](users/mpio.md) | Dual-path multipath: lab Compose, dual-NIC, rolling upgrade |
| [Admin control](users/admin-ctl.md) | `iscsi-s3-ctl`: stats, cache toggle, safe reload |
| [Tutorials](users/tutorials.md) | Format/mount, grow, remote LAN, firmware iSCSI boot notes |
| [Install OS on iSCSI](users/iscsi-os-install.md) | Live ISO → install to LUN → chroot fixup (Ubuntu, Debian, Alpine) |

## For developers

| Doc | Contents |
|-----|----------|
| [Getting started](developers/getting-started.md) | Clone, build, project layout, vendored `iscsi-target` |
| [Architecture](developers/architecture.md) | Ports, portals/MPIO, chunks, login/advertise |
| [Chunk cache](developers/cache.md) | Write-through LRU, coherence, multi-instance risks |
| [Testing](developers/testing.md) | Unit tests, MinIO integration, smoke script |

## Examples

| Doc | Contents |
|-----|----------|
| [Examples](examples.md) | Many use cases with full TOML and `docker-compose` snippets |

## Build the static site

```bash
./scripts/build-docs-site.sh        # writes ./site/
./scripts/serve-docs-site.sh        # http://127.0.0.1:8000 (live reload)
./scripts/serve-docs-site.sh 8080   # custom port
```

Uses a temporary `squidfunk/mkdocs-material` container (no local Python required).

## Related files in the repo

| Path | Role |
|------|------|
| [config.example.toml](../config.example.toml) | Local MinIO-oriented sample |
| [config.docker.toml](../config.docker.toml) | Small Compose demo volumes |
| [config.integration.toml](../config.integration.toml) | Larger single-volume lab config |
| [config.mpio-single.toml](../config.mpio-single.toml) | Single-process dual-portal MPIO (cache OK) |
| [config.mpio-a.toml](../config.mpio-a.toml) / [config.mpio-b.toml](../config.mpio-b.toml) | Dual-instance MPIO lab |
| [docker-compose.yml](../docker-compose.yml) | MinIO + iscsi-s3 |
| [docker-compose.mpio.yml](../docker-compose.mpio.yml) | MinIO + two iscsi-s3 paths |
| [scripts/smoke-test.sh](../scripts/smoke-test.sh) | Automated smoke |
| [scripts/build-docs-site.sh](../scripts/build-docs-site.sh) | Build static docs site via Docker |
| [scripts/serve-docs-site.sh](../scripts/serve-docs-site.sh) | Live-reload docs preview via Docker |
| [vendor/iscsi-target/VENDOR.md](../vendor/iscsi-target/VENDOR.md) | Notes on the vendored protocol crate |
