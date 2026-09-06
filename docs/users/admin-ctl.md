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
| `cache status` | Cache enabled flag, max/used bytes, entry count |
| `cache disable` | Set `max_bytes = 0` and **clear** all cached chunks |
| `cache enable [--max-bytes 256MiB]` | Enable cache (cold); default size is last non-zero budget |
| `cache set --max-bytes …` | Set budget (`0` disables + clears) |
| `volume list` | List configured volumes (name, IQN, capacity, prefix, compression, auth) |
| `volume s3-stats <name\|iqn>` | List S3 objects under the volume prefix: chunk/meta counts and bytes |
| `volume write-image <name\|iqn> --file PATH` | Stream a raw disk image into a volume that has **no chunk objects** yet |
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
{"op":"reload"}
```

No TLS or tokens in v1 — rely on filesystem permissions on the socket.

## Related

- [Configuration](configuration.md) — `[admin]` fields
- [MPIO setup](mpio.md) — disable cache before multi-instance bring-up
- [Chunk cache](../developers/cache.md)
