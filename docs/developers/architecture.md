# Architecture

## High-level data path

```text
iSCSI initiator
    │  TCP (shared portal)
    ▼
IscsiServer (vendored iscsi-target, IQN routing)
    │  SCSI READ/WRITE → ScsiBlockDevice
    ▼
S3BlockDevice
    │  lba/blocks → byte offsets
    ▼
ChunkCache (optional hit)
    │
    ▼
S3Store  →  {prefix}/chunks/{index:016x}
         →  {prefix}/meta.json
```

S3 I/O runs on a dedicated Tokio runtime. iSCSI connection threads call into the store via a spawn + channel pattern so they never nest `block_on` on the runtime.

## One portal, many IQNs

All volumes share `bind` (for example `0.0.0.0:3260`). Discovery lists every IQN with one or more `TargetAddress` values (from `portals`, else `advertise`, else the socket `local_addr`). Login includes `TargetName=<iqn>`; the server routes to that volume’s device.

Earlier builds used one TCP port per volume to work around digest bugs in the multi-target path. Digests on `IscsiServer` are fixed; the shared portal is the default again.

## Advertise, portals, and multi-instance MPIO

- `bind` — where **this** process listens (`0.0.0.0:3260` is typical in containers).
- `advertise` — single `TargetAddress` when `portals` is empty. Required when `local_addr` is not client-reachable (Docker publish, NAT).
- `portals` — full list of client-reachable portals for SendTargets. Use the **same** list on every iscsi-s3 instance that fronts the same volumes so discovery teaches the initiator every path.

**Path A (host multipath):** either (1) one daemon on `0.0.0.0` advertising two NIC portals (cache OK; no rolling upgrade), or (2) one daemon per path with shared S3 and `cache.max_bytes = 0` (rolling upgrades). SCSI serial/NAA derived from IQN so dm-multipath merges paths; chunk RMW uses S3 `If-Match` CAS. This is **not** MCS (`MaxConnections > 1`).

Operator guide: [MPIO setup](../users/mpio.md).

Implementation: `portal_addrs` / `advertise_addr` on `IscsiServer` / `IscsiTarget` builders in the vendored crate.

## Chunking and meta

- Disk = linear byte space of `capacity`, SCSI `block_size` (default 512).
- Split into `chunk_size` objects (default 4 MiB).
- Missing objects read as zeros (sparse).
- Optional per-volume chunk compression (`none` / `lz4` / `zstd` / `deflate`); compressed objects use an `ISC3` header. Locked in `meta.json`.
- Writes RMW partial chunks as needed.
- `meta.json` locks capacity/geometry grow-only rules (see user configuration docs).

## Sessions and digests

Login uses the typestate session in `vendor/iscsi-target`. Operational keys include digests, burst lengths, `MaxConnections`, etc. Firmware initiators (for example Intel iSCSI Boot) are stricter than open-iscsi about:

- ISID echoed in Login Response
- Transit after `AuthMethod=None`
- Answers for keys such as `MaxConnections`

Header/data digests use CRC32C in little-endian wire order for open-iscsi/tgt interop when negotiated.

## Cache

See **[Chunk cache: behavior and safety](cache.md)** for the full model (write-through, LRU, multi-instance risks).

Short version: process-wide whole-chunk LRU (`cache.max_bytes`), shared across volumes. Writes hit S3 first, then patch or invalidate the local entry. **Safe for a single daemon** (including dual-portal / dual-NIC on one process). **Unsafe across multi-instance peers** — use `cache.max_bytes = 0` when multiple processes share a volume prefix.

## Security model (current)

Optional CHAP (one-way or mutual) and initiator ACL are configured via `[auth]` / `volumes[].auth`. Discovery sessions remain unauthenticated. Bind to trusted interfaces when auth is disabled.
