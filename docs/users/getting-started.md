# User getting started

This guide runs **iscsi-s3** and connects a Linux initiator with **open-iscsi**.

## Prerequisites

- Docker and Docker Compose (Option A), **or** a Rust toolchain 1.70+ (Option B)
- On the initiator host: `open-iscsi` / `iscsi-initiator-utils`
- Network path from initiator → target on TCP **3260** (shared portal; all IQNs)

## Option A — Docker Compose (fastest)

From the repository root:

```bash
docker compose up -d --build
```

This starts:

| Service | Role |
|---------|------|
| `minio` | S3 API `:9000`, console `:9001` (`minioadmin` / `minioadmin`) |
| `createbuckets` | Creates bucket `iscsi` |
| `iscsi-s3` | Target (config mounted from the host) |

Default Compose mounts a lab config from the repo root (`config.integration.toml` or `config.docker.toml` — check `docker-compose.yml`).

**Important:** set `advertise` in that TOML to an address **clients** use (for example `127.0.0.1` on the same machine, or `192.168.x.x` on the LAN). Without it, SendTargets may advertise the container’s internal IP and `iscsiadm` will store an unusable portal.

Publish portal port `3260` in Compose.

Watch logs:

```bash
docker compose logs -f iscsi-s3
```

Stop and remove MinIO data:

```bash
docker compose down -v
```

## Option B — Local binary + MinIO

```bash
docker compose up -d minio createbuckets
cp config.example.toml config.toml
# edit config.toml; set advertise if needed
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
cargo run --release -- --config config.toml
```

## Option C — AWS S3

Omit `s3.endpoint` / use real region credentials. See [configuration](configuration.md) and [examples — AWS S3](../examples.md#3-aws-s3-single-volume).

## Connect from Linux (open-iscsi)

### Install

```bash
# Debian / Ubuntu / MX
sudo apt-get update && sudo apt-get install -y open-iscsi

# Fedora / RHEL
sudo dnf install -y iscsi-initiator-utils
```

Optional stable initiator IQN in `/etc/iscsi/initiatorname.iscsi`:

```text
InitiatorName=iqn.2026-09.example.client:host1
```

### Discover and login

One portal lists all volumes:

```bash
HOST=127.0.0.1   # or your advertise / LAN address
sudo iscsiadm -m discovery -t sendtargets -p ${HOST}:3260
# Example: both disk0 and disk1 appear on the same portal
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p ${HOST}:3260 --login
lsblk
```

Logout:

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p ${HOST}:3260 --logout
```

## Verify SendTargets address

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -o show | grep address
```

`node.conn[0].address` should be your advertised host, not a Docker bridge IP like `172.25.0.x`. If wrong, fix `advertise`, restart the target, delete the node record, and rediscover:

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p OLDHOST:3260 -o delete
```

## Next steps

- [Tutorials](tutorials.md) — format/mount, grow, remote, iSCSI boot
- [Configuration](configuration.md) — full reference
- [Examples](../examples.md) — ready-made Compose + TOML recipes
