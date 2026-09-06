//! S3-backed chunk store with per-volume meta.json (legacy flat or COW).

use super::{check_range, BlockStore, StoreError};
use crate::compression::{decode_chunk, encode_chunk, Compression};
use crate::metrics::{Metrics, VolumeLabels};
use crate::snapshot::{
    decode_pointer, encode_pointer, hash_hex, hash_object_bytes, read_chunks_bin, require_cow,
    write_chunks_bin, ChunkRef, SnapshotManifest, SnapshotState, HASH_LEN,
};
use crate::storage_mode::StorageMode;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Cursor;
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

/// List present chunk indices under `{chunks_prefix}` (must end with `/chunks/` or similar).
pub fn list_chunk_indices(
    client: Client,
    runtime: Handle,
    bucket: String,
    chunks_prefix: &str,
) -> Result<Vec<u64>, StoreError> {
    let chunks_prefix = if chunks_prefix.ends_with('/') {
        chunks_prefix.to_string()
    } else {
        format!("{chunks_prefix}/")
    };
    run_on_runtime(
        &runtime,
        async move {
            let mut indices = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let mut req = client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&chunks_prefix);
                if let Some(t) = token.take() {
                    req = req.continuation_token(t);
                }
                let out = req
                    .send()
                    .await
                    .map_err(|e| StoreError::S3(e.to_string()))?;
                for obj in out.contents() {
                    let key = obj.key().unwrap_or("");
                    if let Some(idx) = parse_chunk_index_from_key(&chunks_prefix, key) {
                        indices.push(idx);
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
            indices.sort_unstable();
            indices.dedup();
            Ok(indices)
        },
        Duration::from_secs(300),
    )
}

fn parse_chunk_index_from_key(chunks_prefix: &str, key: &str) -> Option<u64> {
    let name = key.strip_prefix(chunks_prefix)?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    u64::from_str_radix(name, 16).ok()
}

/// Lightweight connectivity check used by admin `health`.
pub fn head_bucket(client: Client, runtime: Handle, bucket: String) -> Result<(), StoreError> {
    run_on_runtime(
        &runtime,
        async move {
            client
                .head_bucket()
                .bucket(&bucket)
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        },
        Duration::from_secs(15),
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
    /// Locked layout; missing in old meta → `legacy`.
    #[serde(default)]
    pub storage: StorageMode,
}

pub struct S3StoreConfig {
    pub bucket: String,
    pub prefix: String,
    pub capacity: u64,
    pub block_size: u32,
    pub chunk_size: u64,
    pub compression: Compression,
    pub storage: StorageMode,
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
    storage: Mutex<StorageMode>,
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

        let prefix = cfg.prefix.trim_matches('/').to_string();
        let store = Self {
            client,
            bucket: cfg.bucket,
            prefix,
            block_size: cfg.block_size,
            chunk_size: cfg.chunk_size,
            compression: cfg.compression,
            storage: Mutex::new(cfg.storage),
            capacity: AtomicU64::new(cfg.capacity),
            runtime,
            locks,
            labels: cfg.labels,
            metrics: cfg.metrics,
        };

        let (mode, existing) = store.probe_layout(cfg.storage)?;
        *store.storage.lock() = mode;
        let effective = store.resolve_meta(cfg.capacity, existing)?;
        store.capacity.store(effective, Ordering::SeqCst);
        Ok(store)
    }

    pub fn storage_mode(&self) -> StorageMode {
        *self.storage.lock()
    }

    pub fn family_prefix(&self) -> &str {
        &self.prefix
    }

    pub fn bucket_name(&self) -> &str {
        &self.bucket
    }

    fn probe_layout(
        &self,
        config: StorageMode,
    ) -> Result<(StorageMode, Option<VolumeMeta>), StoreError> {
        let live = self.get_meta_at(&self.live_meta_key())?;
        let flat = self.get_meta_at(&self.flat_meta_key())?;
        match (live, flat) {
            (Some(m), _) => {
                if m.storage != StorageMode::Cow {
                    return Err(StoreError::Meta(
                        "live/meta.json present but storage is not cow".into(),
                    ));
                }
                if config != StorageMode::Cow {
                    return Err(StoreError::GeometryMismatch(
                        "volume meta is storage=cow; set volumes[].storage = \"cow\"".into(),
                    ));
                }
                Ok((StorageMode::Cow, Some(m)))
            }
            (None, Some(m)) => {
                if m.storage == StorageMode::Cow {
                    return Err(StoreError::Meta(
                        "flat meta.json claims storage=cow; expected live/ layout".into(),
                    ));
                }
                if config == StorageMode::Cow {
                    return Err(StoreError::GeometryMismatch(
                        "volume is storage=legacy on S3; run `iscsi-s3-ctl volume migrate-cow` \
                         then set storage=\"cow\" and restart"
                            .into(),
                    ));
                }
                Ok((StorageMode::Legacy, Some(m)))
            }
            (None, None) => Ok((config, None)),
        }
    }

    fn flat_meta_key(&self) -> String {
        format!("{}/meta.json", self.prefix)
    }

    fn live_meta_key(&self) -> String {
        format!("{}/live/meta.json", self.prefix)
    }

    fn meta_key(&self) -> String {
        match self.storage_mode() {
            StorageMode::Legacy => self.flat_meta_key(),
            StorageMode::Cow => self.live_meta_key(),
        }
    }

    fn chunks_prefix(&self) -> String {
        match self.storage_mode() {
            StorageMode::Legacy => format!("{}/chunks/", self.prefix),
            StorageMode::Cow => format!("{}/live/chunks/", self.prefix),
        }
    }

    fn chunk_key(&self, index: u64) -> String {
        format!("{}{index:016x}", self.chunks_prefix())
    }

    fn object_key(&self, hash: &[u8; HASH_LEN]) -> String {
        format!("{}/objects/{}", self.prefix, hash_hex(hash))
    }

    fn snapshot_prefix(&self, id: &str) -> String {
        format!("{}/snapshots/{}", self.prefix, id)
    }

    fn snapshot_manifest_key(&self, id: &str) -> String {
        format!("{}/manifest.json", self.snapshot_prefix(id))
    }

    fn snapshot_chunks_key(&self, id: &str) -> String {
        format!("{}/chunks.bin", self.snapshot_prefix(id))
    }

    fn stripe(&self, chunk_idx: u64) -> &Mutex<()> {
        &self.locks[(chunk_idx as usize) % LOCK_STRIPES]
    }

    fn delete_object_key(&self, key: String) -> Result<(), StoreError> {
        let started = Instant::now();
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let result = self.run_async(async move {
            client
                .delete_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        });
        self.metrics
            .observe_s3(&self.labels, "delete", 0, started, result.is_ok());
        result
    }

    fn delete_chunk_object(&self, index: u64) -> Result<(), StoreError> {
        self.delete_object_key(self.chunk_key(index))
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

    fn run_async_long<F, T>(&self, fut: F, timeout: Duration) -> Result<T, StoreError>
    where
        F: std::future::Future<Output = Result<T, StoreError>> + Send + 'static,
        T: Send + 'static,
    {
        run_on_runtime(&self.runtime, fut, timeout)
    }

    fn resolve_meta(
        &self,
        config_capacity: u64,
        existing: Option<VolumeMeta>,
    ) -> Result<u64, StoreError> {
        let mode = self.storage_mode();
        let (effective, to_write) = plan_capacity(
            existing.as_ref(),
            config_capacity,
            self.chunk_size,
            self.block_size,
            self.compression,
            mode,
        )?;
        if let Some(meta) = to_write {
            self.put_meta(&meta)?;
            if existing.is_none() {
                tracing::info!(
                    prefix = %self.prefix,
                    capacity = effective,
                    storage = mode.as_str(),
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

    fn get_meta_at(&self, key: &str) -> Result<Option<VolumeMeta>, StoreError> {
        let started = Instant::now();
        let key = key.to_string();
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

    fn get_bytes(&self, key: String) -> Result<Option<(Vec<u8>, Option<String>)>, StoreError> {
        let started = Instant::now();
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
                    let etag = out.e_tag().map(|s| s.to_string());
                    let bytes = out
                        .body
                        .collect()
                        .await
                        .map_err(|e| StoreError::S3(e.to_string()))?
                        .into_bytes()
                        .to_vec();
                    Ok(Some((bytes, etag)))
                }
                Err(e) => {
                    let msg = e.to_string();
                    if matches!(&e, SdkError::ServiceError(se) if matches!(se.err(), GetObjectError::NoSuchKey(_)))
                        || msg.contains("NoSuchKey")
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
        let bytes = result
            .as_ref()
            .ok()
            .and_then(|o| o.as_ref())
            .map(|(v, _)| v.len() as u64)
            .unwrap_or(0);
        self.metrics
            .observe_s3(&self.labels, "get", bytes, started, result.is_ok());
        result
    }

    fn put_bytes(
        &self,
        key: String,
        body: Vec<u8>,
        content_type: Option<&str>,
        if_match: Option<&str>,
    ) -> Result<(), StoreError> {
        let started = Instant::now();
        let bytes = body.len() as u64;
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let if_match = if_match.map(|s| s.to_string());
        let content_type = content_type.map(|s| s.to_string());
        let result = self.run_async(async move {
            let mut req = client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(ByteStream::from(body));
            if let Some(ct) = content_type {
                req = req.content_type(ct);
            }
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

    fn get_chunk(&self, index: u64) -> Result<(Vec<u8>, Option<String>), StoreError> {
        let chunk_size = self.chunk_size as usize;
        match self.storage_mode() {
            StorageMode::Legacy => {
                let key = self.chunk_key(index);
                match self.get_bytes(key)? {
                    Some((bytes, etag)) => {
                        let plain = decode_chunk(self.compression, &bytes, chunk_size)?;
                        Ok((plain, etag))
                    }
                    None => Ok((vec![0u8; chunk_size], None)),
                }
            }
            StorageMode::Cow => {
                let ptr_key = self.chunk_key(index);
                match self.get_bytes(ptr_key)? {
                    None => Ok((vec![0u8; chunk_size], None)),
                    Some((ptr_bytes, etag)) => {
                        let hash = decode_pointer(&ptr_bytes)?;
                        let obj_key = self.object_key(&hash);
                        let Some((body, _)) = self.get_bytes(obj_key)? else {
                            return Err(StoreError::Other(format!(
                                "missing cow object for chunk {index}"
                            )));
                        };
                        let plain = decode_chunk(self.compression, &body, chunk_size)?;
                        Ok((plain, etag))
                    }
                }
            }
        }
    }

    fn put_chunk(
        &self,
        index: u64,
        data: &[u8],
        if_match: Option<&str>,
    ) -> Result<(), StoreError> {
        if data.len() as u64 != self.chunk_size {
            return Err(StoreError::Other(format!(
                "put_chunk length {} != chunk_size {}",
                data.len(),
                self.chunk_size
            )));
        }
        match self.storage_mode() {
            StorageMode::Legacy => {
                let key = self.chunk_key(index);
                let body = encode_chunk(self.compression, data)?;
                self.put_bytes(key, body, None, if_match)
            }
            StorageMode::Cow => {
                let body = encode_chunk(self.compression, data)?;
                let hash = hash_object_bytes(&body);
                let obj_key = self.object_key(&hash);
                // Immutable content-addressed put (overwrite of identical bytes is fine).
                self.put_bytes(obj_key, body, None, None)?;
                let ptr = encode_pointer(&hash);
                self.put_bytes(self.chunk_key(index), ptr, None, if_match)
            }
        }
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

    fn read_pointer_hash(&self, index: u64) -> Result<Option<[u8; HASH_LEN]>, StoreError> {
        match self.get_bytes(self.chunk_key(index))? {
            None => Ok(None),
            Some((bytes, _)) => Ok(Some(decode_pointer(&bytes)?)),
        }
    }

    fn put_pointer(&self, index: u64, hash: &[u8; HASH_LEN]) -> Result<(), StoreError> {
        self.put_bytes(self.chunk_key(index), encode_pointer(hash), None, None)
    }

    fn collect_live_refs(&self) -> Result<Vec<ChunkRef>, StoreError> {
        require_cow_store(self)?;
        let indices = self.present_chunks()?;
        let mut refs = Vec::with_capacity(indices.len());
        for idx in indices {
            if let Some(hash) = self.read_pointer_hash(idx)? {
                refs.push(ChunkRef { index: idx, hash });
            }
        }
        refs.sort_by_key(|r| r.index);
        Ok(refs)
    }

    /// Create a crash-consistent snapshot from live pointers (caller must quiesce).
    pub fn snapshot_create(
        &self,
        id: &str,
        source_volume: &str,
    ) -> Result<SnapshotManifest, StoreError> {
        require_cow_store(self)?;
        validate_snapshot_id(id)?;
        if self.get_bytes(self.snapshot_manifest_key(id))?.is_some() {
            return Err(StoreError::Other(format!(
                "snapshot {id} already exists"
            )));
        }

        let mut manifest = SnapshotManifest::new_creating(
            id.to_string(),
            source_volume.to_string(),
            self.capacity(),
            self.chunk_size,
            self.block_size,
            self.compression,
        );
        self.put_bytes(
            self.snapshot_manifest_key(id),
            serde_json::to_vec_pretty(&manifest).map_err(|e| StoreError::Meta(e.to_string()))?,
            Some("application/json"),
            None,
        )?;

        let refs = self.collect_live_refs()?;
        let mut bin = Vec::new();
        write_chunks_bin(&mut bin, &refs)?;
        self.put_bytes(self.snapshot_chunks_key(id), bin, None, None)?;

        manifest.chunk_count = refs.len() as u64;
        manifest.state = SnapshotState::Ready;
        self.put_bytes(
            self.snapshot_manifest_key(id),
            serde_json::to_vec_pretty(&manifest).map_err(|e| StoreError::Meta(e.to_string()))?,
            Some("application/json"),
            None,
        )?;
        Ok(manifest)
    }

    pub fn snapshot_list(&self) -> Result<Vec<SnapshotManifest>, StoreError> {
        require_cow_store(self)?;
        let list_prefix = format!("{}/snapshots/", self.prefix);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let keys: Vec<String> = self.run_async_long(
            async move {
                let mut keys = Vec::new();
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
                        let key = obj.key().unwrap_or("").to_string();
                        if key.ends_with("/manifest.json") {
                            keys.push(key);
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
                Ok(keys)
            },
            Duration::from_secs(300),
        )?;

        let mut out = Vec::new();
        for key in keys {
            if let Some((bytes, _)) = self.get_bytes(key)? {
                let m: SnapshotManifest = serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Meta(e.to_string()))?;
                out.push(m);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub fn snapshot_get(&self, id: &str) -> Result<SnapshotManifest, StoreError> {
        require_cow_store(self)?;
        let Some((bytes, _)) = self.get_bytes(self.snapshot_manifest_key(id))? else {
            return Err(StoreError::Other(format!("snapshot {id} not found")));
        };
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Meta(e.to_string()))
    }

    fn load_snapshot_refs(&self, id: &str) -> Result<Vec<ChunkRef>, StoreError> {
        let Some((bytes, _)) = self.get_bytes(self.snapshot_chunks_key(id))? else {
            return Err(StoreError::Other(format!(
                "snapshot {id} chunks.bin missing"
            )));
        };
        read_chunks_bin(&mut Cursor::new(bytes))
    }

    /// Delete snapshot metadata and GC unreferenced objects.
    pub fn snapshot_delete(&self, id: &str) -> Result<GcStats, StoreError> {
        require_cow_store(self)?;
        let _ = self.snapshot_get(id)?;
        self.delete_object_key(self.snapshot_chunks_key(id))?;
        self.delete_object_key(self.snapshot_manifest_key(id))?;
        self.gc_unreferenced_objects()
    }

    /// Replace live pointers from a snapshot (grow-only capacity if needed).
    pub fn snapshot_restore(&self, id: &str) -> Result<RestoreStats, StoreError> {
        require_cow_store(self)?;
        let manifest = self.snapshot_get(id)?;
        if manifest.state != SnapshotState::Ready {
            return Err(StoreError::Other(format!(
                "snapshot {id} is not ready ({:?})",
                manifest.state
            )));
        }
        if manifest.chunk_size != self.chunk_size || manifest.block_size != self.block_size {
            return Err(StoreError::GeometryMismatch(
                "snapshot geometry does not match live volume".into(),
            ));
        }
        if manifest.compression != self.compression {
            return Err(StoreError::GeometryMismatch(
                "snapshot compression does not match live volume".into(),
            ));
        }
        if self.capacity() < manifest.capacity_bytes {
            self.set_capacity(manifest.capacity_bytes)?;
        }

        let refs = self.load_snapshot_refs(id)?;
        let snap_set: HashSet<u64> = refs.iter().map(|r| r.index).collect();
        let live = self.present_chunks()?;
        let mut pointers_written = 0u64;
        let mut pointers_deleted = 0u64;

        for r in &refs {
            self.put_pointer(r.index, &r.hash)?;
            pointers_written += 1;
        }
        for idx in live {
            if !snap_set.contains(&idx) {
                self.delete_chunk_object(idx)?;
                pointers_deleted += 1;
            }
        }
        Ok(RestoreStats {
            pointers_written,
            pointers_deleted,
            capacity: self.capacity(),
        })
    }

    /// Install snapshot pointers into an empty destination COW volume (shared objects).
    pub fn snapshot_clone_into(
        &self,
        dest: &S3ChunkStore,
        id: &str,
    ) -> Result<CloneStats, StoreError> {
        require_cow_store(self)?;
        require_cow_store(dest)?;
        if self.prefix == dest.prefix {
            return Err(StoreError::Other(
                "clone destination must be a different volume prefix".into(),
            ));
        }
        if dest.chunk_size != self.chunk_size
            || dest.block_size != self.block_size
            || dest.compression != self.compression
        {
            return Err(StoreError::GeometryMismatch(
                "clone destination geometry/compression must match snapshot source".into(),
            ));
        }
        let manifest = self.snapshot_get(id)?;
        if manifest.state != SnapshotState::Ready {
            return Err(StoreError::Other(format!(
                "snapshot {id} is not ready"
            )));
        }
        if dest.capacity() < manifest.capacity_bytes {
            dest.set_capacity(manifest.capacity_bytes)?;
        }
        let existing = dest.present_chunks()?;
        if !existing.is_empty() {
            return Err(StoreError::Other(
                "clone destination must be empty (no live chunk pointers)".into(),
            ));
        }

        let refs = self.load_snapshot_refs(id)?;
        // Same family objects/ only if prefixes share a parent — plan uses per-volume family.
        // Clone across volumes: copy missing object bytes into dest family, then write pointers.
        let mut objects_copied = 0u64;
        let mut pointers_written = 0u64;
        for r in &refs {
            let src_key = self.object_key(&r.hash);
            let dst_key = dest.object_key(&r.hash);
            if dest.get_bytes(dst_key.clone())?.is_none() {
                let Some((body, _)) = self.get_bytes(src_key)? else {
                    return Err(StoreError::Other(format!(
                        "missing object for hash {}",
                        hash_hex(&r.hash)
                    )));
                };
                dest.put_bytes(dst_key, body, None, None)?;
                objects_copied += 1;
            }
            dest.put_pointer(r.index, &r.hash)?;
            pointers_written += 1;
        }
        Ok(CloneStats {
            pointers_written,
            objects_copied,
        })
    }

    /// Migrate a live legacy volume to COW layout (objects + pointers + live/meta).
    pub fn migrate_to_cow(&self) -> Result<MigrateStats, StoreError> {
        if self.storage_mode() != StorageMode::Legacy {
            return Err(StoreError::Other(
                "volume is already storage=cow".into(),
            ));
        }
        let indices = list_chunk_indices(
            self.client.clone(),
            self.runtime.clone(),
            self.bucket.clone(),
            &format!("{}/chunks/", self.prefix),
        )?;
        let mut chunks_migrated = 0u64;
        for idx in &indices {
            let key = format!("{}/chunks/{:016x}", self.prefix, idx);
            let Some((body, _)) = self.get_bytes(key.clone())? else {
                continue;
            };
            let hash = hash_object_bytes(&body);
            self.put_bytes(self.object_key(&hash), body, None, None)?;
            let ptr_key = format!("{}/live/chunks/{:016x}", self.prefix, idx);
            self.put_bytes(ptr_key, encode_pointer(&hash), None, None)?;
            chunks_migrated += 1;
        }

        let meta = VolumeMeta {
            version: META_VERSION,
            capacity_bytes: self.capacity(),
            chunk_size: self.chunk_size,
            block_size: self.block_size,
            compression: self.compression,
            storage: StorageMode::Cow,
        };
        // Write live meta, flip mode, then remove flat layout.
        let live_key = self.live_meta_key();
        let body =
            serde_json::to_vec_pretty(&meta).map_err(|e| StoreError::Meta(e.to_string()))?;
        self.put_bytes(live_key, body, Some("application/json"), None)?;
        *self.storage.lock() = StorageMode::Cow;

        for idx in &indices {
            let key = format!("{}/chunks/{:016x}", self.prefix, idx);
            let _ = self.delete_object_key(key);
        }
        let _ = self.delete_object_key(self.flat_meta_key());

        Ok(MigrateStats {
            chunks_migrated,
            note: "set volumes[].storage = \"cow\" before next restart".into(),
        })
    }

    pub fn gc_unreferenced_objects(&self) -> Result<GcStats, StoreError> {
        require_cow_store(self)?;
        let mut referenced: HashSet<[u8; HASH_LEN]> = HashSet::new();
        for idx in self.present_chunks()? {
            if let Some(h) = self.read_pointer_hash(idx)? {
                referenced.insert(h);
            }
        }
        for m in self.snapshot_list()? {
            if let Ok(refs) = self.load_snapshot_refs(&m.id) {
                for r in refs {
                    referenced.insert(r.hash);
                }
            }
        }

        let obj_prefix = format!("{}/objects/", self.prefix);
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let keys: Vec<String> = self.run_async_long(
            async move {
                let mut keys = Vec::new();
                let mut token: Option<String> = None;
                loop {
                    let mut req = client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&obj_prefix);
                    if let Some(t) = token.take() {
                        req = req.continuation_token(t);
                    }
                    let out = req
                        .send()
                        .await
                        .map_err(|e| StoreError::S3(e.to_string()))?;
                    for obj in out.contents() {
                        if let Some(k) = obj.key() {
                            keys.push(k.to_string());
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
                Ok(keys)
            },
            Duration::from_secs(300),
        )?;

        let mut deleted = 0u64;
        for key in keys {
            let Some(hex) = key.rsplit('/').next() else {
                continue;
            };
            let Ok(hash) = crate::snapshot::parse_hash_hex(hex) else {
                continue;
            };
            if !referenced.contains(&hash) {
                self.delete_object_key(key)?;
                deleted += 1;
            }
        }
        Ok(GcStats {
            objects_deleted: deleted,
            objects_referenced: referenced.len() as u64,
        })
    }
}

fn require_cow_store(store: &S3ChunkStore) -> Result<(), StoreError> {
    require_cow(store.storage_mode(), store.labels.volume.as_str())
        .map_err(StoreError::Other)
}

fn validate_snapshot_id(id: &str) -> Result<(), StoreError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(StoreError::Other(
            "snapshot id must be 1-128 chars of [A-Za-z0-9._-]".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct GcStats {
    pub objects_deleted: u64,
    pub objects_referenced: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreStats {
    pub pointers_written: u64,
    pub pointers_deleted: u64,
    pub capacity: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloneStats {
    pub pointers_written: u64,
    pub objects_copied: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MigrateStats {
    pub chunks_migrated: u64,
    pub note: String,
}

/// Decide effective capacity and whether meta must be rewritten.
pub fn plan_capacity(
    existing: Option<&VolumeMeta>,
    config_capacity: u64,
    chunk_size: u64,
    block_size: u32,
    compression: Compression,
    storage: StorageMode,
) -> Result<(u64, Option<VolumeMeta>), StoreError> {
    match existing {
        None => {
            let meta = VolumeMeta {
                version: META_VERSION,
                capacity_bytes: config_capacity,
                chunk_size,
                block_size,
                compression,
                storage,
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
            if meta.storage != storage {
                return Err(StoreError::GeometryMismatch(format!(
                    "storage config {} != meta {}",
                    storage.as_str(),
                    meta.storage.as_str()
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
            storage: self.storage_mode(),
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

    fn present_chunks(&self) -> Result<Vec<u64>, StoreError> {
        list_chunk_indices(
            self.client.clone(),
            self.runtime.clone(),
            self.bucket.clone(),
            &self.chunks_prefix(),
        )
    }

    fn delete_chunk(&self, index: u64) -> Result<(), StoreError> {
        let _guard = self.stripe(index).lock();
        self.delete_chunk_object(index)
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
            storage: StorageMode::Legacy,
        }
    }

    #[test]
    fn plan_creates_on_missing() {
        let (cap, write) = plan_capacity(
            None,
            8192,
            4096,
            512,
            Compression::None,
            StorageMode::Legacy,
        )
        .unwrap();
        assert_eq!(cap, 8192);
        assert_eq!(write.unwrap().capacity_bytes, 8192);
    }

    #[test]
    fn plan_grows() {
        let m = meta(4096);
        let (cap, write) = plan_capacity(
            Some(&m),
            8192,
            4096,
            512,
            Compression::None,
            StorageMode::Legacy,
        )
        .unwrap();
        assert_eq!(cap, 8192);
        assert!(write.is_some());
    }

    #[test]
    fn plan_refuses_shrink() {
        let m = meta(8192);
        let err = plan_capacity(
            Some(&m),
            4096,
            4096,
            512,
            Compression::None,
            StorageMode::Legacy,
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::ShrinkRefused { .. }));
    }

    #[test]
    fn plan_refuses_geometry_change() {
        let m = meta(8192);
        let err = plan_capacity(
            Some(&m),
            8192,
            8192,
            512,
            Compression::None,
            StorageMode::Legacy,
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::GeometryMismatch(_)));
    }

    #[test]
    fn plan_refuses_compression_change() {
        let m = meta(8192);
        let err = plan_capacity(
            Some(&m),
            8192,
            4096,
            512,
            Compression::Lz4,
            StorageMode::Legacy,
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::GeometryMismatch(_)));
    }

    #[test]
    fn plan_refuses_storage_change() {
        let m = meta(8192);
        let err = plan_capacity(
            Some(&m),
            8192,
            4096,
            512,
            Compression::None,
            StorageMode::Cow,
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::GeometryMismatch(_)));
    }

    #[test]
    fn parse_chunk_index_hex() {
        assert_eq!(
            parse_chunk_index_from_key("disks/a/chunks/", "disks/a/chunks/000000000000000a"),
            Some(10)
        );
        assert_eq!(
            parse_chunk_index_from_key("disks/a/chunks/", "disks/a/chunks/"),
            None
        );
    }
}
