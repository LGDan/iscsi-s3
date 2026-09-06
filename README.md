# iscsi-s3

Userspace iSCSI target that presents virtual disks backed by S3 (or MinIO / other S3-compatible object stores).

Initiators see ordinary SCSI block devices. Each READ/WRITE maps to `GetObject` / `PutObject` on fixed-size chunk keys under a per-volume prefix.

```
Initiator  --iSCSI-->  iscsi-s3  --Get/PutObject-->  S3 / MinIO
                          |
                     chunk objects + meta.json
```

## Features

- Layered config: defaults → TOML → `ISCSI_S3_*` env → CLI
- Multiple volumes on one TCP portal (IQN-based routing)
- Multi-portal SendTargets + multi-instance Path-A MPIO (shared S3, CAS writes)
- Grow-only capacity via `{prefix}/meta.json`
- Sparse disks (missing chunks read as zeros)
- Whole-chunk LRU cache (disable for multi-instance)
- AWS S3 and path-style / custom-endpoint backends
- Prometheus metrics (`/metrics`)

## Quick start

```bash
docker compose up -d --build
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --login
```

Set `advertise` in the mounted config to an address clients can reach (for Docker, often `127.0.0.1` or your LAN IP). See [docs/users/getting-started.md](docs/users/getting-started.md).

## Documentation

| Section | Description |
|---------|-------------|
| [Docs index](docs/README.md) | Full documentation map |
| [User getting started](docs/users/getting-started.md) | Run the target and connect a Linux initiator |
| [Configuration](docs/users/configuration.md) | TOML, env, CLI reference |
| [MPIO setup](docs/users/mpio.md) | Dual-path multipath, lab Compose, dual-NIC, rolling upgrade |
| [User tutorials](docs/users/tutorials.md) | Mount disks, grow volumes, remote access, iSCSI boot |
| [Developer getting started](docs/developers/getting-started.md) | Build, layout, vendored crate |
| [Architecture](docs/developers/architecture.md) | How chunks, ports, and sessions work |
| [Testing](docs/developers/testing.md) | Unit, integration, and smoke tests |
| [Examples](docs/examples.md) | Use cases with config + Compose YAML |

## Build docs site

```bash
./scripts/build-docs-site.sh        # static HTML → ./site/
./scripts/serve-docs-site.sh        # live preview on :8000
./scripts/serve-docs-site.sh 8080   # custom port
```

## Limitations

- No CHAP / initiator ACL UI yet (treat portals as trusted-network only).
- No SCSI UNMAP/TRIM thin-provision reporting.
- Shrink and geometry (`chunk_size` / `block_size`) changes are refused after first open.
- Multi-instance MPIO requires `cache.max_bytes = 0` and identical IQN/prefix/portals on peers.
- Not MCS (`MaxConnections > 1`); use separate sessions + dm-multipath.
- Primary validation: Linux open-iscsi, Intel iSCSI Boot (with caveats), bundled smoke client.

## License

MIT OR Apache-2.0
