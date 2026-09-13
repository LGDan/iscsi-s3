---
name: S3 write buffer
overview: "Add an optional per-volume S3 write buffer that sits after the write cache: quiet or overflow chunks are handed off and Put in the background, with a configurable byte cap, scrape gauges, and a bottleneck-analyser component."
todos:
  - id: config
    content: Add per-volume write_buffer.max_bytes and validation
    status: completed
  - id: buffer
    content: Hand off dirty chunks into a background write buffer with generation-safe Put, flush, and discard
    status: completed
  - id: metrics
    content: Export write-buffer bytes/chunks gauges and a write_buffer bottleneck component
    status: completed
  - id: docs-tests
    content: Document the stage and cover handoff, in-flight rewrite, flush, and discard
    status: completed
isProject: false
---

# S3 write buffer

A new stage after the write cache and before `PutObject`. The write cache stays the coalesce window. The buffer is the queue of chunk snapshots waiting on S3. `0` (default) leaves today’s behaviour: no background Puts, only flush and inline budget eviction.

```mermaid
flowchart LR
  scsi[SCSI write]
  cache[Write cache dirty set]
  buffer[Write buffer]
  put[PutObject]
  scsi --> cache
  cache -->|"quiet or cache over budget"| buffer
  buffer --> put
```

Bytes leave the dirty set on handoff, so the two gauges do not overlap. A pinned cache means the coalesce window is full. A pinned buffer means Puts are not keeping up.

## Behaviour

Implemented inside [`src/write_cache.rs`](src/write_cache.rs) (same store, new stage). Started from [`WriteCachedStore::wrap`](src/write_cache.rs) when the buffer budget is non-zero.

- One drain thread per volume (`iscsi-s3-wbuf`). It puts one chunk at a time via the existing `flush_chunk` path, so different indexes are not in flight together. That keeps per-chunk order trivial.
- A chunk is handed off when it has been quiet for **100ms** (constant, not config), or immediately if the write cache is over `write_cache.max_bytes` and needs the oldest slot. Streaming writes do not wait out the quiet period.
- Handoff copies the snapshot into the buffer and removes it from the dirty map. A later write to that index goes back into the dirty map as a newer generation. It does not mutate the snapshot already queued.
- Put completion drops that buffer snapshot only. If the dirty map has a newer copy, it is queued after the in-flight put finishes. A stale Put must not clear the newer copy.
- Reads check the dirty map, then the buffer, then the inner store. A chunk sitting in the buffer is still visible.
- `SYNCHRONIZE CACHE`, shutdown, and `lock_io` pause the worker, push every dirty chunk into the buffer, and wait until the buffer is empty before returning.
- `discard_dirty` (restore / clone dest) drops dirty and buffer snapshots without putting, and waits until any in-flight Put has finished so it cannot overlay restored pointers.
- Disk mode: the local file stays until the Put succeeds and the dirty map does not hold a newer generation. Delete and `persist_chunk` share the dirty lock so a completing Put cannot remove a file that a newer write just wrote.
- If the buffer is at `max_bytes` and the cache needs to hand off (budget eviction), the SCSI write blocks until a slot frees. That is the backpressure. Unlimited write cache (`max_bytes = 0`) still hands off quiet chunks; it only blocks if a handoff is required to free cache space.

## Config

Per volume, same precedence as other settings (TOML, `ISCSI_S3_`, CLI is not needed — volumes are TOML-only, matching `write_cache`):

```toml
write_cache = { mode = "memory", max_bytes = "64MiB" }
write_buffer = { max_bytes = "64MiB" }
```

- [`VolumeConfig`](src/config.rs) gains `write_buffer: WriteBufferConfig` with `max_bytes` (`bytesize_serde`, default `0`).
- Validation: `0` or at least `chunk_size`. Non-zero with `write_cache.mode = "none"` is an error (write-through already Puts inline; there is no dirty set to hand off).
- Reload rejects a change (`volumes_structurally_changed` in [`src/admin.rs`](src/admin.rs)), same as write-cache mode and budget. The thread starts at volume open.

## Metrics

Sampled on `/metrics` scrape, same callback pattern as `iscsi_s3_write_cache_bytes` in [`src/main.rs`](src/main.rs) and [`src/metrics.rs`](src/metrics.rs):

- `iscsi_s3_write_buffer_bytes` — gauge, queued plus in-flight snapshot bytes, summed across volumes
- `iscsi_s3_write_buffer_chunks` — gauge, number of those snapshots

The bottleneck analyser in [`src/perf.rs`](src/perf.rs) gains a `write_buffer` component, registered on `iscsi_s3_bottleneck` next to the existing five. It lights when a budget is set, fill is at least 85%, SCSI writes happened, and buffer bytes did not drop by at least 10% over the window. It wins over `write_cache`, so a stalled Put queue is not blamed on the coalesce window.

## Docs and tests

- [`docs/users/configuration.md`](docs/users/configuration.md), [`config.example.toml`](config.example.toml), a short section in [`docs/developers/cache.md`](docs/developers/cache.md), and the component table in [`docs/developers/performance.md`](docs/developers/performance.md).
- Unit tests in `write_cache.rs`: handoff leaves the dirty map and reads still see the bytes; a rewrite during a blocked Put is not dropped when that Put finishes; `flush` waits for the buffer; `discard_dirty` does not put. Use a small `BlockStore` whose `write_at` can block, so the in-flight race is deterministic.
- Config test: buffer below `chunk_size` rejected; buffer with `mode = "none"` rejected.
