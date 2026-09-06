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

Writes never complete as “cached only.” A successful SCSI write means the inner store accepted the data (for S3: `PutObject`, with CAS retries on partial-chunk RMW).

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

## Summary

| Scenario | Cache > 0 | Outcome |
|----------|-----------|---------|
| One process, local sessions | OK | Write-through; local coherence |
| One process, dual portals (`0.0.0.0` + two NIC IPs) | OK | Network resilience; no rolling upgrade |
| Process crash after SCSI write OK | OK | Data already in S3 |
| Two processes, same volume, active/active or failover | **Unsafe** | Stale reads possible |
| Two processes, cache = 0 | OK | Rely on S3 + CAS for writes; rolling upgrade |

When in doubt for **multi-instance** deployments: **turn the cache off**.
