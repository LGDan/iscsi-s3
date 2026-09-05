# iscsi-s3

Userspace iSCSI target that stores each virtual disk as fixed-size S3 objects (chunks).

## Features

- Layered config: defaults → TOML file → `ISCSI_S3_*` env → CLI
- Multiple volumes on one portal (IQN-routed via `IscsiServer`)
- Grow-only capacity via `{prefix}/meta.json` (no data migration)
- Whole-chunk LRU cache
- MinIO / path-style S3 compatible

## Quick start (Docker)

```bash
docker compose up -d --build
# target on localhost:3260, MinIO API on :9000 / console :9001
./scripts/smoke-test.sh
```

Config used by the container: [config.docker.toml](config.docker.toml).

## Quick start (local binary + MinIO)

```bash
docker compose up -d minio createbuckets
cp config.example.toml config.toml
# point endpoint at localhost and set credentials in config or env
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin

cargo run --release -- --config config.toml
```

All volumes listen on the shared `bind` address (default `0.0.0.0:3260`). Discovery lists every IQN.

## Initiator (Linux open-iscsi)

```bash
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260

sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk1 -p 127.0.0.1:3260 --login

lsblk
# optional smoke test (replace sdX)
sudo dd if=/dev/sdX of=/dev/null bs=1M count=8
sudo dd if=/dev/zero of=/dev/sdX bs=1M count=8
```

Logout:

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --logout
```

## Growing a volume

1. Raise `capacity` in `config.toml` (must be ≥ current size; shrink is refused).
2. Restart `iscsi-s3`.
3. On the initiator, rescan so the kernel picks up the new size:

```bash
echo 1 | sudo tee /sys/block/sdX/device/rescan
# or logout/login
```

Existing chunk objects are left untouched. Sparse unread regions remain zeros.

## Configuration

Precedence (low → high): built-in defaults, `--config` TOML, env (`ISCSI_S3_` prefix, nested with `__`), CLI flags.

Examples:

```bash
ISCSI_S3_S3__ENDPOINT=http://minio:9000 iscsi-s3 -c config.toml
iscsi-s3 -c config.toml --bind 0.0.0.0:3260 --bucket iscsi --force-path-style true
```

See [config.example.toml](config.example.toml).

### Object layout

```
{prefix}/meta.json
{prefix}/chunks/0000000000000000
{prefix}/chunks/0000000000000001
...
```

## Development

```bash
cargo test
cargo run -- --config config.toml --log iscsi_s3=debug
```
