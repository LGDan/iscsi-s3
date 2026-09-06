//! S3-backed chunk store with per-volume meta.json.

use super::{check_range, BlockStore, StoreError};
use crate::compression::{decode_chunk, encode_chunk, Compression};
use crate::metrics::{Metrics, VolumeLabels};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

const META_VERSION: u32 = 1;
const LOCK_STRIPES: usize = 64;

fn run_on_runtime<F, T>(runtime: &Handle, fut: F, timeout: Duration) -> Result<T, StoreError>
where
    F: std::future::Future<Output = Result<T, StoreError>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    runtime.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv_timeout(timeout)
        .map_err(|_| StoreError::S3("timed out waiting for S3 operation".into()))?
}

/// Aggregated object counts/sizes under a volume prefix.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrefixObjectStats {
    pub object_count: u64,
    pub chunk_count: u64,
    pub meta_count: u64,
    pub other_count: u64,
    pub total_bytes: u64,
    pub chunk_bytes: u64,
    pub meta_bytes: u64,
    pub other_bytes: u64,
}

/// List all objects under `{prefix}/` and sum sizes (paginated `ListObjectsV2`).
pub fn list_prefix_stats(
    client: Client,
    runtime: Handle,
    bucket: String,
    prefix: &str,
) -> Result<PrefixObjectStats, StoreError> {
    let prefix = prefix.trim_matches('/').to_string();
    let list_prefix = format!("{prefix}/");
    run_on_runtime(
        &runtime,
        async move {
            let mut stats = PrefixObjectStats::default();
            let mut token: Option<String> = None;
            loop {
                let mut req = client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&list_prefix);
                if let Some(t) = token.take() {
                    req = req.continuation_token(t);
                }
                let out = req
                    .send()
                    .await
                    .map_err(|e| StoreError::S3(e.to_string()))?;
                for obj in out.contents() {
                    let key = obj.key().unwrap_or("");
                    let size = obj.size().unwrap_or(0) as u64;
                    stats.object_count += 1;
                    stats.total_bytes += size;
                    if key.ends_with("/meta.json") || key == format!("{prefix}/meta.json") {
                        stats.meta_count += 1;
                        stats.meta_bytes += size;
                    } else if key.contains("/chunks/") {
                        stats.chunk_count += 1;
                        stats.chunk_bytes += size;
                    } else {
                        stats.other_count += 1;
                        stats.other_bytes += size;
                    }
                }
                if out.is_truncated().unwrap_or(false) {
                    token = out.next_continuation_token().map(|s| s.to_string());
                    if token.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
            Ok(stats)
        },
        Duration::from_secs(300),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMeta {
    pub version: u32,
    pub capacity_bytes: u64,
    pub chunk_size: u64,
    pub block_size: u32,
    /// Locked at first meta write; missing in old meta → `none`.
    #[serde(default)]
    pub compression: Compression,
}

pub struct S3StoreConfig {
    pub bucket: String,
    pub prefix: String,
    pub capacity: u64,
    pub block_size: u32,
    pub chunk_size: u64,
    pub compression: Compression,
    pub labels: VolumeLabels,
    pub metrics: Arc<Metrics>,
}

pub struct S3ChunkStore {
    client: Client,
    bucket: String,
    prefix: String,
    block_size: u32,
    chunk_size: u64,
    compression: Compression,
    capacity: AtomicU64,
    runtime: Handle,
    locks: Arc<[Mutex<()>; LOCK_STRIPES]>,
    labels: VolumeLabels,
    metrics: Arc<Metrics>,
}

impl S3ChunkStore {
    /// Open or create a volume. Applies grow-only rules against `meta.json`.
    pub fn open(client: Client, runtime: Handle, cfg: S3StoreConfig) -> Result<Self, StoreError> {
        let locks: Arc<[Mutex<()>; LOCK_STRIPES]> =
            Arc::new(std::array::from_fn(|_| Mutex::new(())));

        let store = Self {
            client,
            bucket: cfg.bucket,
            prefix: cfg.prefix.trim_matches('/').to_string(),
            block_size: cfg.block_size,
            chunk_size: cfg.chunk_size,
            compression: cfg.compression,
            capacity: AtomicU64::new(cfg.capacity),
            runtime,
            locks,
            labels: cfg.labels,
            metrics: cfg.metrics,
        };

        let effective = store.resolve_meta(cfg.capacity)?;
        store.capacity.store(effective, Ordering::SeqCst);
        Ok(store)
    }

    fn meta_key(&self) -> String {
        format!("{}/meta.json", self.prefix)
    }

    fn chunk_key(&self, index: u64) -> String {
        format!("{}/chunks/{index:016x}", self.prefix)
    }

    fn stripe(&self, chunk_idx: u64) -> &Mutex<()> {
        &self.locks[(chunk_idx as usize) % LOCK_STRIPES]
    }

    /// Run an async S3 op on the Tokio runtime without nesting `block_on` on the
    /// caller thread (iSCSI connection threads are sync and must stay responsive).
    fn run_async<F, T>(&self, fut: F) -> Result<T, StoreError>
    where
        F: std::future::Future<Output = Result<T, StoreError>> + Send + 'static,
        T: Send + 'static,
    {
        run_on_runtime(&self.runtime, fut, Duration::from_secs(60))
    }

    fn resolve_meta(&self, config_capacity: u64) -> Result<u64, StoreError> {
        let existing = self.get_meta()?;
        let (effective, to_write) = plan_capacity(
            existing.as_ref(),
            config_capacity,
            self.chunk_size,
            self.block_size,
            self.compression,
        )?;
        if let Some(meta) = to_write {
            self.put_meta(&meta)?;
            if existing.is_none() {
                tracing::info!(
                    prefix = %self.prefix,
                    capacity = effective,
                    "created volume metadata"
                );
            } else {
                tracing::info!(
                    prefix = %self.prefix,
                    to = effective,
                    "grew volume capacity (existing chunks unchanged)"
                );
            }
        }
        Ok(effective)
    }

    fn get_meta(&self) -> Result<Option<VolumeMeta>, StoreError> {
        let started = Instant::now();
        let key = self.meta_key();
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let result = self.run_async(async move {
            match client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(out) => {
                    let bytes = out
                        .body
                        .collect()
                        .await
                        .map_err(|e| StoreError::S3(e.to_string()))?
                        .into_bytes();
                    let meta: VolumeMeta = serde_json::from_slice(&bytes)
                        .map_err(|e| StoreError::Meta(e.to_string()))?;
                    if meta.version != META_VERSION {
                        return Err(StoreError::Meta(format!(
                            "unsupported meta version {}",
                            meta.version
                        )));
                    }
                    Ok(Some(meta))
                }
                Err(SdkError::ServiceError(se))
                    if matches!(se.err(), GetObjectError::NoSuchKey(_)) =>
                {
                    Ok(None)
                }
                Err(e) => {
                    // MinIO / some S3 clones return 404 as unhandled / generic
                    let msg = e.to_string();
                    if msg.contains("NoSuchKey")
                        || msg.contains("404")
                        || msg.contains("Not Found")
                    {
                        Ok(None)
                    } else {
                        Err(StoreError::S3(msg))
                    }
                }
            }
        });
        self.metrics
            .observe_s3(&self.labels, "get_meta", 0, started, result.is_ok());
        result
    }

    fn put_meta(&self, meta: &VolumeMeta) -> Result<(), StoreError> {
        let started = Instant::now();
        let key = self.meta_key();
        let body =
            serde_json::to_vec_pretty(meta).map_err(|e| StoreError::Meta(e.to_string()))?;
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let result = self.run_async(async move {
            client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(ByteStream::from(body))
                .content_type("application/json")
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        });
        self.metrics
            .observe_s3(&self.labels, "put_meta", 0, started, result.is_ok());
        result
    }

    fn get_chunk(&self, index: u64) -> Result<(Vec<u8>, Option<String>), StoreError> {
        let started = Instant::now();
        let key = self.chunk_key(index);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let chunk_size = self.chunk_size as usize;
        let compression = self.compression;
        let result = self.run_async(async move {
            match client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(out) => {
                    let etag = out.e_tag().map(|s| s.to_string());
                    let bytes = out
                        .body
                        .collect()
                        .await
                        .map_err(|e| StoreError::S3(e.to_string()))?
                        .into_bytes();
                    let plain = decode_chunk(compression, &bytes, chunk_size)?;
                    Ok((plain, etag))
                }
                Err(e) => {
                    let msg = e.to_string();
                    if matches!(&e, SdkError::ServiceError(se) if matches!(se.err(), GetObjectError::NoSuchKey(_)))
                        || msg.contains("NoSuchKey")
                        || msg.contains("404")
                        || msg.contains("Not Found")
                    {
                        Ok((vec![0u8; chunk_size], None))
                    } else {
                        Err(StoreError::S3(msg))
                    }
                }
            }
        });
        let bytes = result
            .as_ref()
            .map(|(v, _)| v.len() as u64)
            .unwrap_or(0);
        self.metrics
            .observe_s3(&self.labels, "get", bytes, started, result.is_ok());
        result
    }

    fn put_chunk(
        &self,
        index: u64,
        data: &[u8],
        if_match: Option<&str>,
    ) -> Result<(), StoreError> {
        let started = Instant::now();
        if data.len() as u64 != self.chunk_size {
            self.metrics
                .observe_s3(&self.labels, "put", 0, started, false);
            return Err(StoreError::Other(format!(
                "put_chunk length {} != chunk_size {}",
                data.len(),
                self.chunk_size
            )));
        }
        let key = self.chunk_key(index);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let body = encode_chunk(self.compression, data)?;
        let bytes = body.len() as u64;
        let if_match = if_match.map(|s| s.to_string());
        let result = self.run_async(async move {
            let mut req = client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(ByteStream::from(body));
            if let Some(etag) = if_match {
                req = req.if_match(etag);
            }
            req.send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        });
        self.metrics
            .observe_s3(&self.labels, "put", bytes, started, result.is_ok());
        result
    }

    fn is_precondition_failed(err: &StoreError) -> bool {
        match err {
            StoreError::S3(msg) => {
                msg.contains("PreconditionFailed")
                    || msg.contains("412")
                    || msg.contains("At least one of the pre-conditions")
            }
            _ => false,
        }
    }
}

/// Decide effective capacity and whether meta must be rewritten.
///
/// Returns `(effective_capacity, Some(new_meta_to_write))` or an error.
pub fn plan_capacity(
    existing: Option<&VolumeMeta>,
    config_capacity: u64,
    chunk_size: u64,
    block_size: u32,
    compression: Compression,
) -> Result<(u64, Option<VolumeMeta>), StoreError> {
    match existing {
        None => {
            let meta = VolumeMeta {
                version: META_VERSION,
                capacity_bytes: config_capacity,
                chunk_size,
                block_size,
                compression,
            };
            Ok((config_capacity, Some(meta)))
        }
        Some(meta) => {
            if meta.chunk_size != chunk_size {
                return Err(StoreError::GeometryMismatch(format!(
                    "chunk_size config {chunk_size} != meta {}",
                    meta.chunk_size
                )));
            }
            if meta.block_size != block_size {
                return Err(StoreError::GeometryMismatch(format!(
                    "block_size config {block_size} != meta {}",
                    meta.block_size
                )));
            }
            if meta.compression != compression {
                return Err(StoreError::GeometryMismatch(format!(
                    "compression config {:?} != meta {:?}",
                    compression.as_str(),
                    meta.compression.as_str()
                )));
            }
            if config_capacity < meta.capacity_bytes {
                return Err(StoreError::ShrinkRefused {
                    current: meta.capacity_bytes,
                    requested: config_capacity,
                });
            }
            if config_capacity > meta.capacity_bytes {
                let grown = VolumeMeta {
                    capacity_bytes: config_capacity,
                    ..meta.clone()
                };
                Ok((config_capacity, Some(grown)))
            } else {
                Ok((meta.capacity_bytes, None))
            }
        }
    }
}

impl BlockStore for S3ChunkStore {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        let capacity = self.capacity.load(Ordering::SeqCst);
        check_range(capacity, offset, buf.len())?;
        buf.fill(0);
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / self.chunk_size;
            let within = (abs % self.chunk_size) as usize;
            let take = ((self.chunk_size as usize) - within).min(buf.len() - done);
            let _guard = self.stripe(chunk_idx).lock();
            let (chunk, _) = self.get_chunk(chunk_idx)?;
            buf[done..done + take].copy_from_slice(&chunk[within..within + take]);
            done += take;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
        let started = std::time::Instant::now();
        let capacity = self.capacity.load(Ordering::SeqCst);
        check_range(capacity, offset, data.len())?;
        let chunk_size = self.chunk_size as usize;
        let mut done = 0usize;
        while done < data.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / self.chunk_size;
            let within = (abs % self.chunk_size) as usize;
            let take = (chunk_size - within).min(data.len() - done);
            let _guard = self.stripe(chunk_idx).lock();
            // Optimistic concurrency for multi-instance: RMW with If-Match ETag.
            const MAX_CAS_ATTEMPTS: usize = 8;
            let mut attempt = 0usize;
            loop {
                attempt += 1;
                let t0 = std::time::Instant::now();
                let (mut chunk, etag) = if within == 0 && take == chunk_size {
                    (vec![0u8; chunk_size], None)
                } else {
                    self.get_chunk(chunk_idx)?
                };
                let t1 = std::time::Instant::now();
                chunk[within..within + take].copy_from_slice(&data[done..done + take]);
                match self.put_chunk(chunk_idx, &chunk, etag.as_deref()) {
                    Ok(()) => {
                        tracing::info!(
                            chunk_idx,
                            attempt,
                            get_ms = t1.duration_since(t0).as_millis(),
                            put_ms = t0.elapsed().as_millis(),
                            "s3 write_at chunk"
                        );
                        break;
                    }
                    Err(e) if Self::is_precondition_failed(&e) && attempt < MAX_CAS_ATTEMPTS => {
                        tracing::warn!(
                            chunk_idx,
                            attempt,
                            "s3 chunk CAS conflict; retrying"
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            done += take;
        }
        tracing::info!(
            offset,
            len = data.len(),
            total_ms = started.elapsed().as_millis(),
            "s3 write_at done"
        );
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::SeqCst)
    }

    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
        if new_capacity % u64::from(self.block_size) != 0 {
            return Err(StoreError::GeometryMismatch(
                "capacity must be multiple of block_size".into(),
            ));
        }
        let current = self.capacity.load(Ordering::SeqCst);
        if new_capacity < current {
            return Err(StoreError::ShrinkRefused {
                current,
                requested: new_capacity,
            });
        }
        if new_capacity == current {
            return Ok(());
        }
        let meta = VolumeMeta {
            version: META_VERSION,
            capacity_bytes: new_capacity,
            chunk_size: self.chunk_size,
            block_size: self.block_size,
            compression: self.compression,
        };
        self.put_meta(&meta)?;
        self.capacity.store(new_capacity, Ordering::SeqCst);
        Ok(())
    }

    fn flush(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn chunk_size(&self) -> u64 {
        self.chunk_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(cap: u64) -> VolumeMeta {
        VolumeMeta {
            version: 1,
            capacity_bytes: cap,
            chunk_size: 4096,
            block_size: 512,
            compression: Compression::None,
        }
    }

    #[test]
    fn plan_creates_on_missing() {
        let (cap, write) =
            plan_capacity(None, 8192, 4096, 512, Compression::None).unwrap();
        assert_eq!(cap, 8192);
        assert_eq!(write.unwrap().capacity_bytes, 8192);
    }

    #[test]
    fn plan_grows() {
        let m = meta(4096);
        let (cap, write) =
            plan_capacity(Some(&m), 8192, 4096, 512, Compression::None).unwrap();
        assert_eq!(cap, 8192);
        assert!(write.is_some());
    }

    #[test]
    fn plan_refuses_shrink() {
        let m = meta(8192);
        let err = plan_capacity(Some(&m), 4096, 4096, 512, Compression::None).unwrap_err();
        assert!(matches!(err, StoreError::ShrinkRefused { .. }));
    }

    #[test]
    fn plan_refuses_geometry_change() {
        let m = meta(8192);
        let err = plan_capacity(Some(&m), 8192, 8192, 512, Compression::None).unwrap_err();
        assert!(matches!(err, StoreError::GeometryMismatch(_)));
    }

    #[test]
    fn plan_refuses_compression_change() {
        let m = meta(8192);
        let err =
            plan_capacity(Some(&m), 8192, 4096, 512, Compression::Lz4).unwrap_err();
        assert!(matches!(err, StoreError::GeometryMismatch(_)));
    }
}
