# Admin control (`iscsi-s3-ctl`)

Manage a **running** iscsi-s3 daemon over a local Unix-domain socket. Useful for scripts and operators: stats, safe cache enable/disable, and limited config reload.

## Enable the socket

In TOML (defaults shown):

```toml
[admin]
enabled = true
socket = "/tmp/iscsi-s3/admin.sock"
```

Env: `ISCSI_S3_ADMIN__ENABLED`, `ISCSI_S3_ADMIN__SOCKET`.

The daemon creates the parent directory, removes a stale socket file, and binds with mode `0660` when possible. Production systemd units often use `/run/iscsi-s3/admin.sock` with a dedicated runtime directory.

The control client defaults to the same path, or `ISCSI_S3_ADMIN_SOCKET`.

```bash
iscsi-s3-ctl --socket /tmp/iscsi-s3/admin.sock stats
# or
export ISCSI_S3_ADMIN_SOCKET=/tmp/iscsi-s3/admin.sock
iscsi-s3-ctl stats --format json
```

Docker: install both `iscsi-s3` and `iscsi-s3-ctl` in the image. Mount/share the socket path (or `docker exec` and call ctl inside the container).

## Commands

| Command | Effect |
|---------|--------|
| `stats` | Uptime, bind, portals, instance, volumes, cache, iSCSI connection/session counts |
| `health` | Liveness: uptime, sessions, cache, S3 `HeadBucket` (exit 1 if degraded) |
| `cache status` | Cache enabled flag, max/used bytes, entry count |
| `cache disable` | Set `max_bytes = 0` and **clear** all cached chunks |
| `cache enable [--max-bytes 256MiB]` | Enable cache (cold); default size is last non-zero budget |
| `cache set --max-bytes …` | Set budget (`0` disables + clears) |
| `volume list` | List configured volumes (name, IQN, capacity, prefix, storage, compression, auth) |
| `volume sessions [name\|iqn]` | Active FullFeature sessions (initiator, peer, age) |
| `volume s3-stats <name\|iqn>` | List S3 objects under the volume prefix: chunk/meta counts and bytes |
| `volume write-image <name\|iqn> --file PATH` | Stream a raw disk image into a volume that has **no chunk objects** yet |
| `volume export <name\|iqn> --file PATH [--size N]` | Stream a raw image out of a volume (default: full capacity) |
| `volume grow <name\|iqn> --capacity SIZE` | Grow-only capacity update (meta.json + live READ CAPACITY) |
| `volume copy <from> <to> [--force]` | 1:1 sparse copy; **overwrites** destination (prompts unless `--force`) |
| `volume wipe <name\|iqn> [--force]` | Delete all chunk objects; keeps `meta.json` (prompts unless `--force`) |
| `volume snapshot create <vol> [--name ID] [--force]` | Create CoW snapshot (`storage=cow` only); quiesce unless `--force` |
| `volume snapshot list <vol>` | List snapshot headers |
| `volume snapshot delete <vol> <id> [--force]` | Delete snapshot + GC unreferenced objects |
| `volume snapshot restore <vol> <id> [--force]` | Restore live pointers from snapshot |
| `volume snapshot clone <vol> <id> --to <dest>` | Clone snapshot into empty matching COW volume |
| `volume migrate-cow <vol> [--force]` | Convert legacy flat layout → COW (then set `storage=cow` and restart) |
| `volume connect <name\|iqn> [--portal …] [--username …] [--password …]` | Local helper: `iscsiadm` discovery + optional CHAP + login |
| `volume disconnect <name\|iqn> [--portal HOST:PORT]` | Local helper: `iscsiadm` logout |
| `volume device <name\|iqn> [--wait SECS]` | Print local `/dev` path(s) for a connected volume |
| `reload` | Re-read the startup `--config` file + env; apply **safe** fields only |

Global flags: `--socket PATH`, `--format text|json`.

Exit code `0` when the daemon returns `"ok": true`; non-zero on transport errors or `"ok": false`.

## JSON output

```bash
iscsi-s3-ctl stats --format json
```

Envelope:

```json
{
  "ok": true,
  "data": { ... }
}
```

or `{ "ok": false, "error": "..." }`.

Example cache disable:

```bash
iscsi-s3-ctl cache disable --format json
```

S3 usage for a volume (paginated `ListObjectsV2` on the volume prefix):

```bash
iscsi-s3-ctl volume s3-stats disk0
iscsi-s3-ctl volume s3-stats disk0 --format json
# alias:
iscsi-s3-ctl volume usage iqn.2026-09.local.iscsi-s3:disk0
```

Reports object counts (`chunks`, `meta`, `other`) and summed object sizes. Unwritten sparse regions have no chunk object. With compression enabled, `bytes.chunks` is compressed on-disk size.

## Seed a raw image (`volume write-image`)

Write a dd-style / `.img` file into a volume **only when it has no chunk (block) objects yet**. Existing `meta.json` is fine. Unexpected non-chunk objects under the prefix are refused.

```bash
# Prefer offline: logout initiators first so nothing races the seed.
iscsi-s3-ctl volume write-image disk0 --file ./disk.img
# alias:
iscsi-s3-ctl volume seed disk0 -f ./disk.img
```

Rules:

- Image size must be `> 0` and `≤` volume capacity (smaller images leave the tail sparse).
- The client streams bytes over the admin socket (path is local to `iscsi-s3-ctl`, not the daemon).
- All-zero chunks are **skipped** so sparse regions stay object-free (same as never written).
- Writes go through the live volume store (cache stays coherent).

Wire protocol is two-phase: JSON request with `size` → ready JSON → raw body → final JSON.

## Copy a volume (`volume copy`)

Make a **1:1 sparse copy** of one configured volume onto another. Destination data is overwritten: source chunks are copied, and destination-only chunks are deleted so sparsity matches.

```bash
iscsi-s3-ctl volume copy disk0 disk1
# prompts: Type 'yes' to continue
iscsi-s3-ctl volume copy disk0 disk1 --force
```

Rules:

- Source and destination must be different volumes on the same daemon.
- `capacity`, `chunk_size`, and `block_size` must match.
- Compression may differ (payload is re-encoded for the destination).
- Client prompts for confirmation unless `--force` (non-TTY stdin also requires `--force`).
- Prefer logging out initiators on both volumes first.

## Wipe a volume (`volume wipe`)

Delete every chunk object under the volume prefix. `meta.json` (capacity / geometry / compression) is kept so the volume stays configured but fully sparse.

```bash
iscsi-s3-ctl volume wipe disk0
# prompts: Type 'yes' to continue
iscsi-s3-ctl volume wipe disk0 --force
```

Prefer `volume disconnect` (or logout) first so initiators are not reading/writing during the wipe.

## Grow capacity (`volume grow`)

Grow-only live capacity change. Updates `meta.json` and the in-memory store so SCSI `READ CAPACITY` reflects the new size immediately. Initiators usually need a device rescan (or logout/login).

```bash
iscsi-s3-ctl volume grow disk0 --capacity 20GiB
```

Also update the TOML `capacity` so the next restart does not refuse a shrink back to the old value.

## Export a raw image (`volume export`)

Stream the volume’s logical bytes to a local file (zeros for sparse regions):

```bash
iscsi-s3-ctl volume export disk0 --file ./disk.img
iscsi-s3-ctl volume export disk0 --file ./partial.img --size 1GiB
```

## Sessions (`volume sessions`)

List active FullFeature sessions (optional volume filter):

```bash
iscsi-s3-ctl volume sessions
iscsi-s3-ctl volume sessions disk0
```

## Health

```bash
iscsi-s3-ctl health
```

Reports uptime, connections/sessions, cache, and an S3 `HeadBucket` probe. Exit code `1` when `status` is not `ok` (e.g. S3 unreachable).

## Connect / disconnect (open-iscsi helpers)

These run **on the machine where you invoke `iscsi-s3-ctl`**, not inside the daemon. They look up the volume IQN (and portals) over the admin socket, then call `iscsiadm`. Requires `open-iscsi` / `iscsiadm` in `PATH`, and usually root.

```bash
# Discover + login (all advertised portals, or one with --portal)
sudo iscsi-s3-ctl volume connect disk0
sudo iscsi-s3-ctl volume connect disk0 --portal 127.0.0.1:3260

# CHAP (required when the volume auth is chap / mutual-chap)
sudo iscsi-s3-ctl volume connect disk0 \
  --username iscsiuser --password 'change-me'
# Prefer env so the secret is not on the command line:
export ISCSI_S3_CHAP_USERNAME=iscsiuser
export ISCSI_S3_CHAP_PASSWORD='change-me'
sudo -E iscsi-s3-ctl volume connect disk0
# Mutual CHAP also needs:
#   --mutual-username / --mutual-password
#   or ISCSI_S3_CHAP_MUTUAL_USERNAME / ISCSI_S3_CHAP_MUTUAL_PASSWORD

# Logout all sessions for that IQN (or one portal)
sudo iscsi-s3-ctl volume disconnect disk0
sudo iscsi-s3-ctl volume disconnect disk0 --portal 127.0.0.1:3260

# After connect: resolve the local block device (prefers multipath map)
sudo iscsi-s3-ctl volume device disk0
# DEV=$(sudo iscsi-s3-ctl volume device disk0 | awk '/^device:/{print $2}')
```

Portal selection: `--portal` if set; else daemon `portals` / `advertise` from `stats`; else a non-wildcard `bind`. If the daemon only binds `0.0.0.0:…` with no advertise/portals, pass `--portal` explicitly.

CHAP: connect configures `node.session.auth.*` on each portal before `--login`. Credentials come from flags or `ISCSI_S3_CHAP_*` env vars (never from the daemon admin socket). See also [configuration](configuration.md#chap-authentication).

`volume device` scans `/dev/disk/by-path/*-iscsi-{iqn}-lun-*`, resolves to `/dev/sd*`, and prefers a multipath `/dev/mapper/…` when the block device is a path under dm-multipath. Waits up to `--wait` seconds (default 5) for udev.

## Safe cache toggle

Disabling always flushes the LRU so the next reads hit S3. That is the right step **before** starting a second multi-instance peer that must not see stale data from this process’s cache. See [chunk cache safety](../developers/cache.md).

Enabling starts empty (cold) at the requested (or last) budget.

## Config reload — what applies

`reload` requires the daemon to have been started with `--config` / `-c`.

| Applied | Notes |
|---------|--------|
| `cache.max_bytes` | Via the same safe enable/disable path |

| Rejected (reported, need restart) | Notes |
|-----------------------------------|--------|
| `bind` | Listener cannot move |
| `volumes` | Add/remove/iqn/capacity/geometry |
| `s3` bucket/endpoint/region/path-style | Client already built |
| `auth` / CHAP | Bound into target table at start |

`portals` / `advertise` may update **stats labels** only; SendTargets still uses portals from process start until restart (warning in the reload response).

## Wire protocol

One JSON object per connection, newline-terminated request and response:

```json
{"op":"stats"}
{"op":"cache.disable"}
{"op":"cache.enable","max_bytes":268435456}
{"op":"cache.set","max_bytes":0}
{"op":"volume.list"}
{"op":"volume.s3_stats","volume":"disk0"}
{"op":"volume.write_image","volume":"disk0","size":1073741824}
{"op":"volume.copy","volume":"disk0","to":"disk1"}
{"op":"volume.wipe","volume":"disk0"}
{"op":"volume.grow","volume":"disk0","capacity":21474836480}
{"op":"volume.sessions"}
{"op":"volume.export","volume":"disk0"}
{"op":"health"}
{"op":"reload"}
```

No TLS or tokens in v1 — rely on filesystem permissions on the socket.

## Related

- [Configuration](configuration.md) — `[admin]` fields
- [MPIO setup](mpio.md) — disable cache before multi-instance bring-up
- [Chunk cache](../developers/cache.md)
