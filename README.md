# iscsi-s3

Userspace iSCSI target that presents one or more virtual disks whose blocks are stored as fixed-size objects in S3 (or MinIO / other S3-compatible stores).

Initiators see ordinary SCSI disks. Each READ/WRITE is mapped to `GetObject` / `PutObject` on chunk keys under a per-volume prefix.

## Features

- Layered configuration: defaults → TOML file → `ISCSI_S3_*` environment → CLI
- Multiple volumes (one IQN and TCP port per volume)
- Grow-only capacity via `{prefix}/meta.json` (no data migration)
- Sparse disks (missing chunks read as zeros)
- Whole-chunk LRU cache
- AWS S3 and path-style / custom-endpoint backends (MinIO, Ceph RGW, etc.)

## How it works

```
Initiator  --iSCSI-->  iscsi-s3  --Get/PutObject-->  S3 / MinIO
                         |
                    chunk objects + meta.json
```

- SCSI block size defaults to **512** bytes.
- The disk is split into chunks (default **4 MiB**). Each chunk is one S3 object.
- Volume *N* listens on `bind_port + N` (for example `:3260`, `:3261`, …).

---

## Getting started: target

### Option A — Docker Compose (recommended for a first run)

This starts MinIO and `iscsi-s3` with two demo volumes:

| Volume | IQN | Portal |
|--------|-----|--------|
| disk0 | `iqn.2026-09.local.iscsi-s3:disk0` | `localhost:3260` |
| disk1 | `iqn.2026-09.local.iscsi-s3:disk1` | `localhost:3261` |

```bash
docker compose up -d --build
```

- Target config: [config.docker.toml](config.docker.toml)
- MinIO API: `http://127.0.0.1:9000` (console `:9001`, user/pass `minioadmin` / `minioadmin`)

Verify with the bundled smoke script (unit tests + S3 integration + in-network iSCSI R/W):

```bash
./scripts/smoke-test.sh
```

Stop and wipe local MinIO data:

```bash
docker compose down -v
```

### Option B — Local binary + MinIO

1. Start MinIO and create the bucket:

```bash
docker compose up -d minio createbuckets
```

2. Copy and edit config:

```bash
cp config.example.toml config.toml
```

Set credentials either in the file:

```toml
[s3]
access_key_id = "minioadmin"
secret_access_key = "minioadmin"
```

or via the AWS chain:

```bash
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
```

3. Run the target:

```bash
cargo run --release -- --config config.toml
```

Or after `cargo build --release`:

```bash
./target/release/iscsi-s3 --config config.toml
```

### Option C — Real AWS S3

Omit `endpoint` and `force_path_style` (or set them appropriately for your account). Use an IAM role, shared credentials file, or:

```bash
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_REGION=eu-west-1
```

```toml
[s3]
bucket = "my-iscsi-bucket"
region = "eu-west-1"
# no endpoint → real AWS
```

Ensure the principal can `s3:GetObject`, `s3:PutObject`, and ideally `s3:ListBucket` on the prefixes you use.

---

## Target configuration

### Precedence

Lowest → highest:

1. Built-in defaults (`bind = 0.0.0.0:3260`, block size 512, chunk size 4 MiB, cache 256 MiB)
2. TOML file (`--config` / `-c`)
3. Environment variables (`ISCSI_S3_` prefix; nest with `__`)
4. CLI flags

Volumes (`[[volumes]]`) are defined in the TOML file. Env/CLI override shared settings (bind, bucket, endpoint, region, path style, logging).

### Example TOML

See [config.example.toml](config.example.toml):

```toml
bind = "0.0.0.0:3260"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://127.0.0.1:9000"   # omit for AWS
force_path_style = true                # typically true for MinIO
# access_key_id = "minioadmin"
# secret_access_key = "minioadmin"

[cache]
max_bytes = "256MiB"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "10GiB"
# block_size = 512          # optional
# chunk_size = "4MiB"       # optional; locked in meta.json after first open

[[volumes]]
name = "disk1"
iqn = "iqn.2026-09.local.iscsi-s3:disk1"
prefix = "disks/disk1"
capacity = "50GiB"
```

### Field reference

| Key | Description |
|-----|-------------|
| `bind` | Base listen address `host:port`. Volume index *i* uses `port + i`. |
| `s3.bucket` | Required. Target bucket name. |
| `s3.region` | AWS region (also used with custom endpoints). |
| `s3.endpoint` | Optional custom endpoint URL (MinIO, etc.). |
| `s3.force_path_style` | Use path-style addressing (`http://endpoint/bucket/key`). |
| `s3.access_key_id` / `secret_access_key` | Optional static keys; otherwise AWS default credential chain. |
| `cache.max_bytes` | Shared in-process LRU budget for whole chunks. |
| `volumes[].name` | Short label (logs, INQUIRY product id). |
| `volumes[].iqn` | iSCSI target name (must be unique; typically `iqn.…`). |
| `volumes[].prefix` | S3 key prefix for this volume (must be unique). |
| `volumes[].capacity` | Advertised size (`10GiB`, `64MiB`, or integer bytes). |
| `volumes[].block_size` | SCSI block size (default 512; immutable after first meta write). |
| `volumes[].chunk_size` | S3 object size (default 4 MiB; immutable after first meta write). |

Sizes accept human strings (`64KiB`, `4MiB`, `10GiB`) or raw byte integers.

### Environment examples

```bash
export ISCSI_S3_BIND=0.0.0.0:3260
export ISCSI_S3_S3__BUCKET=iscsi
export ISCSI_S3_S3__ENDPOINT=http://minio:9000
export ISCSI_S3_S3__FORCE_PATH_STYLE=true
export ISCSI_S3_CACHE__MAX_BYTES=256MiB   # if supported by your figment/bytesize parsing
```

### CLI examples

```bash
iscsi-s3 --config config.toml
iscsi-s3 -c config.toml --bind 0.0.0.0:3260 --bucket iscsi
iscsi-s3 -c config.toml --endpoint http://127.0.0.1:9000 --force-path-style true
iscsi-s3 -c config.toml --log info,iscsi_s3=debug
```

### Port mapping

With `bind = "0.0.0.0:3260"` and two volumes:

| Index | Volume | Listen address |
|------:|--------|----------------|
| 0 | first `[[volumes]]` entry | `0.0.0.0:3260` |
| 1 | second entry | `0.0.0.0:3261` |

Publish every used port from Docker (`3260`, `3261`, …).

### Object layout (per volume)

```
{prefix}/meta.json
{prefix}/chunks/0000000000000000
{prefix}/chunks/0000000000000001
…
```

`meta.json` stores `capacity_bytes`, `chunk_size`, and `block_size`. On open:

- Missing meta → created from config.
- Config capacity **larger** than meta → grow (rewrite meta only).
- Config capacity **smaller** than meta → **refused** (no shrink).
- Config `chunk_size` / `block_size` ≠ meta → **refused**.

### Growing a volume

1. Increase `capacity` in the TOML (must be ≥ current size).
2. Restart `iscsi-s3`.
3. On the initiator, rescan the block device (see client section).

Existing chunk objects are left untouched.

---

## Getting started: client (Linux open-iscsi)

These steps assume the target is reachable (for Compose demos: `127.0.0.1`).

### Install

Debian/Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y open-iscsi
```

RHEL/Fedora:

```bash
sudo dnf install -y iscsi-initiator-utils
```

Optional: set a stable initiator name in `/etc/iscsi/initiatorname.iscsi`:

```
InitiatorName=iqn.2026-09.example.client:host1
```

### Discover

Run discovery **per portal** (each volume has its own port):

```bash
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3261
```

Example output:

```
127.0.0.1:3260,1 iqn.2026-09.local.iscsi-s3:disk0
127.0.0.1:3261,1 iqn.2026-09.local.iscsi-s3:disk1
```

### Login

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk1 -p 127.0.0.1:3261 --login
```

### Find the block devices

```bash
lsblk
# or
ls -l /dev/disk/by-path/
dmesg | tail
```

New devices typically appear as `/dev/sdX`, `/dev/sdY`, …

### Format and mount (optional)

Only do this on a **new** empty volume:

```bash
# replace sdX with your device
sudo mkfs.ext4 /dev/sdX
sudo mkdir -p /mnt/iscsi-disk0
sudo mount /dev/sdX /mnt/iscsi-disk0
```

For a quick I/O check without a filesystem:

```bash
sudo dd if=/dev/zero of=/dev/sdX bs=1M count=8 oflag=direct
sudo dd if=/dev/sdX of=/dev/null bs=1M count=8 iflag=direct
```

### Logout

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --logout
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk1 -p 127.0.0.1:3261 --logout
```

### After growing the target

The kernel keeps the old size until you rescan:

```bash
# replace sdX
echo 1 | sudo tee /sys/block/sdX/device/rescan
blockdev --getsize64 /dev/sdX
```

Then grow the filesystem if needed (for example `sudo resize2fs /dev/sdX` for ext4). If rescan is unavailable, logout and login again.

### Autostart on boot (optional)

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --op update -n node.startup -v automatic
sudo systemctl enable --now iscsid
# on many distros also:
sudo systemctl enable --now open-iscsi
```

### Remote target

Replace `127.0.0.1` with the host running `iscsi-s3`, and open the portal ports on the firewall (default **3260+**). Prefer a trusted network or VPN; this project does not enable CHAP in the default config yet.

---

## Development

```bash
cargo test
ISCSI_S3_INTEGRATION=1 cargo test --test s3_integration -- --nocapture   # needs MinIO on :9000
cargo run --example smoke_client -- 127.0.0.1:3260
./scripts/smoke-test.sh
```

## Limitations (current)

- No CHAP / ACL UI yet (open network access to the portal).
- No SCSI UNMAP/TRIM thin-provision reporting.
- Shrink is refused; change `chunk_size` / `block_size` only on a new prefix.
- Multi-volume uses **one TCP port per volume** (not a single shared portal).
- Validated primarily with Linux open-iscsi and the bundled Rust smoke client.
