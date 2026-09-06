//! Prometheus metrics registry and HTTP scrape endpoint.

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use std::collections::HashMap;
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
    volume_capacity_bytes: IntGaugeVec,
    iscsi_connections: IntGauge,
    iscsi_sessions: IntGauge,
    iscsi_sessions_active: IntGaugeVec,
    iscsi_sessions_total: IntCounterVec,
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
                "S3 GetObject/PutObject (and meta) operations",
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
                "S3 operation latency",
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

        registry.register(Box::new(scsi_ops.clone()))?;
        registry.register(Box::new(scsi_bytes.clone()))?;
        registry.register(Box::new(scsi_duration.clone()))?;
        registry.register(Box::new(s3_ops.clone()))?;
        registry.register(Box::new(s3_bytes.clone()))?;
        registry.register(Box::new(s3_duration.clone()))?;
        registry.register(Box::new(cache_ops.clone()))?;
        registry.register(Box::new(cache_bytes.clone()))?;
        registry.register(Box::new(cache_entries.clone()))?;
        registry.register(Box::new(volume_capacity_bytes.clone()))?;
        registry.register(Box::new(iscsi_connections.clone()))?;
        registry.register(Box::new(iscsi_sessions.clone()))?;
        registry.register(Box::new(iscsi_sessions_active.clone()))?;
        registry.register(Box::new(iscsi_sessions_total.clone()))?;

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
            volume_capacity_bytes,
            iscsi_connections,
            iscsi_sessions,
            iscsi_sessions_active,
            iscsi_sessions_total,
        }))
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
    }

    pub fn observe_cache(&self, labels: &VolumeLabels, hit: bool) {
        let result = if hit { "hit" } else { "miss" };
        self.cache_ops
            .with_label_values(&[&labels.volume, &labels.iqn, result])
            .inc();
    }

    pub fn set_cache_stats(&self, bytes: u64, entries: usize) {
        self.cache_bytes.set(bytes as i64);
        self.cache_entries.set(entries as i64);
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

/// Maps IQN → volume labels for session hooks.
pub struct SessionMetricsSink {
    metrics: Arc<Metrics>,
    by_iqn: HashMap<String, VolumeLabels>,
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
        Arc::new(Self { metrics, by_iqn })
    }
}

impl iscsi_target::SessionEventSink for SessionMetricsSink {
    fn on_session_start(&self, target_iqn: &str) {
        if let Some(labels) = self.by_iqn.get(target_iqn) {
            self.metrics.session_started(labels);
        }
    }

    fn on_session_end(&self, target_iqn: &str) {
        if let Some(labels) = self.by_iqn.get(target_iqn) {
            self.metrics.session_ended(labels);
        }
    }
}

/// Serve Prometheus text on `GET /metrics` (and `GET /healthz`).
pub fn spawn_metrics_server(
    bind: String,
    metrics: Arc<Metrics>,
    gauge_source: impl Fn() -> (usize, usize) + Send + Sync + 'static,
    cache_stats: impl Fn() -> (u64, usize) + Send + Sync + 'static,
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
