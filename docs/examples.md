# Examples

Copy-paste oriented recipes. Adjust hostnames, IQNs, capacities, and credentials before production use.

Treat iSCSI portals as **trusted-network** endpoints until CHAP/ACLs are available.

---

## 1. Local demo — MinIO + two small volumes

**Goal:** Quickest path to discover/login from the same machine.

### `config.toml`

```toml
bind = "0.0.0.0:3260"
advertise = "127.0.0.1"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://minio:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "64MiB"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "64MiB"
chunk_size = "64KiB"

[[volumes]]
name = "disk1"
iqn = "iqn.2026-09.local.iscsi-s3:disk1"
prefix = "disks/disk1"
capacity = "32MiB"
chunk_size = "64KiB"
```

### `docker-compose.yml`

```yaml
services:
  minio:
    image: minio/minio:latest
    command: server /data --console-address ":9001"
    ports:
      - "9000:9000"
      - "9001:9001"
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin
    volumes:
      - minio-data:/data
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:9000/minio/health/live"]
      interval: 3s
      timeout: 5s
      retries: 20

  createbuckets:
    image: minio/mc:latest
    depends_on:
      minio:
        condition: service_healthy
    entrypoint: >
      /bin/sh -c "
      mc alias set local http://minio:9000 minioadmin minioadmin;
      mc mb -p local/iscsi || true;
      "

  iscsi-s3:
    build: .
    depends_on:
      createbuckets:
        condition: service_completed_successfully
    ports:
      - "3260:3260"
    volumes:
      - ./config.toml:/etc/iscsi-s3/config.toml:ro
    environment:
      RUST_LOG: info,iscsi_s3=debug,iscsi_target=info
    restart: unless-stopped

volumes:
  minio-data:
```

### Initiator

```bash
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk1 -p 127.0.0.1:3260 --login
```

---

## 2. LAN lab — single large volume for NUC / bare metal

**Goal:** One disk advertised on a LAN IP for open-iscsi or firmware boot.

### `config.toml`

```toml
bind = "0.0.0.0:3260"
advertise = "192.168.88.15"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://minio:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "256MiB"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "16GiB"
chunk_size = "256KiB"
```

### Compose excerpt

```yaml
  iscsi-s3:
    build: .
    ports:
      - "3260:3260"
    volumes:
      - ./config.toml:/etc/iscsi-s3/config.toml:ro
    environment:
      AWS_ACCESS_KEY_ID: minioadmin
      AWS_SECRET_ACCESS_KEY: minioadmin
      RUST_LOG: info,iscsi_s3=debug,iscsi_target=debug
```

Firmware: Boot LUN **0**, portal `192.168.88.15:3260`, IQN as above. Digests `None`.

---

## 3. AWS S3 — single volume

**Goal:** No MinIO; use AWS credentials and a real bucket.

### `config.toml`

```toml
bind = "0.0.0.0:3260"
advertise = "10.0.0.20"

[s3]
bucket = "my-company-iscsi"
region = "eu-west-1"
# no endpoint → AWS
# force_path_style = false

[cache]
max_bytes = "512MiB"

[[volumes]]
name = "data0"
iqn = "iqn.2026-09.example.prod:data0"
prefix = "iscsi/data0"
capacity = "100GiB"
chunk_size = "4MiB"
```

### Run (host or container with IAM/role or keys)

```bash
export AWS_ACCESS_KEY_ID=AKIA...
export AWS_SECRET_ACCESS_KEY=...
export AWS_REGION=eu-west-1
iscsi-s3 --config config.toml
```

### Compose with env credentials (not recommended for production secrets)

```yaml
  iscsi-s3:
    build: .
    network_mode: host   # optional: simplify advertise = host IP
    volumes:
      - ./config.toml:/etc/iscsi-s3/config.toml:ro
    environment:
      AWS_ACCESS_KEY_ID: ${AWS_ACCESS_KEY_ID}
      AWS_SECRET_ACCESS_KEY: ${AWS_SECRET_ACCESS_KEY}
      AWS_REGION: eu-west-1
      RUST_LOG: info
```

Prefer IAM instance roles / IRSA / task roles over embedding keys.

---

## 4. Binary on host, MinIO in Docker

**Goal:** Debug the target under `cargo run` while keeping MinIO containerized.

### Compose (MinIO only)

```yaml
services:
  minio:
    image: minio/minio:latest
    command: server /data --console-address ":9001"
    ports:
      - "9000:9000"
      - "9001:9001"
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin
    volumes:
      - minio-data:/data

  createbuckets:
    image: minio/mc:latest
    depends_on: [minio]
    entrypoint: >
      /bin/sh -c "
      sleep 3;
      mc alias set local http://minio:9000 minioadmin minioadmin;
      mc mb -p local/iscsi || true;
      "

volumes:
  minio-data:
```

### Host `config.toml`

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

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "1GiB"
```

```bash
docker compose up -d
cargo run --release -- -c config.toml --log info,iscsi_target=debug
```

---

## 5. High-capacity archive disk (large chunks)

**Goal:** Fewer S3 objects; accept larger RMW on partial writes.

```toml
bind = "0.0.0.0:3260"
advertise = "10.0.0.20"

[s3]
bucket = "iscsi-archive"
region = "us-east-1"
endpoint = "http://minio:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "1GiB"

[[volumes]]
name = "archive"
iqn = "iqn.2026-09.local.iscsi-s3:archive"
prefix = "disks/archive"
capacity = "2TiB"
block_size = 512
chunk_size = "16MiB"
```

Pick `chunk_size` carefully: it is **immutable** after first open for that prefix.

---

## 6. Many small test volumes

**Goal:** Several IQNs for automation; tiny capacities and chunks.

```toml
bind = "0.0.0.0:3260"
advertise = "127.0.0.1"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://minio:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "32MiB"

[[volumes]]
name = "t0"
iqn = "iqn.2026-09.local.iscsi-s3:t0"
prefix = "test/t0"
capacity = "8MiB"
chunk_size = "64KiB"

[[volumes]]
name = "t1"
iqn = "iqn.2026-09.local.iscsi-s3:t1"
prefix = "test/t1"
capacity = "8MiB"
chunk_size = "64KiB"

[[volumes]]
name = "t2"
iqn = "iqn.2026-09.local.iscsi-s3:t2"
prefix = "test/t2"
capacity = "8MiB"
chunk_size = "64KiB"
```

### Compose ports

```yaml
ports:
  - "3260:3260"
```

---

## 7. Env-heavy override (12-factor style)

**Goal:** Same image/config file; environment selects bucket and advertise.

### Base `config.toml` (no secrets)

```toml
bind = "0.0.0.0:3260"

[s3]
region = "us-east-1"
force_path_style = true

[cache]
max_bytes = "256MiB"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "50GiB"
```

### Compose

```yaml
  iscsi-s3:
    build: .
    ports:
      - "3260:3260"
    volumes:
      - ./config.toml:/etc/iscsi-s3/config.toml:ro
    environment:
      ISCSI_S3_ADVERTISE: "192.168.88.15"
      ISCSI_S3_S3__BUCKET: iscsi
      ISCSI_S3_S3__ENDPOINT: http://minio:9000
      ISCSI_S3_S3__FORCE_PATH_STYLE: "true"
      AWS_ACCESS_KEY_ID: minioadmin
      AWS_SECRET_ACCESS_KEY: minioadmin
      RUST_LOG: info
```

---

## 8. Compose with smoke client profile

**Goal:** Automated R/W after the target is up.

```yaml
services:
  # ... minio, createbuckets, iscsi-s3 as above ...

  smoke:
    build: .
    entrypoint: ["smoke_client"]
    command: ["iscsi-s3:3260"]
    depends_on:
      - iscsi-s3
    profiles: ["test"]
    restart: "no"
```

```bash
docker compose --profile test run --rm smoke
# or from host:
cargo run --release --example smoke_client -- 127.0.0.1:3260
```

---

## 9. Separate “data” and “boot” volumes

**Goal:** One volume for OS install / iSCSI boot experiments; another for bulk data.

```toml
bind = "0.0.0.0:3260"
advertise = "192.168.88.15"

[s3]
bucket = "iscsi"
region = "us-east-1"
endpoint = "http://minio:9000"
force_path_style = true
access_key_id = "minioadmin"
secret_access_key = "minioadmin"

[cache]
max_bytes = "512MiB"

[[volumes]]
name = "boot"
iqn = "iqn.2026-09.local.iscsi-s3:boot"
prefix = "disks/boot"
capacity = "32GiB"
chunk_size = "256KiB"

[[volumes]]
name = "data"
iqn = "iqn.2026-09.local.iscsi-s3:data"
prefix = "disks/data"
capacity = "200GiB"
chunk_size = "4MiB"
```

```yaml
ports:
  - "3260:3260"   # shared portal (boot + data IQNs)
```

Point firmware at **boot** IQN on port **3260**, LUN **0**. Mount **data** from the OS via open-iscsi on the same portal with the data IQN.

---

## 10. Grow capacity in place

Starting config:

```toml
[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "10GiB"
chunk_size = "4MiB"
```

Later:

```toml
capacity = "40GiB"   # must be >= previous
```

Restart target → initiator rescan → `resize2fs` / `xfs_growfs`. Do **not** change `chunk_size` or `prefix` if you want to keep data.

---

## 11. Ceph RGW (S3-compatible)

```toml
bind = "0.0.0.0:3260"
advertise = "10.0.0.20"

[s3]
bucket = "iscsi"
region = "default"
endpoint = "http://rgw.example.local:7480"
force_path_style = true
access_key_id = "RGWACCESSKEY"
secret_access_key = "RGWSECRETKEY"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "100GiB"
```

Confirm path-style and signature version with your RGW docs; adjust `force_path_style` / region as required.

---

## 12. Logging for initiator troubleshooting

```yaml
environment:
  RUST_LOG: info,iscsi_s3=debug,iscsi_target=debug
```

Expect sequences like:

1. `Login PDU` / `Login security complete: transit CSG=0->NSG=1`
2. `Login operational complete` including `MaxConnections=1`
3. `Session … entered FullFeaturePhase`
4. `SCSI INQUIRY` / `REPORT_LUNS` / `READ_CAPACITY_…`

If the peer EOFs before FullFeature, compare login response flags/ISID (see vendor notes). If FullFeature then silent EOF, check operational key answers and initiator firmware constraints.

---

## 13. Dual-path MPIO (two daemons, shared S3)

**Goal:** Resilience / rolling upgrades via Path-A multipath (not MCS).

Use the lab stack:

```bash
docker compose -f docker-compose.mpio.yml up -d --build
```

Or dual-NIC on bare metal (same volume IQN/prefix on both; cache off):

```toml
# instance A — bind this NIC
bind = "10.0.0.1:3260"
instance = "a"
portals = ["10.0.0.1:3260", "10.0.0.2:3260"]

[cache]
max_bytes = 0

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "100GiB"
```

```toml
# instance B — bind the other NIC
bind = "10.0.0.2:3260"
instance = "b"
portals = ["10.0.0.1:3260", "10.0.0.2:3260"]

[cache]
max_bytes = 0

[[volumes]]
# identical to A
```

Login both portals, then configure dm-multipath. Full walkthrough: **[MPIO setup](users/mpio.md)**.

---

## Related docs

- [User getting started](users/getting-started.md)
- [Configuration](users/configuration.md)
- [Tutorials](users/tutorials.md)
- [Developer testing](developers/testing.md)
