# Architecture

## High-level data path

```text
iSCSI initiator
    │  TCP (one port per volume)
    ▼
IscsiTarget (vendored iscsi-target)
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

## One port per volume

Earlier multi-IQN-on-one-port (`IscsiServer`) paths were abandoned for digest interoperability. The binary starts **one `IscsiTarget` thread per volume**:

| Volume index | Listen port |
|-------------:|-------------|
| 0 | `bind_port` |
| 1 | `bind_port + 1` |
| … | … |

Discovery and login are therefore **per portal**.

## Advertise vs bind

- `bind` — where the process listens (`0.0.0.0:3260` is typical in containers).
- `advertise` — `TargetAddress` in SendTargets / related text. Required when `local_addr` is not client-reachable (Docker publish, NAT).

Implementation: `IscsiTargetBuilder::advertise_addr` in the vendored crate.

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
