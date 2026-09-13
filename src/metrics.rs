//! Prometheus metrics registry and HTTP scrape endpoint.

use parking_lot::Mutex;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tiny_http::{Header, Response, Server};
use tracing::{error, info};

/// Labels shared by all per-volume series.
#[derive(Debug, Clone)]
pub struct VolumeLabels {
    pub volume: String,
    pub iqn: String,
}

impl VolumeLabels {
    pub fn new(volume: impl Into<String>, iqn: impl Into<String>) -> Self {
        Self {
            volume: volume.into(),
            iqn: iqn.into(),
        }
    }
}

/// Prometheus label values for `iscsi_s3_bottleneck`.
pub const BOTTLENECK_COMPONENTS: &[&str] = &[
    "s3_read",
    "s3_write",
    "read_cache",
    "write_cache",
    "write_buffer",
    "iscsi",
];

/// Cumulative latency sample (count and sum of microseconds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyAccum {
    pub count: u64,
    pub sum_us: u64,
}

impl LatencyAccum {
    pub fn avg_secs(self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            (self.sum_us as f64 / self.count as f64) / 1_000_000.0
        }
    }

    pub fn total_secs(self) -> f64 {
        self.sum_us as f64 / 1_000_000.0
    }

    fn saturating_delta(self, earlier: Self) -> Self {
        Self {
            count: self.count.saturating_sub(earlier.count),
            sum_us: self.sum_us.saturating_sub(earlier.sum_us),
        }
    }
}

/// Process-wide counters the bottleneck analyser diffs between windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerfCounters {
    pub scsi_read: LatencyAccum,
    pub scsi_write: LatencyAccum,
    pub scsi_flush: LatencyAccum,
    pub s3_get: LatencyAccum,
    pub s3_put: LatencyAccum,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl PerfCounters {
    pub fn delta(self, earlier: Self) -> Self {
        Self {
            scsi_read: self.scsi_read.saturating_delta(earlier.scsi_read),
            scsi_write: self.scsi_write.saturating_delta(earlier.scsi_write),
            scsi_flush: self.scsi_flush.saturating_delta(earlier.scsi_flush),
            s3_get: self.s3_get.saturating_delta(earlier.s3_get),
            s3_put: self.s3_put.saturating_delta(earlier.s3_put),
            cache_hits: self.cache_hits.saturating_sub(earlier.cache_hits),
            cache_misses: self.cache_misses.saturating_sub(earlier.cache_misses),
        }
    }
}

struct AtomicLatency {
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl AtomicLatency {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    fn record(&self, elapsed_secs: f64) {
        let us = (elapsed_secs * 1_000_000.0).round().max(0.0) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    fn snapshot(&self) -> LatencyAccum {
        LatencyAccum {
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
        }
    }
}

struct PerfAccumulators {
    scsi_read: AtomicLatency,
    scsi_write: AtomicLatency,
    scsi_flush: AtomicLatency,
    s3_get: AtomicLatency,
    s3_put: AtomicLatency,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

impl PerfAccumulators {
    fn new() -> Self {
        Self {
            scsi_read: AtomicLatency::new(),
            scsi_write: AtomicLatency::new(),
            scsi_flush: AtomicLatency::new(),
            s3_get: AtomicLatency::new(),
            s3_put: AtomicLatency::new(),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> PerfCounters {
        PerfCounters {
            scsi_read: self.scsi_read.snapshot(),
            scsi_write: self.scsi_write.snapshot(),
            scsi_flush: self.scsi_flush.snapshot(),
            s3_get: self.s3_get.snapshot(),
            s3_put: self.s3_put.snapshot(),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
        }
    }
}

/// Process-wide metrics for iscsi-s3.
pub struct Metrics {
    registry: Registry,
    scsi_ops: IntCounterVec,
    scsi_bytes: IntCounterVec,
    scsi_duration: HistogramVec,
    s3_ops: IntCounterVec,
    s3_bytes: IntCounterVec,
    s3_duration: HistogramVec,
    cache_ops: IntCounterVec,
    cache_bytes: IntGauge,
    cache_entries: IntGauge,
    write_cache_bytes: IntGauge,
    write_buffer_bytes: IntGauge,
    write_buffer_chunks: IntGauge,
    volume_capacity_bytes: IntGaugeVec,
    iscsi_connections: IntGauge,
    iscsi_sessions: IntGauge,
    iscsi_sessions_active: IntGaugeVec,
    iscsi_sessions_total: IntCounterVec,
    bottleneck: IntGaugeVec,
    perf: PerfAccumulators,
}

impl Metrics {
    pub fn new() -> Result<Arc<Self>, prometheus::Error> {
        let registry = Registry::new();

        let scsi_ops = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_scsi_ops_total",
                "SCSI read/write/flush operations completed",
            ),
            &["volume", "iqn", "op"],
        )?;
        let scsi_bytes = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_scsi_bytes_total",
                "Bytes transferred via SCSI read/write",
            ),
            &["volume", "iqn", "op"],
        )?;
        let scsi_duration = HistogramVec::new(
            HistogramOpts::new(
                "iscsi_s3_scsi_op_duration_seconds",
                "SCSI operation latency (includes S3/cache)",
            )
            .buckets(latency_buckets()),
            &["volume", "iqn", "op"],
        )?;

        let s3_ops = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_s3_ops_total",
                "S3 operations (get, put, get_pointer, get_meta, put_meta, delete)",
            ),
            &["volume", "iqn", "op", "result"],
        )?;
        let s3_bytes = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_s3_bytes_total",
                "Bytes transferred to/from S3 chunk objects",
            ),
            &["volume", "iqn", "op"],
        )?;
        let s3_duration = HistogramVec::new(
            HistogramOpts::new(
                "iscsi_s3_s3_op_duration_seconds",
                "S3 operation latency; op=get_pointer is the COW pointer-file GetObject",
            )
            .buckets(latency_buckets()),
            &["volume", "iqn", "op"],
        )?;

        let cache_ops = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_cache_ops_total",
                "Chunk cache lookups (hit or miss)",
            ),
            &["volume", "iqn", "result"],
        )?;
        let cache_bytes = IntGauge::new(
            "iscsi_s3_cache_bytes",
            "Current chunk cache memory usage in bytes",
        )?;
        let cache_entries = IntGauge::new(
            "iscsi_s3_cache_entries",
            "Current number of cached chunks",
        )?;
        let write_cache_bytes = IntGauge::new(
            "iscsi_s3_write_cache_bytes",
            "Current dirty write-cache bytes, summed across volumes",
        )?;
        let write_buffer_bytes = IntGauge::new(
            "iscsi_s3_write_buffer_bytes",
            "Queued plus in-flight write-buffer snapshot bytes, summed across volumes",
        )?;
        let write_buffer_chunks = IntGauge::new(
            "iscsi_s3_write_buffer_chunks",
            "Queued plus in-flight write-buffer snapshots, summed across volumes",
        )?;

        let volume_capacity_bytes = IntGaugeVec::new(
            Opts::new(
                "iscsi_s3_volume_capacity_bytes",
                "Configured/effective volume capacity in bytes",
            ),
            &["volume", "iqn"],
        )?;

        let iscsi_connections = IntGauge::new(
            "iscsi_s3_iscsi_connections",
            "Active TCP connections on the iSCSI portal",
        )?;
        let iscsi_sessions = IntGauge::new(
            "iscsi_s3_iscsi_sessions",
            "Active FullFeature iSCSI sessions (all volumes)",
        )?;
        let iscsi_sessions_active = IntGaugeVec::new(
            Opts::new(
                "iscsi_s3_iscsi_sessions_active",
                "Active FullFeature iSCSI sessions per volume",
            ),
            &["volume", "iqn"],
        )?;
        let iscsi_sessions_total = IntCounterVec::new(
            Opts::new(
                "iscsi_s3_iscsi_sessions_total",
                "FullFeature iSCSI sessions opened (cumulative)",
            ),
            &["volume", "iqn"],
        )?;
        let bottleneck = IntGaugeVec::new(
            Opts::new(
                "iscsi_s3_bottleneck",
                "1 if this component is the current primary performance bottleneck, else 0",
            ),
            &["component"],
        )?;

        registry.register(Box::new(scsi_ops.clone()))?;
        registry.register(Box::new(scsi_bytes.clone()))?;
        registry.register(Box::new(scsi_duration.clone()))?;
        registry.register(Box::new(s3_ops.clone()))?;
        registry.register(Box::new(s3_bytes.clone()))?;
        registry.register(Box::new(s3_duration.clone()))?;
        registry.register(Box::new(cache_ops.clone()))?;
        registry.register(Box::new(cache_bytes.clone()))?;
        registry.register(Box::new(cache_entries.clone()))?;
        registry.register(Box::new(write_cache_bytes.clone()))?;
        registry.register(Box::new(write_buffer_bytes.clone()))?;
        registry.register(Box::new(write_buffer_chunks.clone()))?;
        registry.register(Box::new(volume_capacity_bytes.clone()))?;
        registry.register(Box::new(iscsi_connections.clone()))?;
        registry.register(Box::new(iscsi_sessions.clone()))?;
        registry.register(Box::new(iscsi_sessions_active.clone()))?;
        registry.register(Box::new(iscsi_sessions_total.clone()))?;
        registry.register(Box::new(bottleneck.clone()))?;
        for component in BOTTLENECK_COMPONENTS {
            bottleneck.with_label_values(&[component]).set(0);
        }

        Ok(Arc::new(Self {
            registry,
            scsi_ops,
            scsi_bytes,
            scsi_duration,
            s3_ops,
            s3_bytes,
            s3_duration,
            cache_ops,
            cache_bytes,
            cache_entries,
            write_cache_bytes,
            write_buffer_bytes,
            write_buffer_chunks,
            volume_capacity_bytes,
            iscsi_connections,
            iscsi_sessions,
            iscsi_sessions_active,
            iscsi_sessions_total,
            bottleneck,
            perf: PerfAccumulators::new(),
        }))
    }

    pub fn perf_counters(&self) -> PerfCounters {
        self.perf.snapshot()
    }

    /// Set exactly one component to 1, or all to 0 when `active` is `None`.
    pub fn set_bottleneck(&self, active: Option<&str>) {
        for component in BOTTLENECK_COMPONENTS {
            let value = if Some(*component) == active { 1 } else { 0 };
            self.bottleneck.with_label_values(&[component]).set(value);
        }
    }

    pub fn set_volume_capacity(&self, labels: &VolumeLabels, capacity: u64) {
        self.volume_capacity_bytes
            .with_label_values(&[&labels.volume, &labels.iqn])
            .set(capacity as i64);
    }

    pub fn observe_scsi(
        &self,
        labels: &VolumeLabels,
        op: &str,
        bytes: u64,
        started: Instant,
        ok: bool,
    ) {
        let elapsed = started.elapsed().as_secs_f64();
        self.scsi_ops
            .with_label_values(&[&labels.volume, &labels.iqn, op])
            .inc();
        self.scsi_duration
            .with_label_values(&[&labels.volume, &labels.iqn, op])
            .observe(elapsed);
        if ok && bytes > 0 {
            self.scsi_bytes
                .with_label_values(&[&labels.volume, &labels.iqn, op])
                .inc_by(bytes);
        }
        match op {
            "read" => self.perf.scsi_read.record(elapsed),
            "write" => self.perf.scsi_write.record(elapsed),
            "flush" => self.perf.scsi_flush.record(elapsed),
            _ => {}
        }
    }

    pub fn observe_s3(
        &self,
        labels: &VolumeLabels,
        op: &str,
        bytes: u64,
        started: Instant,
        ok: bool,
    ) {
        let result = if ok { "ok" } else { "error" };
        let elapsed = started.elapsed().as_secs_f64();
        self.s3_ops
            .with_label_values(&[&labels.volume, &labels.iqn, op, result])
            .inc();
        self.s3_duration
            .with_label_values(&[&labels.volume, &labels.iqn, op])
            .observe(elapsed);
        if ok && bytes > 0 && (op == "get" || op == "put") {
            self.s3_bytes
                .with_label_values(&[&labels.volume, &labels.iqn, op])
                .inc_by(bytes);
        }
        match op {
            "get" => self.perf.s3_get.record(elapsed),
            "put" => self.perf.s3_put.record(elapsed),
            _ => {}
        }
    }

    pub fn observe_cache(&self, labels: &VolumeLabels, hit: bool) {
        let result = if hit { "hit" } else { "miss" };
        self.cache_ops
            .with_label_values(&[&labels.volume, &labels.iqn, result])
            .inc();
        if hit {
            self.perf.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.perf.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn set_cache_stats(&self, bytes: u64, entries: usize) {
        self.cache_bytes.set(bytes as i64);
        self.cache_entries.set(entries as i64);
    }

    pub fn set_write_cache_bytes(&self, bytes: u64) {
        self.write_cache_bytes.set(bytes as i64);
    }

    pub fn set_write_buffer_stats(&self, bytes: u64, chunks: usize) {
        self.write_buffer_bytes.set(bytes as i64);
        self.write_buffer_chunks.set(chunks as i64);
    }

    pub fn set_iscsi_gauges(&self, connections: usize, sessions: usize) {
        self.iscsi_connections.set(connections as i64);
        self.iscsi_sessions.set(sessions as i64);
    }

    pub fn session_started(&self, labels: &VolumeLabels) {
        self.iscsi_sessions_total
            .with_label_values(&[&labels.volume, &labels.iqn])
            .inc();
        self.iscsi_sessions_active
            .with_label_values(&[&labels.volume, &labels.iqn])
            .inc();
    }

    pub fn session_ended(&self, labels: &VolumeLabels) {
        self.iscsi_sessions_active
            .with_label_values(&[&labels.volume, &labels.iqn])
            .dec();
    }

    pub fn gather_text(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
            error!(error = %e, "failed to encode prometheus metrics");
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }
}

fn latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
        60.0,
    ]
}

/// Maps IQN → volume labels for session hooks; also tracks live sessions for admin.
pub struct SessionMetricsSink {
    metrics: Arc<Metrics>,
    by_iqn: HashMap<String, VolumeLabels>,
    live: Mutex<HashMap<(String, String), LiveSessionInner>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSession {
    pub volume: String,
    pub target_iqn: String,
    pub initiator_iqn: String,
    pub peer: String,
    pub started_secs_ago: u64,
}

#[derive(Debug, Clone)]
struct LiveSessionInner {
    volume: String,
    target_iqn: String,
    initiator_iqn: String,
    peer: String,
    started: Instant,
}

impl SessionMetricsSink {
    pub fn new(metrics: Arc<Metrics>, volumes: impl IntoIterator<Item = VolumeLabels>) -> Arc<Self> {
        let by_iqn: HashMap<String, VolumeLabels> = volumes
            .into_iter()
            .map(|v| (v.iqn.clone(), v))
            .collect();
        for labels in by_iqn.values() {
            metrics
                .iscsi_sessions_active
                .with_label_values(&[&labels.volume, &labels.iqn])
                .set(0);
        }
        Arc::new(Self {
            metrics,
            by_iqn,
            live: Mutex::new(HashMap::new()),
        })
    }

    pub fn list_sessions(&self, volume_or_iqn: Option<&str>) -> Vec<LiveSession> {
        let now = Instant::now();
        let live = self.live.lock();
        let mut out: Vec<LiveSession> = live
            .values()
            .filter(|s| match volume_or_iqn {
                None => true,
                Some(sel) => s.volume == sel || s.target_iqn == sel,
            })
            .map(|s| LiveSession {
                volume: s.volume.clone(),
                target_iqn: s.target_iqn.clone(),
                initiator_iqn: s.initiator_iqn.clone(),
                peer: s.peer.clone(),
                started_secs_ago: now.duration_since(s.started).as_secs(),
            })
            .collect();
        out.sort_by(|a, b| {
            a.volume
                .cmp(&b.volume)
                .then(a.peer.cmp(&b.peer))
                .then(a.initiator_iqn.cmp(&b.initiator_iqn))
        });
        out
    }
}

impl iscsi_target::SessionEventSink for SessionMetricsSink {
    fn on_session_start(&self, event: &iscsi_target::SessionEvent) {
        if let Some(labels) = self.by_iqn.get(&event.target_iqn) {
            self.metrics.session_started(labels);
            self.live.lock().insert(
                (event.target_iqn.clone(), event.peer.clone()),
                LiveSessionInner {
                    volume: labels.volume.clone(),
                    target_iqn: event.target_iqn.clone(),
                    initiator_iqn: event.initiator_iqn.clone(),
                    peer: event.peer.clone(),
                    started: Instant::now(),
                },
            );
        }
    }

    fn on_session_end(&self, event: &iscsi_target::SessionEvent) {
        if let Some(labels) = self.by_iqn.get(&event.target_iqn) {
            self.metrics.session_ended(labels);
            self.live
                .lock()
                .remove(&(event.target_iqn.clone(), event.peer.clone()));
        }
    }
}

/// Serve Prometheus text on `GET /metrics` (and `GET /healthz`).
pub fn spawn_metrics_server(
    bind: String,
    metrics: Arc<Metrics>,
    gauge_source: impl Fn() -> (usize, usize) + Send + Sync + 'static,
    cache_stats: impl Fn() -> (u64, usize) + Send + Sync + 'static,
    write_cache_bytes: impl Fn() -> u64 + Send + Sync + 'static,
    write_buffer_stats: impl Fn() -> (u64, usize) + Send + Sync + 'static,
) -> Result<(), String> {
    let server = Server::http(&bind).map_err(|e| format!("metrics bind {bind}: {e}"))?;
    info!(%bind, "prometheus metrics listening");

    thread::Builder::new()
        .name("iscsi-s3-metrics".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                let path = request.url().split('?').next().unwrap_or("/");
                match path {
                    "/metrics" => {
                        let (conns, sessions) = gauge_source();
                        metrics.set_iscsi_gauges(conns, sessions);
                        let (bytes, entries) = cache_stats();
                        metrics.set_cache_stats(bytes, entries);
                        metrics.set_write_cache_bytes(write_cache_bytes());
                        let (buf_bytes, buf_chunks) = write_buffer_stats();
                        metrics.set_write_buffer_stats(buf_bytes, buf_chunks);
                        let body = metrics.gather_text();
                        let response = Response::from_string(body).with_header(
                            Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/plain; version=0.0.4; charset=utf-8"[..],
                            )
                            .unwrap(),
                        );
                        let _ = request.respond(response);
                    }
                    "/healthz" | "/health" => {
                        let _ = request.respond(Response::from_string("ok\n"));
                    }
                    _ => {
                        let _ = request.respond(
                            Response::from_string("not found\n").with_status_code(404),
                        );
                    }
                }
            }
        })
        .map_err(|e| format!("spawn metrics thread: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_cache_bytes_gauge() {
        let metrics = Metrics::new().unwrap();
        metrics.set_write_cache_bytes(4096);
        let text = metrics.gather_text();
        assert!(
            text.contains("iscsi_s3_write_cache_bytes 4096"),
            "{text}"
        );
        metrics.set_write_buffer_stats(8192, 2);
        let text = metrics.gather_text();
        assert!(text.contains("iscsi_s3_write_buffer_bytes 8192"), "{text}");
        assert!(text.contains("iscsi_s3_write_buffer_chunks 2"), "{text}");
    }
}
