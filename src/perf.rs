//! Optional background analyser that names the current performance bottleneck.
//!
//! Diagnostic only: it does not change cache sizes, flush policy, or iSCSI settings.
//! At most one component is lit (`iscsi_s3_bottleneck` = 1). A new winner must lead
//! for two consecutive windows before the gauge flips.

use crate::cache::ChunkCache;
use crate::config::WriteCacheMode;
use crate::metrics::{LatencyAccum, Metrics, PerfCounters};
use crate::volume::VolumeStore;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Sample interval. A new winner must lead for two of these before the gauge flips.
pub const WINDOW: Duration = Duration::from_secs(15);

const MIN_OPS: u64 = 8;
const WRITE_CACHE_FILL: f64 = 0.85;
const WRITE_CACHE_MIN_DRAIN: f64 = 0.10;
const READ_CACHE_FILL: f64 = 0.90;
const READ_CACHE_MISS: f64 = 0.25;
const S3_AVG_SECS: f64 = 0.050;
const ISCSI_AVG_SECS: f64 = 0.020;
const S3_SHARE_OF_SCSI: f64 = 0.40;
const MAJORITY: f64 = 0.50;
const HYSTERESIS_WINDOWS: u8 = 2;

/// Primary bottleneck component. Label values match [`crate::metrics::BOTTLENECK_COMPONENTS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    S3Read,
    S3Write,
    ReadCache,
    WriteCache,
    WriteBuffer,
    Iscsi,
}

impl Component {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3Read => "s3_read",
            Self::S3Write => "s3_write",
            Self::ReadCache => "read_cache",
            Self::WriteCache => "write_cache",
            Self::WriteBuffer => "write_buffer",
            Self::Iscsi => "iscsi",
        }
    }
}

/// One analysis window. Latency fields are deltas, not process totals.
#[derive(Debug, Clone, Copy)]
pub struct WindowSample {
    pub scsi_read: LatencyAccum,
    pub scsi_write: LatencyAccum,
    pub scsi_flush: LatencyAccum,
    pub s3_get: LatencyAccum,
    pub s3_put: LatencyAccum,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub read_cache_used: u64,
    pub read_cache_max: u64,
    pub write_cache_dirty: u64,
    pub write_cache_max: u64,
    /// Dirty bytes at the start of the window (budgeted volumes only).
    pub write_cache_dirty_prev: u64,
    pub write_buffer_bytes: u64,
    pub write_buffer_max: u64,
    pub write_buffer_bytes_prev: u64,
}

impl WindowSample {
    fn from_counters(
        counters: PerfCounters,
        read_cache_used: u64,
        read_cache_max: u64,
        write_cache_dirty: u64,
        write_cache_max: u64,
        write_cache_dirty_prev: u64,
        write_buffer_bytes: u64,
        write_buffer_max: u64,
        write_buffer_bytes_prev: u64,
    ) -> Self {
        Self {
            scsi_read: counters.scsi_read,
            scsi_write: counters.scsi_write,
            scsi_flush: counters.scsi_flush,
            s3_get: counters.s3_get,
            s3_put: counters.s3_put,
            cache_hits: counters.cache_hits,
            cache_misses: counters.cache_misses,
            read_cache_used,
            read_cache_max,
            write_cache_dirty,
            write_cache_max,
            write_cache_dirty_prev,
            write_buffer_bytes,
            write_buffer_max,
            write_buffer_bytes_prev,
        }
    }
}

/// Requires the same winner for [`HYSTERESIS_WINDOWS`] consecutive samples.
#[derive(Debug, Default)]
pub struct BottleneckTracker {
    pending: Option<Component>,
    streak: u8,
    lit: Option<Component>,
}

impl BottleneckTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lit(&self) -> Option<Component> {
        self.lit
    }

    /// `winner` is this window's raw decision. Returns the component to export.
    pub fn observe(&mut self, winner: Option<Component>) -> Option<Component> {
        if winner == self.lit {
            self.pending = None;
            self.streak = 0;
            return self.lit;
        }
        if self.pending == winner {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.pending = winner;
            self.streak = 1;
        }
        if self.streak >= HYSTERESIS_WINDOWS {
            self.lit = winner;
            self.pending = None;
            self.streak = 0;
        }
        self.lit
    }
}

/// Pick the primary bottleneck for one window, or `None` if idle or healthy.
pub fn decide(sample: &WindowSample) -> Option<Component> {
    let scsi_ops = sample.scsi_read.count + sample.scsi_write.count + sample.scsi_flush.count;
    let s3_ops = sample.s3_get.count + sample.s3_put.count;
    if scsi_ops < MIN_OPS && s3_ops < MIN_OPS {
        return None;
    }
    if write_buffer_pinned(sample) {
        return Some(Component::WriteBuffer);
    }
    if write_cache_pinned(sample) {
        return Some(Component::WriteCache);
    }
    if read_cache_thrashing(sample) {
        return Some(Component::ReadCache);
    }

    let mut best: Option<(Component, f64)> = None;
    if let Some(score) = s3_write_score(sample) {
        consider(&mut best, Component::S3Write, score);
    }
    if let Some(score) = s3_read_score(sample) {
        consider(&mut best, Component::S3Read, score);
    }
    if let Some(score) = iscsi_score(sample) {
        consider(&mut best, Component::Iscsi, score);
    }
    best.map(|(component, _)| component)
}

fn consider(best: &mut Option<(Component, f64)>, component: Component, score: f64) {
    match best {
        Some((_, prev)) if score <= *prev => {}
        _ => *best = Some((component, score)),
    }
}

fn write_buffer_pinned(sample: &WindowSample) -> bool {
    if sample.write_buffer_max == 0 || sample.scsi_write.count == 0 {
        return false;
    }
    let fill = sample.write_buffer_bytes as f64 / sample.write_buffer_max as f64;
    if fill < WRITE_CACHE_FILL {
        return false;
    }
    let prev = sample.write_buffer_bytes_prev;
    if prev == 0 || sample.write_buffer_bytes >= prev {
        return true;
    }
    let dropped = (prev - sample.write_buffer_bytes) as f64 / prev as f64;
    dropped < WRITE_CACHE_MIN_DRAIN
}

fn write_cache_pinned(sample: &WindowSample) -> bool {
    if sample.write_cache_max == 0 || sample.scsi_write.count == 0 {
        return false;
    }
    let fill = sample.write_cache_dirty as f64 / sample.write_cache_max as f64;
    if fill < WRITE_CACHE_FILL {
        return false;
    }
    let prev = sample.write_cache_dirty_prev;
    if prev == 0 || sample.write_cache_dirty >= prev {
        return true;
    }
    let dropped = (prev - sample.write_cache_dirty) as f64 / prev as f64;
    dropped < WRITE_CACHE_MIN_DRAIN
}

fn read_cache_thrashing(sample: &WindowSample) -> bool {
    if sample.read_cache_max == 0 {
        return false;
    }
    let lookups = sample.cache_hits.saturating_add(sample.cache_misses);
    if lookups < MIN_OPS {
        return false;
    }
    let fill = sample.read_cache_used as f64 / sample.read_cache_max as f64;
    let miss = sample.cache_misses as f64 / lookups as f64;
    fill >= READ_CACHE_FILL && miss >= READ_CACHE_MISS
}

fn s3_read_score(sample: &WindowSample) -> Option<f64> {
    if sample.s3_get.count < MIN_OPS || sample.scsi_read.count < MIN_OPS {
        return None;
    }
    if sample.s3_get.avg_secs() < S3_AVG_SECS {
        return None;
    }
    let get = sample.s3_get.total_secs();
    let scsi = sample.scsi_read.total_secs();
    if scsi <= 0.0 || get < scsi * MAJORITY {
        return None;
    }
    Some(get)
}

fn s3_write_score(sample: &WindowSample) -> Option<f64> {
    let scsi_ops = sample.scsi_write.count + sample.scsi_flush.count;
    if sample.s3_put.count < MIN_OPS || scsi_ops < MIN_OPS {
        return None;
    }
    if sample.s3_put.avg_secs() < S3_AVG_SECS {
        return None;
    }
    let put = sample.s3_put.total_secs();
    let scsi = sample.scsi_write.total_secs() + sample.scsi_flush.total_secs();
    if scsi <= 0.0 || put < scsi * MAJORITY {
        return None;
    }
    Some(put)
}

fn iscsi_score(sample: &WindowSample) -> Option<f64> {
    let count = sample.scsi_read.count + sample.scsi_write.count + sample.scsi_flush.count;
    if count < MIN_OPS {
        return None;
    }
    let scsi = sample.scsi_read.total_secs()
        + sample.scsi_write.total_secs()
        + sample.scsi_flush.total_secs();
    if scsi <= 0.0 || scsi / (count as f64) < ISCSI_AVG_SECS {
        return None;
    }
    let s3 = sample.s3_get.total_secs() + sample.s3_put.total_secs();
    if s3 >= scsi * S3_SHARE_OF_SCSI {
        return None;
    }
    Some(scsi - s3)
}

/// Queued plus in-flight buffer bytes, and budget, summed over volumes with a buffer.
fn write_buffer_fill(stores: &[Arc<VolumeStore>]) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut max = 0u64;
    for store in stores {
        let budget = store.buffer_max_bytes();
        if budget == 0 {
            continue;
        }
        bytes = bytes.saturating_add(store.buffer_bytes());
        max = max.saturating_add(budget);
    }
    (bytes, max)
}

/// Dirty bytes and budget summed over volumes that have a finite write-cache limit.
fn write_cache_fill(stores: &[Arc<VolumeStore>]) -> (u64, u64) {
    let mut dirty = 0u64;
    let mut max = 0u64;
    for store in stores {
        let budget = store.max_bytes();
        if budget == 0 || store.mode() == WriteCacheMode::None {
            continue;
        }
        dirty = dirty.saturating_add(store.dirty_bytes());
        max = max.saturating_add(budget);
    }
    (dirty, max)
}

pub fn spawn_perf_analyser(
    metrics: Arc<Metrics>,
    cache: Arc<ChunkCache>,
    stores: Vec<Arc<VolumeStore>>,
    metrics_enabled: bool,
) -> Result<(), String> {
    if !metrics_enabled {
        warn!(
            "performance optimiser enabled but Prometheus metrics are disabled; Grafana will not see iscsi_s3_bottleneck"
        );
    }
    info!(
        window_secs = WINDOW.as_secs(),
        "performance optimiser started"
    );

    thread::Builder::new()
        .name("iscsi-s3-perf".into())
        .spawn(move || {
            let mut prev = metrics.perf_counters();
            let (mut prev_dirty, _) = write_cache_fill(&stores);
            let (mut prev_buffer, _) = write_buffer_fill(&stores);
            let mut tracker = BottleneckTracker::new();
            let mut shown = tracker.lit();
            loop {
                thread::sleep(WINDOW);
                let now = metrics.perf_counters();
                let counters = now.delta(prev);
                prev = now;
                let (dirty, write_max) = write_cache_fill(&stores);
                let (buffer_bytes, buffer_max) = write_buffer_fill(&stores);
                let (used, _) = cache.stats();
                let sample = WindowSample::from_counters(
                    counters,
                    used,
                    cache.max_bytes(),
                    dirty,
                    write_max,
                    prev_dirty,
                    buffer_bytes,
                    buffer_max,
                    prev_buffer,
                );
                prev_dirty = dirty;
                prev_buffer = buffer_bytes;

                let winner = decide(&sample);
                let next = tracker.observe(winner);
                metrics.set_bottleneck(next.map(Component::as_str));
                if next != shown {
                    match next {
                        Some(component) => {
                            info!(component = component.as_str(), "performance bottleneck")
                        }
                        None => info!("performance bottleneck cleared"),
                    }
                    shown = next;
                }
                debug!(
                    winner = winner.map(Component::as_str).unwrap_or("none"),
                    shown = next.map(Component::as_str).unwrap_or("none"),
                    scsi_read_ms = sample.scsi_read.avg_secs() * 1_000.0,
                    scsi_write_ms = sample.scsi_write.avg_secs() * 1_000.0,
                    s3_get_ms = sample.s3_get.avg_secs() * 1_000.0,
                    s3_put_ms = sample.s3_put.avg_secs() * 1_000.0,
                    read_cache_used = sample.read_cache_used,
                    read_cache_max = sample.read_cache_max,
                    write_cache_dirty = sample.write_cache_dirty,
                    write_cache_max = sample.write_cache_max,
                    write_buffer_bytes = sample.write_buffer_bytes,
                    write_buffer_max = sample.write_buffer_max,
                    "performance window"
                );
            }
        })
        .map_err(|e| format!("spawn performance optimiser: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::VolumeLabels;
    use std::time::Duration;

    fn lat(count: u64, avg_ms: f64) -> LatencyAccum {
        LatencyAccum {
            count,
            sum_us: (count as f64 * avg_ms * 1_000.0).round() as u64,
        }
    }

    fn idle() -> WindowSample {
        WindowSample {
            scsi_read: LatencyAccum::default(),
            scsi_write: LatencyAccum::default(),
            scsi_flush: LatencyAccum::default(),
            s3_get: LatencyAccum::default(),
            s3_put: LatencyAccum::default(),
            cache_hits: 0,
            cache_misses: 0,
            read_cache_used: 0,
            read_cache_max: 0,
            write_cache_dirty: 0,
            write_cache_max: 0,
            write_cache_dirty_prev: 0,
            write_buffer_bytes: 0,
            write_buffer_max: 0,
            write_buffer_bytes_prev: 0,
        }
    }

    #[test]
    fn idle_is_not_a_bottleneck() {
        assert_eq!(decide(&idle()), None);
    }

    #[test]
    fn slow_gets_are_s3_read() {
        let mut sample = idle();
        sample.scsi_read = lat(10, 80.0);
        sample.s3_get = lat(10, 70.0);
        assert_eq!(decide(&sample), Some(Component::S3Read));
    }

    #[test]
    fn slow_puts_are_s3_write() {
        let mut sample = idle();
        sample.scsi_write = lat(10, 90.0);
        sample.s3_put = lat(10, 80.0);
        assert_eq!(decide(&sample), Some(Component::S3Write));
    }

    #[test]
    fn few_s3_ops_are_ignored() {
        let mut sample = idle();
        sample.scsi_read = lat(10, 80.0);
        sample.s3_get = lat(3, 200.0);
        assert_eq!(decide(&sample), None);
    }

    #[test]
    fn pinned_write_buffer_wins_over_write_cache() {
        let mut sample = idle();
        sample.scsi_write = lat(10, 90.0);
        sample.s3_put = lat(10, 80.0);
        sample.write_cache_max = 1000;
        sample.write_cache_dirty = 900;
        sample.write_cache_dirty_prev = 900;
        sample.write_buffer_max = 1000;
        sample.write_buffer_bytes = 900;
        sample.write_buffer_bytes_prev = 900;
        assert_eq!(decide(&sample), Some(Component::WriteBuffer));
    }

    #[test]
    fn pinned_write_cache_wins_over_slow_s3() {
        let mut sample = idle();
        sample.scsi_write = lat(10, 90.0);
        sample.s3_put = lat(10, 80.0);
        sample.write_cache_max = 1000;
        sample.write_cache_dirty = 900;
        sample.write_cache_dirty_prev = 900;
        assert_eq!(decide(&sample), Some(Component::WriteCache));
    }

    #[test]
    fn draining_write_cache_is_not_the_bottleneck() {
        let mut sample = idle();
        sample.scsi_write = lat(10, 10.0);
        sample.write_cache_max = 1000;
        sample.write_cache_dirty = 850;
        sample.write_cache_dirty_prev = 1000;
        assert_eq!(decide(&sample), None);
    }

    #[test]
    fn full_read_cache_with_misses() {
        let mut sample = idle();
        sample.scsi_read = lat(10, 5.0);
        sample.cache_hits = 6;
        sample.cache_misses = 4;
        sample.read_cache_max = 1000;
        sample.read_cache_used = 950;
        assert_eq!(decide(&sample), Some(Component::ReadCache));
    }

    #[test]
    fn high_scsi_with_little_s3_is_iscsi() {
        let mut sample = idle();
        sample.scsi_read = lat(10, 40.0);
        sample.s3_get = lat(10, 5.0);
        assert_eq!(decide(&sample), Some(Component::Iscsi));
    }

    #[test]
    fn equal_s3_time_prefers_writes() {
        let mut sample = idle();
        sample.scsi_read = lat(10, 80.0);
        sample.scsi_write = lat(10, 80.0);
        sample.s3_get = lat(10, 70.0);
        sample.s3_put = lat(10, 70.0);
        assert_eq!(decide(&sample), Some(Component::S3Write));
    }

    #[test]
    fn hysteresis_waits_for_two_windows() {
        let mut tracker = BottleneckTracker::new();
        assert_eq!(tracker.observe(Some(Component::S3Read)), None);
        assert_eq!(
            tracker.observe(Some(Component::S3Read)),
            Some(Component::S3Read)
        );
        assert_eq!(tracker.observe(None), Some(Component::S3Read));
        assert_eq!(tracker.observe(None), None);
    }

    #[test]
    fn gauge_lights_one_component() {
        let metrics = Metrics::new().unwrap();
        let text = metrics.gather_text();
        assert!(text.contains("iscsi_s3_bottleneck"));
        assert!(text.contains("component=\"s3_read\""));
        metrics.set_bottleneck(Some("s3_read"));
        let text = metrics.gather_text();
        assert!(
            text.contains("iscsi_s3_bottleneck{component=\"s3_read\"} 1"),
            "{text}"
        );
        assert!(text.contains("iscsi_s3_bottleneck{component=\"iscsi\"} 0"));
        metrics.set_bottleneck(None);
        let text = metrics.gather_text();
        assert!(!text.contains("iscsi_s3_bottleneck{component=\"s3_read\"} 1"));
    }

    #[test]
    fn observe_feeds_perf_counters() {
        let metrics = Metrics::new().unwrap();
        let labels = VolumeLabels::new("disk0", "iqn.test:disk0");
        let started = std::time::Instant::now() - Duration::from_millis(40);
        metrics.observe_scsi(&labels, "read", 512, started, true);
        metrics.observe_s3(&labels, "get", 4096, started, true);
        metrics.observe_cache(&labels, false);
        let c = metrics.perf_counters();
        assert_eq!(c.scsi_read.count, 1);
        assert_eq!(c.s3_get.count, 1);
        assert!(c.s3_get.sum_us >= 40_000);
        assert_eq!(c.cache_misses, 1);
        assert_eq!(c.cache_hits, 0);
    }
}
