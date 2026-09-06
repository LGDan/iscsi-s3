# Configuration reference

## Precedence

Lowest → highest:

1. Built-in defaults (`bind = 0.0.0.0:3260`, block size 512, chunk size 4 MiB, cache 256 MiB)
2. TOML file (`--config` / `-c`)
3. Environment (`ISCSI_S3_` prefix; nest with `__`)
4. CLI flags

Volumes (`[[volumes]]`) are defined only in TOML. Env/CLI override shared settings (bind, advertise, bucket, endpoint, region, path style, logging). Multi-portal lists (`portals`), `instance`, and most auth fields are TOML-oriented; auth secrets may also come from env (`ISCSI_S3_AUTH__SECRET`, etc.). There are no CLI flags for CHAP secrets.

`RUST_LOG` is preferred when set; otherwise `--log` applies (Compose commonly sets `RUST_LOG`).

## Minimal TOML

```toml
bind = "0.0.0.0:3260"
advertise = "127.0.0.1"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://127.0.0.1:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "256MiB"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "10GiB"
```

## Field reference

| Key | Description |
|-----|-------------|
| `bind` | Listen address `host:port` for **this** process (all IQNs on one TCP port). |
| `advertise` | Single portal in SendTargets when `portals` is empty (`host` or `host:port`). Host-only reuses the port from `bind`. Unset → socket `local_addr` (often a container IP behind Docker). |
| `portals` | All client-reachable portals (`host` or `host:port`) listed in SendTargets for MPIO. Same list on every multi-instance peer. Takes precedence over `advertise`. |
| `instance` | Optional label for logs (multi-instance deployments). |
| `auth.username` / `auth.secret` | One-way CHAP (both required). Omit `[auth]` for no authentication. |
| `auth.mutual_username` / `auth.mutual_secret` | When both set with username/secret → mutual CHAP. |
| `auth.allowed_initiators` | Optional list of initiator IQNs allowed after successful auth. |
| `s3.bucket` | Required. Bucket name. |
| `s3.region` | AWS region (also used with custom endpoints). |
| `s3.endpoint` | Optional custom endpoint (MinIO, Ceph RGW, …). Omit for AWS. |
| `s3.force_path_style` | Path-style URLs (`http://endpoint/bucket/key`). Usually `true` for MinIO. |
| `s3.access_key_id` / `secret_access_key` | Optional static keys; otherwise AWS default credential chain / `AWS_*`. |
| `cache.max_bytes` | Shared in-process LRU for whole chunks. Safe with one process (including dual-portal). Use `0` when multiple daemons share a volume. Live toggle via [`iscsi-s3-ctl`](admin-ctl.md). Details: [cache safety](../developers/cache.md), [MPIO Setup A](mpio.md#setup-a--single-process-dual-nic-keep-the-cache). |
| `admin.enabled` | Listen for `iscsi-s3-ctl` on a Unix socket (default `true`). |
| `admin.socket` | Admin UDS path (default `/tmp/iscsi-s3/admin.sock`). |
| `volumes[].name` | Short label (logs, INQUIRY product id). |
| `volumes[].iqn` | iSCSI target name (unique). |
| `volumes[].prefix` | S3 key prefix (unique). |
| `volumes[].capacity` | Size (`10GiB`, `64MiB`, or integer bytes). |
| `volumes[].block_size` | SCSI block size (default 512; locked after first meta write). |
| `volumes[].chunk_size` | S3 object size (default 4 MiB; locked after first meta write). |
| `volumes[].compression` | Chunk compression: `none` (default), `lz4`, `zstd`, or `deflate`. Locked in `meta.json` like geometry; change requires a new prefix. |
| `volumes[].auth` | Optional per-volume CHAP override (unset fields inherit from `[auth]`). |

Sizes accept human strings (`64KiB`, `4MiB`, `10GiB`) or raw byte integers.

## CHAP authentication

Omit `[auth]` (default) → `AuthMethod=None`. Discovery sessions stay unauthenticated; CHAP applies to **normal** (non-discovery) login only.

```toml
[auth]
username = "iscsiuser"
secret = "change-me"
# mutual_username = "targetid"
# mutual_secret = "change-me-too"
# allowed_initiators = ["iqn.1993-08.org.debian:01:host1"]
```

Prefer env for secrets:

```bash
export ISCSI_S3_AUTH__USERNAME=iscsiuser
export ISCSI_S3_AUTH__SECRET='change-me'
```

open-iscsi after discovery:

```bash
IQN=iqn.2026-09.local.iscsi-s3:disk0
HOST=127.0.0.1
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:3260 \
  --op update -n node.session.auth.authmethod -v CHAP
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:3260 \
  --op update -n node.session.auth.username -v iscsiuser
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:3260 \
  --op update -n node.session.auth.password -v 'change-me'
# Mutual CHAP also needs:
#   node.session.auth.username_in / password_in
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:3260 --login
```

Or with `iscsi-s3-ctl` (sets the same node keys, then logs in):

```bash
export ISCSI_S3_CHAP_USERNAME=iscsiuser
export ISCSI_S3_CHAP_PASSWORD='change-me'
sudo -E iscsi-s3-ctl volume connect disk0
sudo iscsi-s3-ctl volume device disk0
```

MPIO peers must use the **same** auth settings on every instance.

## Port / portal

All volumes on one process share one listen address (`bind`). Discovery against that portal returns every IQN; each IQN lists `portals` (or a single `advertise`) as `TargetAddress` values.

For dual-path MPIO see **[MPIO setup](mpio.md)** (single-process + cache, or multi-instance + rolling upgrades) and `docker-compose.mpio.yml` / `config.mpio-single.toml`.

Docker must publish each process’s portal port (default `3260`).

## Environment

```bash
export ISCSI_S3_BIND=0.0.0.0:3260
export ISCSI_S3_ADVERTISE=127.0.0.1
export ISCSI_S3_S3__BUCKET=iscsi
export ISCSI_S3_S3__ENDPOINT=http://minio:9000
export ISCSI_S3_S3__FORCE_PATH_STYLE=true
export ISCSI_S3_CACHE__MAX_BYTES=256MiB
export ISCSI_S3_AUTH__USERNAME=iscsiuser
export ISCSI_S3_AUTH__SECRET='change-me'
export RUST_LOG=info,iscsi_s3=debug,iscsi_target=debug
```

## CLI

```bash
iscsi-s3 --config config.toml
iscsi-s3 -c config.toml --bind 0.0.0.0:3260 --advertise 127.0.0.1 --bucket iscsi
iscsi-s3 -c config.toml --endpoint http://127.0.0.1:9000 --force-path-style true
iscsi-s3 -c config.toml --log info,iscsi_s3=debug,iscsi_target=debug
```

## Object layout (per volume)

```text
{prefix}/meta.json
{prefix}/chunks/0000000000000000
{prefix}/chunks/0000000000000001
…
```

`meta.json` stores `capacity_bytes`, `chunk_size`, and `block_size`.

| On open | Behavior |
|---------|----------|
| Missing meta | Created from config |
| Config capacity larger | Grow (rewrite meta only) |
| Config capacity smaller | **Refused** |
| Different `chunk_size` / `block_size` | **Refused** |

## Logging

Useful filters:

| Filter | Use |
|--------|-----|
| `info` | Connection lifecycle, login, SCSI probes |
| `iscsi_s3=debug` | Application detail |
| `iscsi_target=debug` | PDU / bulk I/O detail |

Bulk READ/WRITE stay at debug so a mounted filesystem does not flood logs; INQUIRY / REPORT LUNS / READ CAPACITY appear at info.
