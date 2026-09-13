# Performance bottleneck analyser

Optional diagnostic thread. It does **not** change cache sizes, flush policy, or iSCSI settings. It names the single component most worth chasing and exports that as a Prometheus gauge.

Off by default. Same precedence as other settings: TOML, then `ISCSI_S3_PERFORMANCE_OPTIMISER`, then `--performance-optimiser true|false`. Changing it requires a restart (`reload` reports it as rejected). The series is on the existing metrics endpoint. If metrics are disabled (`--no-metrics` / `metrics.enabled = false`), the thread still logs but Grafana cannot scrape it.

```toml
performance_optimiser = true
```

```bash
export ISCSI_S3_PERFORMANCE_OPTIMISER=true
iscsi-s3 --config config.toml --performance-optimiser true
```

## Grafana

```promql
iscsi_s3_bottleneck
```

Label `component` is one of `s3_read`, `s3_write`, `read_cache`, `write_cache`, `iscsi`. The value is `1` if that component is the current primary bottleneck, otherwise `0`. At most one series is `1`. Idle or healthy windows leave every series at `0`. All five series are registered at start so legends stay stable.

A change of winner is also logged at info (`performance bottleneck` / `performance bottleneck cleared`). Window numbers are debug (`performance window`).

## How a window is scored

The thread samples every **15 seconds**. Counters are process-wide atomics updated on the SCSI, S3, and cache paths (not parsed from the scrape text). Cache fill is read from the read LRU and from each volume’s write-back cache.

A new winner must lead for **two** consecutive windows before the gauge flips.

A window with fewer than **8** SCSI ops and fewer than **8** S3 get/put ops is idle (all zeros). Signals with fewer than 8 ops are ignored.

| Component | Lit when |
|-----------|----------|
| `write_cache` | A write-cache budget is set, dirty fill is at least 85%, SCSI writes happened, and dirty bytes did not drop by at least 10% over the window. Wins over the rows below. Unlimited write cache (`max_bytes = 0`) cannot be “full”. |
| `read_cache` | Read LRU budget is set, fill is at least 90%, and the miss ratio is at least 25% (at least 8 lookups). |
| `s3_write` | At least 8 puts and 8 SCSI write/flush ops, average put at least 50ms, and put time is at least half of SCSI write/flush time. Score is time spent in puts. |
| `s3_read` | Same idea for gets vs SCSI reads. |
| `iscsi` | At least 8 SCSI ops, average SCSI time at least 20ms, and S3 get+put time is under 40% of SCSI time (the leftover is the local path). |

If both S3 rows qualify, the larger time spent wins. A tie prefers `s3_write`, then `s3_read`, then `iscsi`.

Implementation: [`src/perf.rs`](../../src/perf.rs).
