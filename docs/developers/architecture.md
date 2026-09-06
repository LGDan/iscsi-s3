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

All volumes share `bind` (for example `0.0.0.0:3260`). Discovery lists every IQN with the same `TargetAddress` (from `advertise`, or the socket `local_addr`). Login includes `TargetName=<iqn>`; the server routes to that volume’s device.

Earlier builds used one TCP port per volume to work around digest bugs in the multi-target path. Digests on `IscsiServer` are fixed; the shared portal is the default again.

## Advertise vs bind

- `bind` — where the process listens (`0.0.0.0:3260` is typical in containers).
- `advertise` — `TargetAddress` in SendTargets / related text. Required when `local_addr` is not client-reachable (Docker publish, NAT).

Implementation: `advertise_addr` on `IscsiServer` / `IscsiTarget` builders in the vendored crate.

## Chunking and meta

- Disk = linear byte space of `capacity`, SCSI `block_size` (default 512).
- Split into `chunk_size` objects (default 4 MiB).
- Missing objects read as zeros (sparse).
- Writes RMW partial chunks as needed.
- `meta.json` locks capacity/geometry grow-only rules (see user configuration docs).

## Sessions and digests

Login uses the typestate session in `vendor/iscsi-target`. Operational keys include digests, burst lengths, `MaxConnections`, etc. Firmware initiators (for example Intel iSCSI Boot) are stricter than open-iscsi about:

- ISID echoed in Login Response
- Transit after `AuthMethod=None`
- Answers for keys such as `MaxConnections`

Header/data digests use CRC32C in little-endian wire order for open-iscsi/tgt interop when negotiated.

## Cache

Process-wide LRU of whole chunks (`cache.max_bytes`). Shared across volumes. Improves repeated reads; first touch after restart still hits S3.

## Security model (current)

No CHAP or ACL in default configs. Bind to trusted interfaces / networks only until auth lands.
