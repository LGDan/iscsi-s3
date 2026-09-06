# MPIO setup

Path-A multipath for iscsi-s3: **multiple daemon instances**, each owning one portal, sharing the same S3-backed volumes. The initiator opens a separate iSCSI session per portal; **dm-multipath** bonds those paths so you can take one instance down for upgrade without losing the LUN.

This is **not** MCS (`MaxConnections > 1` on a single session). That mode is unsupported.

```text
                    ┌─────────────┐
   NIC / portal A ─►│ iscsi-s3-a  │──┐
                    └─────────────┘  │
 Initiator                           ├──► same bucket/prefix (chunks + meta)
 (dm-multipath)                      │
                    ┌─────────────┐  │
   NIC / portal B ─►│ iscsi-s3-b  │──┘
                    └─────────────┘
```

## Requirements (every setup)

| Rule | Why |
|------|-----|
| Identical `[[volumes]]` (same `iqn`, `prefix`, `capacity`, geometry) on every instance | Same LUN / same objects |
| Identical `portals = [...]` on every instance | Discovery lists every path from either portal |
| Identical `[auth]` / volume auth on every instance | Same CHAP secret so either path accepts login |
| `cache.max_bytes = 0` | Per-process LRU cannot invalidate peers; stale reads after failover |
| Shared S3 (or MinIO) credentials/bucket | Single source of truth; RMW uses `If-Match` CAS |
| Distinct `bind` (or host port publish) per instance | Each path has its own TCP endpoint |
| Optional `instance = "a"` / `"b"` | Labels logs only |

SCSI **serial** and **NAA** are derived from the IQN, so both paths report the same identity and multipath can merge them. Do not change IQNs between peers.

---

## Setup 1 — Lab Compose (two host ports)

Use this on one machine to prove discovery, dual login, multipath, and rolling restart. Portals are `127.0.0.1:3260` and `127.0.0.1:3261` (not dual NIC).

### Start

```bash
docker compose -f docker-compose.mpio.yml up -d --build
docker compose -f docker-compose.mpio.yml ps
```

| Service | Host reachability |
|---------|-------------------|
| `iscsi-s3-a` | portal `127.0.0.1:3260`, metrics `127.0.0.1:9091` |
| `iscsi-s3-b` | portal `127.0.0.1:3261`, metrics `127.0.0.1:9092` |
| MinIO | S3 `9000`, console `9001` |
| Prometheus | `9090` (scrapes both instances) |

Configs: `config.mpio-a.toml` and `config.mpio-b.toml` in the repo root.

### Discover and login both paths

```bash
IQN=iqn.2026-09.local.iscsi-s3:disk0

sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
# Expect two TargetAddress lines (or node records) for :3260 and :3261

sudo iscsiadm -m node -T "$IQN" -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T "$IQN" -p 127.0.0.1:3261 --login

lsblk -o NAME,SIZE,MODEL,SERIAL,TRAN
# Two sd* devices, same SERIAL (16 hex chars from IQN)
```

If discovery only shows one portal, fix `portals` in both configs, restart both containers, delete stale node records, and rediscover:

```bash
sudo iscsiadm -m node -T "$IQN" -o delete
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
```

### Enable dm-multipath

```bash
# Debian / Ubuntu
sudo apt-get install -y multipath-tools
# Fedora / RHEL
# sudo dnf install -y device-mapper-multipath

sudo systemctl enable --now multipathd
sudo multipath -r
sudo multipath -ll
```

You want one multipath map with two paths under the same WWID/serial. Prefer **failover** while validating:

```bash
# Example /etc/multipath.conf snippet — adjust wwid after first multipath -ll
defaults {
    user_friendly_names yes
    find_multipaths yes
}
# devices { ... } or multipaths { multipath { wwid ... path_grouping_policy failover } }
```

Then use `/dev/mapper/<name>` (not the raw `sd*` paths) for mkfs/mount.

### Smoke I/O and path kill

```bash
MAP=/dev/mapper/mpatha   # your name from multipath -ll
sudo dd if=/dev/urandom of="$MAP" bs=1M count=8 oflag=direct

# Take path A down
docker compose -f docker-compose.mpio.yml stop iscsi-s3-a
sudo multipath -ll    # one path failed / ghost; I/O should continue on B

sudo dd if="$MAP" of=/dev/null bs=1M count=8 iflag=direct

# Bring A back
docker compose -f docker-compose.mpio.yml start iscsi-s3-a
# May need: sudo iscsiadm -m node -T "$IQN" -p 127.0.0.1:3260 --login
sudo multipath -r
sudo multipath -ll
```

### Tear down

```bash
sudo umount /mnt/... 2>/dev/null || true
sudo multipath -F
sudo iscsiadm -m node -T "$IQN" -u
docker compose -f docker-compose.mpio.yml down -v
```

---

## Setup 2 — Dual NIC (production-shaped)

Two hosts or one host with two client-reachable NICs. Each iscsi-s3 process binds its NIC; both advertise the full portal list.

### Instance A (`config` on host/NIC A)

```toml
bind = "10.0.0.1:3260"
instance = "a"
portals = ["10.0.0.1:3260", "10.0.0.2:3260"]

[s3]
bucket = "iscsi"
region = "us-east-1"
# endpoint / credentials as usual

[cache]
max_bytes = 0

[metrics]
enabled = true
bind = "10.0.0.1:9090"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.example:disk0"
prefix = "disks/disk0"
capacity = "100GiB"
```

### Instance B

```toml
bind = "10.0.0.2:3260"
instance = "b"
portals = ["10.0.0.1:3260", "10.0.0.2:3260"]   # identical list

[s3]
# same bucket / prefix credentials as A

[cache]
max_bytes = 0

[metrics]
enabled = true
bind = "10.0.0.2:9090"

[[volumes]]
# identical block to A (iqn, prefix, capacity, geometry)
```

Run both binaries (systemd, Compose `network_mode: host`, or bare metal). Initiator:

```bash
IQN=iqn.2026-09.example:disk0
sudo iscsiadm -m discovery -t sendtargets -p 10.0.0.1:3260
sudo iscsiadm -m node -T "$IQN" -p 10.0.0.1:3260 --login
sudo iscsiadm -m node -T "$IQN" -p 10.0.0.2:3260 --login
sudo multipath -r && sudo multipath -ll
```

Firewall: allow TCP **3260** (and metrics if scraped) on both NICs from the initiator.

---

## Setup 3 — Single host, two binds (no Docker)

Useful when developing without Compose port mapping:

```bash
# Terminal A
iscsi-s3 --config config-a.toml --bind 127.0.0.1:3260

# Terminal B
iscsi-s3 --config config-b.toml --bind 127.0.0.1:3261
```

Both TOML files must still list:

```toml
portals = ["127.0.0.1:3260", "127.0.0.1:3261"]
```

`bind` in the file can be overridden by `--bind`; `portals` is TOML-only today.

---

## Rolling upgrade

1. Confirm multipath shows two healthy paths and I/O uses the map device.
2. Fail or stop instance A (`docker compose … stop iscsi-s3-a`, or `systemctl stop`, or `multipath -f` path / offline).
3. Wait until multipath shows A failed and B active; optional I/O check.
4. Upgrade/restart A; wait for `/health` (or `/healthz`) and metrics.
5. Relogin path A if the session died: `iscsiadm … --login`.
6. `multipath -r` until both paths are up.
7. Repeat for instance B.

Keep `cache.max_bytes = 0` for the whole life of a multi-instance volume.

---

## Verification checklist

| Check | How |
|-------|-----|
| Both portals in discovery | `iscsiadm -m discovery -t sendtargets -p <either-portal>` |
| Same SCSI serial on both sd* | `lsblk -o NAME,SERIAL` or `sg_inq -p 0x80 /dev/sdX` |
| Same NAA | `sg_inq -p 0x83 /dev/sdX` |
| Multipath merged | `multipath -ll` → one map, two paths |
| Cache off | logs warn if `portals.len() > 1` and cache > 0; configs show `max_bytes = 0` |
| CAS conflicts rare | look for `s3 chunk CAS conflict; retrying` under concurrent writes |
| Metrics per instance | scrape both `/metrics` endpoints |

---

## Troubleshooting

| Symptom | Likely cause |
|---------|----------------|
| Only one `TargetAddress` | `portals` missing/mismatched; rediscover after restart |
| Two disks, multipath will not merge | Different IQNs → different serial/NAA; fix volume config |
| Stale data after failover | Cache not zeroed on one or both instances |
| Write errors / precondition storms | Two writers with bad geometry mismatch, or S3 without If-Match support |
| Path stuck after restart | Session not re-logged in; run `iscsiadm --login` for that portal |
| Docker SendTargets shows `172.x` | Lab configs use host-published portals (`127.0.0.1:…`); do not rely on container `local_addr` |

---

## What this is not

- **MCS** — additional TCP connections in one iSCSI session (`MaxConnections > 1`).
- **Shared in-memory cache** across instances.
- **Automatic cluster membership** — you configure peers via identical `portals` lists.

## Related

- [Configuration](configuration.md) — `portals`, `advertise`, `cache`, [CHAP](configuration.md#chap-authentication)
- [Architecture](../developers/architecture.md) — identity and CAS notes
- [Examples §13](../examples.md#13-dual-path-mpio-two-daemons-shared-s3) — dual-NIC TOML snippets
- Repo files: `docker-compose.mpio.yml`, `config.mpio-a.toml`, `config.mpio-b.toml`
