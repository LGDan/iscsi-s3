# Configuration reference

## Precedence

Lowest → highest:

1. Built-in defaults (`bind = 0.0.0.0:3260`, block size 512, chunk size 4 MiB, cache 256 MiB)
2. TOML file (`--config` / `-c`)
3. Environment (`ISCSI_S3_` prefix; nest with `__`)
4. CLI flags

Volumes (`[[volumes]]`) are defined only in TOML. Env/CLI override shared settings (bind, advertise, bucket, endpoint, region, path style, logging).

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
| `bind` | Base listen address `host:port`. Volume index *i* uses `port + i`. |
| `advertise` | Portal returned in SendTargets (`host` or `host:port`). Host-only reuses each volume’s listen port; `host:port` uses the same port-offset rules as `bind`. Unset → socket `local_addr` (often a container IP behind Docker). |
| `s3.bucket` | Required. Bucket name. |
| `s3.region` | AWS region (also used with custom endpoints). |
| `s3.endpoint` | Optional custom endpoint (MinIO, Ceph RGW, …). Omit for AWS. |
| `s3.force_path_style` | Path-style URLs (`http://endpoint/bucket/key`). Usually `true` for MinIO. |
| `s3.access_key_id` / `secret_access_key` | Optional static keys; otherwise AWS default credential chain / `AWS_*`. |
| `cache.max_bytes` | Shared in-process LRU for whole chunks. |
| `volumes[].name` | Short label (logs, INQUIRY product id). |
| `volumes[].iqn` | iSCSI target name (unique). |
| `volumes[].prefix` | S3 key prefix (unique). |
| `volumes[].capacity` | Size (`10GiB`, `64MiB`, or integer bytes). |
| `volumes[].block_size` | SCSI block size (default 512; locked after first meta write). |
| `volumes[].chunk_size` | S3 object size (default 4 MiB; locked after first meta write). |

Sizes accept human strings (`64KiB`, `4MiB`, `10GiB`) or raw byte integers.

## Port mapping

With `bind = "0.0.0.0:3260"` and two volumes:

| Index | Volume | Listen |
|------:|--------|--------|
| 0 | first `[[volumes]]` | `:3260` |
| 1 | second | `:3261` |

Docker must publish every used port. Discovery is **per portal** (run `iscsiadm` discovery on each port, or login with an explicit `-p host:port`).

## Environment

```bash
export ISCSI_S3_BIND=0.0.0.0:3260
export ISCSI_S3_ADVERTISE=127.0.0.1
export ISCSI_S3_S3__BUCKET=iscsi
export ISCSI_S3_S3__ENDPOINT=http://minio:9000
export ISCSI_S3_S3__FORCE_PATH_STYLE=true
export ISCSI_S3_CACHE__MAX_BYTES=256MiB
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
