# iscsi-s3 documentation

Userspace iSCSI target that presents virtual disks backed by S3 (or MinIO / other S3-compatible stores).

## For users

| Doc | Contents |
|-----|----------|
| [Getting started](users/getting-started.md) | Docker Compose or local binary; discover and login with open-iscsi |
| [Configuration](users/configuration.md) | Precedence, field reference, env/CLI |
| [Tutorials](users/tutorials.md) | Format/mount, grow, remote LAN, MPIO, firmware iSCSI boot notes |

## For developers

| Doc | Contents |
|-----|----------|
| [Getting started](developers/getting-started.md) | Clone, build, project layout, vendored `iscsi-target` |
| [Architecture](developers/architecture.md) | Ports, portals/MPIO, chunks, cache, login/advertise |
| [Testing](developers/testing.md) | Unit tests, MinIO integration, smoke script |

## Examples

| Doc | Contents |
|-----|----------|
| [Examples](examples.md) | Many use cases with full TOML and `docker-compose` snippets |

## Repo reference files

These live in the repository root (not rendered here):

- `config.example.toml` — local MinIO-oriented sample
- `config.docker.toml` — small Compose demo volumes
- `config.integration.toml` — larger single-volume lab config
- `config.mpio-a.toml` / `config.mpio-b.toml` — dual-instance MPIO lab
- `docker-compose.yml` — MinIO + iscsi-s3
- `docker-compose.mpio.yml` — MinIO + two iscsi-s3 paths
- `scripts/smoke-test.sh` — automated smoke
- `scripts/build-docs-site.sh` — build this static site via Docker
- `scripts/serve-docs-site.sh` — live-reload docs preview via Docker
- `vendor/iscsi-target/VENDOR.md` — notes on the vendored protocol crate

## Build this site

```bash
./scripts/build-docs-site.sh        # writes ./site/
./scripts/serve-docs-site.sh        # http://127.0.0.1:8000 (live reload)
./scripts/serve-docs-site.sh 8080
```
