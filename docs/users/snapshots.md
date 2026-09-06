# Volume snapshots (copy-on-write)

Application-level snapshots for volumes configured with **`storage = "cow"`**. Legacy volumes keep today’s flat chunk layout and refuse snapshot ops.

## Storage modes

| Mode | Layout | Snapshots | I/O |
|------|--------|-----------|-----|
| **`legacy`** (default) | `{prefix}/meta.json` + `{prefix}/chunks/{016x}` full payloads | Refused | No hashing / pointer puts |
| **`cow`** | `{prefix}/live/` + `{prefix}/objects/{blake3}` + `{prefix}/snapshots/` | Full set | BLAKE3 + pointer object per present chunk |

```toml
[[volumes]]
name = "disk0"
# ...
storage = "cow"       # enable snapshots
# storage = "legacy"  # default: max performance, no snapshots
```

Mode is locked in `meta.json` on first write. Changing `cow` → `legacy` is refused. Upgrade **`legacy` → `cow`** only via explicit migrate (below), then set `storage = "cow"` and restart. Open never auto-upgrades.

## COW layout

```text
{family}/live/meta.json
{family}/live/chunks/{016x}          # thin binary pointers (BLAKE3)
{family}/objects/{blake3hex}         # immutable chunk payloads (post-compression bytes)
{family}/snapshots/{id}/manifest.json
{family}/snapshots/{id}/chunks.bin   # compact present-chunk index
```

Snapshot create builds `chunks.bin` from live pointers (metadata only — not a full data copy). Later writes allocate new objects and update live pointers; snapshot objects stay immutable.

## Commands

```bash
# Create (refuses if FullFeature sessions exist unless --force)
iscsi-s3-ctl volume snapshot create disk0 --name before-upgrade
iscsi-s3-ctl volume snapshot create disk0 --force   # crash-consistent best-effort

iscsi-s3-ctl volume snapshot list disk0

iscsi-s3-ctl volume snapshot restore disk0 before-upgrade [--force]
iscsi-s3-ctl volume snapshot clone disk0 before-upgrade --to disk1
iscsi-s3-ctl volume snapshot delete disk0 before-upgrade [--force]
```

| Command | Behavior |
|---------|----------|
| `create` | Quiesce check → invalidate volume cache → write `chunks.bin` + `manifest.json` |
| `list` | Headers only (`manifest.json`) |
| `delete` | Remove snapshot objects + synchronous GC of unreferenced `objects/` |
| `restore` | Quiesce → replace live pointers from `chunks.bin` (grow-only if snap larger) |
| `clone` | Dest must be empty COW with matching geometry; shares/copies objects as needed |

On a legacy volume:

```text
volume disk0 uses storage=legacy; set storage="cow" (and migrate) to use snapshots
```

## Quiesce and MPIO

- Prefer **no active sessions** for create/restore (`volume sessions` / `volume disconnect`).
- `--force` allows crash-consistent best-effort while sessions remain.
- Multi-instance (Path A peers): set **`cache.max_bytes = 0` on all peers**; run snapshot ops from one admin node. S3 is the source of truth; create lists live pointers from S3, not from cache.

Snapshot ops invalidate the local volume’s cache entries. That does **not** invalidate peer process caches — keep peer caches off.

## Migrate legacy → cow

While the volume is still open as **legacy**:

```bash
iscsi-s3-ctl volume migrate-cow disk0 --force
```

Then set `storage = "cow"` in config and restart. After migrate, flat chunk keys are removed; live pointers + object pool remain.

## Interactions

- **`volume wipe`**: deletes live pointers/chunks only; snapshots kept.
- **`volume.grow`**: live meta only; existing snapshots keep recorded capacity.
- **`volume.copy`**: still a full logical copy (unchanged in v1).
- Clone of a second IQN still requires the destination volume in config (restart to add IQN).

## Related

- Config: [`configuration.md`](configuration.md) (`volumes[].storage`)
- Admin table: [`admin-ctl.md`](admin-ctl.md)
- Architecture: [`../developers/architecture.md`](../developers/architecture.md)
- Cache / MPIO: [`../developers/cache.md`](../developers/cache.md), [`mpio.md`](mpio.md)
