# Chunk cache: behavior and safety

iscsi-s3 sits a **write-through, whole-chunk LRU** in front of the S3 (or memory) store. Understanding when that cache is coherent — and when it is not — matters for single-instance labs vs multi-instance MPIO.

Implementation: [`src/cache.rs`](../../src/cache.rs) (`ChunkCache`, `CachedStore`).

## What it is

| Property | Behavior |
|----------|----------|
| Unit of caching | One full chunk (`volumes[].chunk_size`, default 4 MiB) |
| Scope | One process: shared pool across all volumes (`cache.max_bytes`) |
| Key | `(volume name, chunk index)` |
| Policy | Approximate LRU (access tick); eviction when over budget |
| Disabled | `cache.max_bytes = 0` — puts never stick; every read hits the inner store |
| Persistence | Memory only; restart always cold |

```text
SCSI READ  → CachedStore.load_chunk → hit? return copy
                                  → miss? inner.read_at → put in LRU → return

SCSI WRITE → CachedStore.write_at → inner.write_at (S3 Put, must succeed first)
                                  → if chunk already in LRU: patch bytes in place
                                  → else: invalidate that chunk key (no insert)
```

With the default **write cache** (`volumes[].write_cache.mode = "none"`), writes never complete as “cached only.” A successful SCSI write means the inner store accepted the data (for S3: `PutObject`, with CAS retries on partial-chunk RMW).

## Write cache

Optional **per-volume write-back** (`WriteCachedStore` in [`src/write_cache.rs`](../../src/write_cache.rs)), in front of the read LRU.

| `mode` | Behavior | Crash |
|--------|----------|--------|
| `none` (default) | Write-through to S3 | No extra loss |
| `memory` | Dirty whole chunks in RAM; SCSI write returns after the patch | Dirty data lost |
| `disk` | Same plus atomic files under `write_cache.path` | Recovered on reopen, then flushed |

Optional `write_cache.max_bytes` (default `0` = unlimited) caps dirty data. When the set would exceed the budget, the **oldest** dirty chunks leave the set until it fits: they are put to S3 immediately if the write buffer is disabled, or handed to the buffer if it is enabled. A non-zero budget must be at least `chunk_size` — the cache stores whole chunks.

`iscsi_s3_write_cache_bytes` is the current dirty size in bytes, summed across volumes (updated on each `/metrics` scrape). It is a gauge, not a counter. Bytes already handed to the write buffer are not included.

Partial SCSI writes still RMW a full chunk, but the Get/Put to S3 is deferred until:

- SCSI SYNCHRONIZE CACHE (`flush`)
- admin copy / wipe / snapshot create
- I/O lock (e.g. migrate-cow)
- process shutdown (`Drop`)
- dirty size exceeds `max_bytes` (oldest first)

Reads serve dirty chunks first. Snapshot **restore/clone dest** discards dirty data so it cannot overlay restored pointers.

`mode = "disk"` requires a unique `path` per volume. Do not share a write-cache directory across volumes or hosts. Multi-instance MPIO plus write-back is **unsafe** (same as a hot read cache).

## Write buffer

Optional next stage after the dirty set and before `PutObject` (`write_buffer.max_bytes`, default `0`). `0` keeps the behaviour above: Puts only on flush or inline budget eviction. A non-zero budget requires a memory or disk write cache and must be at least `chunk_size`.

The write cache stays the coalesce window. The buffer is the queue of chunk snapshots waiting on S3. Bytes leave the dirty set on handoff, so `iscsi_s3_write_cache_bytes` and `iscsi_s3_write_buffer_bytes` do not overlap. A pinned cache means the coalesce window is full. A pinned buffer means Puts are not keeping up.

One drain thread per volume (`iscsi-s3-wbuf`) puts one chunk at a time. A chunk is handed off when it has been quiet for 100ms, or immediately if the write cache is over budget and needs the oldest slot. Streaming writes do not wait out the quiet period.

A later write to a handed-off index goes back into the dirty map as a newer generation. It does not mutate the snapshot already queued. Put completion drops that buffer snapshot only. A stale Put does not clear a newer dirty copy. In disk mode the local file stays until the Put succeeds and the dirty map does not hold a newer generation.

Reads check the dirty map, then the buffer, then the inner store. SCSI SYNCHRONIZE CACHE, shutdown, and I/O lock pause the worker, push every dirty chunk into the buffer, and wait until the buffer is empty. Snapshot restore and clone dest drop dirty and queued snapshots without putting, and wait until any in-flight Put has finished.

If the buffer is at `max_bytes` and the cache needs to hand off, the SCSI write blocks until a slot frees. Unlimited write cache still hands off quiet chunks; it only blocks if a handoff is required to free cache space.

`iscsi_s3_write_buffer_bytes` is queued plus in-flight snapshot bytes, summed across volumes. `iscsi_s3_write_buffer_chunks` is the count of those snapshots. Both are scrape-time gauges.

## What “data loss” means here

The cache does **not** hold dirty data waiting for S3. Process crash after a successful write does not lose that write relative to S3.

What *can* go wrong is **stale reads**: the initiator (or another path) sees older bytes than S3 currently holds, because this process’s LRU still has a pre-update copy. Filesystems and applications can then:

- Read stale data after failover / path switch
- Make decisions on stale reads and write back, compounding inconsistency

That is correctness / integrity risk, not “write never reached S3.”

## Safe configurations

### Single iscsi-s3 process (one or many initiator sessions)

**Safe to enable the cache** (`cache.max_bytes` > 0, default 256 MiB).

Within one process:

- Reads populate the LRU
- Writes update S3 first, then patch or invalidate the local entry
- Concurrent sessions on the **same** process share one `ChunkCache`, so they see each other’s writes after the write path runs

This includes **single-process dual-portal MPIO**: bind `0.0.0.0:3260`, advertise two NIC IPs in `portals`, keep the cache. Both paths land in one LRU. Trade-off: no rolling daemon upgrade. See [MPIO — Setup A](../users/mpio.md#setup-a--single-process-dual-nic-keep-the-cache).

Caveats that still apply (independent of cache):

- S3 is the durability boundary; if Put succeeds and the initiator already got a good SCSI status, durability is S3’s
- Full-chunk overwrites skip Get and put without `If-Match` (last writer wins on that object)
- Partial-chunk RMW uses ETag `If-Match` with retries — needed when multiple writers hit the same chunk (including multi-instance with cache off)

### Multi-instance MPIO (two or more daemons, same IQN/prefix)

**Not safe with cache enabled.** Set `cache.max_bytes = 0` on **every** instance.

```text
Instance A cache: chunk 5 = version V1
Instance B writes chunk 5 → S3 now V2 (CAS OK)
Initiator fails over to A → A serves V1 from LRU  ← stale
```

There is no cross-process invalidation, pub/sub, or shared memory. S3 CAS protects **write** races on RMW; it does not refresh peer LRUs.

If `portals` lists more than one address and the cache is enabled, the binary logs a notice reminding you that **peer processes** must not also cache the same prefix (single-process dual-portal is fine).

### After restart

Cache is empty. First reads hit S3. Safe relative to durability; only cold-cache latency.

## Unsafe or risky patterns

| Pattern | Risk |
|---------|------|
| Two (or more) daemons with `cache.max_bytes` > 0 on the same prefix | Stale reads after peer write or failover |
| External writer to the same chunk keys while cache is hot | Same stale-read problem |
| Assuming cache = write-back / battery-backed | It is not; do not size RAM expecting delayed durability |
| Truncating `max_bytes` below one chunk size | Chunks larger than `max_bytes` are never stored (`put` skips insert); effectively miss-always for those chunks — safe but pointless |

## Interaction with S3 CAS

| Layer | Role |
|-------|------|
| `CachedStore` | Coherence **inside one process** only |
| `S3ChunkStore` RMW + `If-Match` | Coherence of **object versions** under concurrent writers |

With cache **off**, two instances can RMW safely (up to CAS retry limits). With cache **on**, instance A can still return a cached V1 after B’s successful V2 put.

Full-chunk writes use `etag = None` (no precondition). Concurrent full overwrites of the same chunk are last-put-wins on S3; the cache on a peer can still be stale until eviction/restart/invalidate.

## Operational guidance

1. **Lab / single portal / one container:** leave default cache on for read performance.
2. **Dual-NIC, one process (network resilience + cache):** `bind = 0.0.0.0:3260`, list both IPs in `portals`, keep cache — [MPIO Setup A](../users/mpio.md#setup-a--single-process-dual-nic-keep-the-cache).
3. **Two or more daemons on the same volume (rolling upgrades):** `cache.max_bytes = 0` on every instance — [MPIO Setup B/C](../users/mpio.md). Use `iscsi-s3-ctl cache disable` on a live single-process instance before adding a peer ([admin control](../users/admin-ctl.md)).
4. **Suspect stale data:** restart the daemon (clears LRU) or set cache to 0 and restart; confirm no second caching peer shares the prefix.
5. **Metrics:** cache hit/miss counters are per volume; use them to see whether the LRU is doing useful work, not as a coherence signal across instances.
6. **Snapshots (COW):** `volume snapshot create|restore|clone` invalidate that volume’s entries in **this** process’s LRU. Multi-instance peers still need `cache.max_bytes = 0` — there is no cross-process invalidate. See [snapshots](../users/snapshots.md).

## Summary

| Scenario | Cache > 0 | Outcome |
|----------|-----------|---------|
| One process, local sessions | OK | Write-through; local coherence |
| One process, dual portals (`0.0.0.0` + two NIC IPs) | OK | Network resilience; no rolling upgrade |
| Process crash after SCSI write OK | OK | Data already in S3 |
| Two processes, same volume, active/active or failover | **Unsafe** | Stale reads possible |
| Two processes, cache = 0 | OK | Rely on S3 + CAS for writes; rolling upgrade |

When in doubt for **multi-instance** deployments: **turn the cache off**.
