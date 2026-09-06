//! Local Unix-domain admin control plane for `iscsi-s3-ctl`.

use crate::cache::ChunkCache;
use crate::config::{parse_byte_size, Config};
use crate::store::{list_prefix_stats, BlockStore, PrefixObjectStats};
use iscsi_target::IscsiServer;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize)]
pub struct VolumeSummary {
    pub name: String,
    pub iqn: String,
    pub capacity: u64,
    pub auth: String,
    pub prefix: String,
    pub chunk_size: u64,
    pub compression: String,
}

/// Live store handle used by admin write paths (same Arc as the iSCSI device).
pub struct AdminVolumeHandle {
    pub name: String,
    pub iqn: String,
    pub store: Arc<dyn BlockStore>,
}

pub struct AdminState {
    pub config_path: Option<PathBuf>,
    pub started: Instant,
    pub cache: Arc<ChunkCache>,
    pub server: Arc<IscsiServer>,
    pub s3_client: aws_sdk_s3::Client,
    pub runtime: tokio::runtime::Handle,
    /// Labels / config snapshot updated on safe reload.
    pub snapshot: Mutex<AdminSnapshot>,
    /// Per-volume stores for direct image seeding.
    pub volume_stores: Vec<AdminVolumeHandle>,
}

#[derive(Debug, Clone)]
pub struct AdminSnapshot {
    pub bind: String,
    pub portals: Vec<String>,
    pub instance: Option<String>,
    pub volumes: Vec<VolumeSummary>,
    pub cache_max_bytes: u64,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    pub s3_force_path_style: bool,
}

#[derive(Debug, Deserialize)]
struct AdminRequest {
    op: String,
    #[serde(default)]
    max_bytes: Option<serde_json::Value>,
    /// Volume name or IQN for volume-scoped ops.
    #[serde(default)]
    volume: Option<String>,
    /// Destination volume name or IQN (`volume.copy`).
    #[serde(default)]
    to: Option<String>,
    /// Exact byte length of a following binary body (`volume.write_image`).
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeedImageResult {
    pub bytes_read: u64,
    pub bytes_stored: u64,
    pub chunks_written: u64,
    pub zero_chunks_skipped: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CopyVolumeResult {
    pub chunks_copied: u64,
    pub chunks_deleted: u64,
    pub bytes_copied: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WipeVolumeResult {
    pub chunks_deleted: u64,
}

#[derive(Debug, Serialize)]
pub struct RejectedChange {
    pub field: String,
    pub reason: String,
}

pub fn spawn_admin_server(socket: PathBuf, state: Arc<AdminState>) -> Result<(), String> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create admin socket dir {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("admin bind {}: {e}", socket.display()))?;
    // Restrict to owner when possible.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660));
    }
    info!(socket = %socket.display(), "admin control socket listening");

    thread::Builder::new()
        .name("iscsi-s3-admin".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let state = Arc::clone(&state);
                        if let Err(e) = handle_client(stream, &state) {
                            warn!(error = %e, "admin client error");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "admin accept failed");
                    }
                }
            }
        })
        .map_err(|e| format!("spawn admin thread: {e}"))?;
    Ok(())
}

fn handle_client(stream: UnixStream, state: &AdminState) -> Result<(), String> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let req: AdminRequest =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad request json: {e}"))?;

    if req.op == "volume.write_image" {
        return handle_write_image(state, &req, &mut reader, &stream);
    }

    let response = dispatch(state, &req);
    write_json_line(&stream, &response)
}

fn write_json_line(mut stream: &UnixStream, value: &serde_json::Value) -> Result<(), String> {
    let body = serde_json::to_string(value).map_err(|e| e.to_string())?;
    stream
        .write_all(body.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.write_all(b"\n").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Two-phase image seed: ready JSON, then exact `size` body bytes, then final JSON.
fn handle_write_image(
    state: &AdminState,
    req: &AdminRequest,
    reader: &mut BufReader<&UnixStream>,
    stream: &UnixStream,
) -> Result<(), String> {
    match prepare_write_image(state, req) {
        Ok(prep) => {
            write_json_line(
                stream,
                &ok(json!({
                    "ready": true,
                    "volume": prep.volume_name,
                    "iqn": prep.iqn,
                    "size": prep.size,
                    "capacity": prep.capacity,
                    "chunk_size": prep.chunk_size,
                })),
            )?;
            let result = match seed_image_from_reader(prep.store.as_ref(), reader, prep.size) {
                Ok(r) => ok(json!({
                    "volume": prep.volume_name,
                    "iqn": prep.iqn,
                    "bytes_read": r.bytes_read,
                    "bytes_stored": r.bytes_stored,
                    "chunks_written": r.chunks_written,
                    "zero_chunks_skipped": r.zero_chunks_skipped,
                    "note": "all-zero chunks were skipped (sparse); meta.json may already exist",
                })),
                Err(e) => err(e),
            };
            write_json_line(stream, &result)
        }
        Err(e) => write_json_line(stream, &err(e)),
    }
}

struct WriteImagePrep {
    volume_name: String,
    iqn: String,
    size: u64,
    capacity: u64,
    chunk_size: u64,
    store: Arc<dyn BlockStore>,
}

fn prepare_write_image(state: &AdminState, req: &AdminRequest) -> Result<WriteImagePrep, String> {
    let size = req
        .size
        .ok_or_else(|| "size is required for volume.write_image".to_string())?;
    if size == 0 {
        return Err("size must be greater than 0".into());
    }

    let snap = state.snapshot.lock().clone();
    let vol = find_volume(&snap.volumes, req.volume.as_deref())?.clone();
    let handle = state
        .volume_stores
        .iter()
        .find(|h| h.name == vol.name || h.iqn == vol.iqn)
        .ok_or_else(|| format!("no live store for volume {}", vol.name))?;

    let capacity = handle.store.capacity();
    if size > capacity {
        return Err(format!(
            "image size {size} exceeds volume capacity {capacity}"
        ));
    }

    let bucket = snap
        .s3_bucket
        .clone()
        .ok_or_else(|| "s3.bucket is not configured".to_string())?;
    let stats = list_prefix_stats(
        state.s3_client.clone(),
        state.runtime.clone(),
        bucket,
        &vol.prefix,
    )
    .map_err(|e| e.to_string())?;
    ensure_prefix_empty_for_seed(&stats)?;

    Ok(WriteImagePrep {
        volume_name: vol.name,
        iqn: vol.iqn,
        size,
        capacity,
        chunk_size: handle.store.chunk_size(),
        store: Arc::clone(&handle.store),
    })
}

/// Volume is seedable when it has no chunk (block) objects yet. `meta.json` is allowed.
pub fn ensure_prefix_empty_for_seed(stats: &PrefixObjectStats) -> Result<(), String> {
    if stats.chunk_count > 0 {
        return Err(format!(
            "volume is not empty: {} chunk object(s) already exist under the prefix",
            stats.chunk_count
        ));
    }
    if stats.other_count > 0 {
        return Err(format!(
            "volume prefix has {} unexpected non-chunk/non-meta object(s); refuse to seed",
            stats.other_count
        ));
    }
    Ok(())
}

/// Read `size` bytes and write into `store`, skipping all-zero chunks (sparse).
pub fn seed_image_from_reader(
    store: &dyn BlockStore,
    reader: &mut impl Read,
    size: u64,
) -> Result<SeedImageResult, String> {
    let chunk_size = store.chunk_size();
    if chunk_size == 0 {
        return Err("invalid chunk_size 0".into());
    }
    let capacity = store.capacity();
    if size > capacity {
        return Err(format!(
            "image size {size} exceeds volume capacity {capacity}"
        ));
    }

    let mut buf = vec![0u8; chunk_size as usize];
    let mut remaining = size;
    let mut offset = 0u64;
    let mut chunks_written = 0u64;
    let mut bytes_stored = 0u64;
    let mut zero_chunks_skipped = 0u64;

    while remaining > 0 {
        let n = (remaining as usize).min(buf.len());
        reader
            .read_exact(&mut buf[..n])
            .map_err(|e| format!("read image body at offset {offset}: {e}"))?;
        if buf[..n].iter().all(|&b| b == 0) {
            zero_chunks_skipped += 1;
        } else {
            store
                .write_at(offset, &buf[..n])
                .map_err(|e| format!("write at offset {offset}: {e}"))?;
            chunks_written += 1;
            bytes_stored += n as u64;
        }
        offset += n as u64;
        remaining -= n as u64;
    }

    store
        .flush()
        .map_err(|e| format!("flush after image write: {e}"))?;

    Ok(SeedImageResult {
        bytes_read: size,
        bytes_stored,
        chunks_written,
        zero_chunks_skipped,
    })
}

fn dispatch(state: &AdminState, req: &AdminRequest) -> serde_json::Value {
    match req.op.as_str() {
        "stats" | "cache.status" => ok(stats_json(state)),
        "cache.disable" => {
            state.cache.disable();
            let mut snap = state.snapshot.lock();
            snap.cache_max_bytes = 0;
            ok(cache_json(state))
        }
        "cache.enable" => match parse_optional_max_bytes(req.max_bytes.as_ref()) {
            Ok(mb) => {
                state.cache.enable(mb);
                let mut snap = state.snapshot.lock();
                snap.cache_max_bytes = state.cache.max_bytes();
                ok(cache_json(state))
            }
            Err(e) => err(e),
        },
        "cache.set" => match parse_required_max_bytes(req.max_bytes.as_ref()) {
            Ok(mb) => {
                if mb == 0 {
                    state.cache.disable();
                } else {
                    state.cache.enable(Some(mb));
                }
                let mut snap = state.snapshot.lock();
                snap.cache_max_bytes = state.cache.max_bytes();
                ok(cache_json(state))
            }
            Err(e) => err(e),
        },
        "reload" => match reload(state) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        "volume.s3_stats" => match volume_s3_stats(state, req.volume.as_deref()) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        "volume.list" => ok(volume_list_json(state)),
        "volume.copy" => match volume_copy(state, req.volume.as_deref(), req.to.as_deref()) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        "volume.wipe" => match volume_wipe(state, req.volume.as_deref()) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        other => err(format!("unknown op: {other}")),
    }
}

fn volume_list_json(state: &AdminState) -> serde_json::Value {
    let snap = state.snapshot.lock();
    json!({
        "volumes": snap.volumes,
        "count": snap.volumes.len(),
    })
}

fn find_volume<'a>(
    volumes: &'a [VolumeSummary],
    selector: Option<&str>,
) -> Result<&'a VolumeSummary, String> {
    let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("volume name or IQN is required".into());
    };
    volumes
        .iter()
        .find(|v| v.name == sel || v.iqn == sel)
        .ok_or_else(|| format!("unknown volume {sel:?}"))
}

fn volume_s3_stats(
    state: &AdminState,
    selector: Option<&str>,
) -> Result<serde_json::Value, String> {
    let snap = state.snapshot.lock().clone();
    let vol = find_volume(&snap.volumes, selector)?.clone();
    let bucket = snap
        .s3_bucket
        .clone()
        .ok_or_else(|| "s3.bucket is not configured".to_string())?;
    let stats = crate::store::list_prefix_stats(
        state.s3_client.clone(),
        state.runtime.clone(),
        bucket.clone(),
        &vol.prefix,
    )
    .map_err(|e| e.to_string())?;

    let logical_chunk_bytes = stats.chunk_count.saturating_mul(vol.chunk_size);
    Ok(json!({
        "volume": vol.name,
        "iqn": vol.iqn,
        "bucket": bucket,
        "prefix": vol.prefix,
        "logical_capacity_bytes": vol.capacity,
        "chunk_size": vol.chunk_size,
        "compression": vol.compression,
        "objects": {
            "total": stats.object_count,
            "chunks": stats.chunk_count,
            "meta": stats.meta_count,
            "other": stats.other_count,
        },
        "bytes": {
            "total": stats.total_bytes,
            "chunks": stats.chunk_bytes,
            "meta": stats.meta_bytes,
            "other": stats.other_bytes,
        },
        "logical_chunk_bytes_if_full": logical_chunk_bytes,
        "note": "bytes are on-disk object sizes (compressed when compression != none); sparse unwritten chunks have no object",
    }))
}

fn find_volume_store<'a>(
    stores: &'a [AdminVolumeHandle],
    vol: &VolumeSummary,
) -> Result<&'a AdminVolumeHandle, String> {
    stores
        .iter()
        .find(|h| h.name == vol.name || h.iqn == vol.iqn)
        .ok_or_else(|| format!("no live store for volume {}", vol.name))
}

fn volume_copy(
    state: &AdminState,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<serde_json::Value, String> {
    let snap = state.snapshot.lock().clone();
    let src_sum = find_volume(&snap.volumes, from)?.clone();
    let dst_sum = find_volume(&snap.volumes, to)?.clone();
    if src_sum.name == dst_sum.name {
        return Err("source and destination must be different volumes".into());
    }

    let src = find_volume_store(&state.volume_stores, &src_sum)?;
    let dst = find_volume_store(&state.volume_stores, &dst_sum)?;

    let result = copy_volume(src.store.as_ref(), dst.store.as_ref()).map_err(|e| e.to_string())?;

    info!(
        from = %src_sum.name,
        to = %dst_sum.name,
        chunks_copied = result.chunks_copied,
        chunks_deleted = result.chunks_deleted,
        bytes_copied = result.bytes_copied,
        "volume copy complete"
    );

    Ok(json!({
        "from": {
            "volume": src_sum.name,
            "iqn": src_sum.iqn,
            "prefix": src_sum.prefix,
            "capacity": src_sum.capacity,
            "compression": src_sum.compression,
        },
        "to": {
            "volume": dst_sum.name,
            "iqn": dst_sum.iqn,
            "prefix": dst_sum.prefix,
            "capacity": dst_sum.capacity,
            "compression": dst_sum.compression,
        },
        "chunks_copied": result.chunks_copied,
        "chunks_deleted": result.chunks_deleted,
        "bytes_copied": result.bytes_copied,
        "note": "destination overwritten to match source sparsity; compression may be re-encoded per destination config",
    }))
}

/// Sparse 1:1 copy: copy present source chunks, delete destination-only chunks.
pub fn copy_volume(
    src: &dyn BlockStore,
    dst: &dyn BlockStore,
) -> Result<CopyVolumeResult, String> {
    if src.chunk_size() != dst.chunk_size() {
        return Err(format!(
            "chunk_size mismatch: source {} != destination {}",
            src.chunk_size(),
            dst.chunk_size()
        ));
    }
    if src.block_size() != dst.block_size() {
        return Err(format!(
            "block_size mismatch: source {} != destination {}",
            src.block_size(),
            dst.block_size()
        ));
    }
    if src.capacity() != dst.capacity() {
        return Err(format!(
            "capacity mismatch: source {} != destination {} (1:1 copy requires equal capacity)",
            src.capacity(),
            dst.capacity()
        ));
    }

    let capacity = src.capacity();
    let chunk_size = src.chunk_size();
    if chunk_size == 0 {
        return Err("invalid chunk_size 0".into());
    }

    let src_chunks = src
        .present_chunks()
        .map_err(|e| format!("list source chunks: {e}"))?;
    let dst_chunks = dst
        .present_chunks()
        .map_err(|e| format!("list destination chunks: {e}"))?;

    let src_set: std::collections::HashSet<u64> = src_chunks.iter().copied().collect();
    let mut chunks_copied = 0u64;
    let mut bytes_copied = 0u64;
    let mut buf = vec![0u8; chunk_size as usize];

    for &idx in &src_chunks {
        let offset = idx.checked_mul(chunk_size).ok_or_else(|| {
            format!("chunk index {idx} overflows with chunk_size {chunk_size}")
        })?;
        if offset >= capacity {
            continue;
        }
        let len = ((capacity - offset) as usize).min(buf.len());
        src.read_at(offset, &mut buf[..len])
            .map_err(|e| format!("read source chunk {idx}: {e}"))?;
        dst.write_at(offset, &buf[..len])
            .map_err(|e| format!("write destination chunk {idx}: {e}"))?;
        chunks_copied += 1;
        bytes_copied += len as u64;
    }

    let mut chunks_deleted = 0u64;
    for &idx in &dst_chunks {
        if src_set.contains(&idx) {
            continue;
        }
        dst.delete_chunk(idx)
            .map_err(|e| format!("delete destination chunk {idx}: {e}"))?;
        chunks_deleted += 1;
    }

    dst.flush()
        .map_err(|e| format!("flush destination after copy: {e}"))?;

    Ok(CopyVolumeResult {
        chunks_copied,
        chunks_deleted,
        bytes_copied,
    })
}

fn volume_wipe(state: &AdminState, selector: Option<&str>) -> Result<serde_json::Value, String> {
    let snap = state.snapshot.lock().clone();
    let vol = find_volume(&snap.volumes, selector)?.clone();
    let handle = find_volume_store(&state.volume_stores, &vol)?;
    let result = wipe_volume(handle.store.as_ref()).map_err(|e| e.to_string())?;

    info!(
        volume = %vol.name,
        chunks_deleted = result.chunks_deleted,
        "volume wipe complete"
    );

    Ok(json!({
        "volume": vol.name,
        "iqn": vol.iqn,
        "prefix": vol.prefix,
        "chunks_deleted": result.chunks_deleted,
        "note": "all chunk objects deleted; meta.json kept (volume geometry unchanged)",
    }))
}

/// Delete every present chunk (restore a fully sparse volume). Keeps meta.json.
pub fn wipe_volume(store: &dyn BlockStore) -> Result<WipeVolumeResult, String> {
    let chunks = store
        .present_chunks()
        .map_err(|e| format!("list chunks: {e}"))?;
    let mut chunks_deleted = 0u64;
    for idx in chunks {
        store
            .delete_chunk(idx)
            .map_err(|e| format!("delete chunk {idx}: {e}"))?;
        chunks_deleted += 1;
    }
    store
        .flush()
        .map_err(|e| format!("flush after wipe: {e}"))?;
    Ok(WipeVolumeResult { chunks_deleted })
}

fn ok(data: serde_json::Value) -> serde_json::Value {
    json!({ "ok": true, "data": data })
}

fn err(message: impl Into<String>) -> serde_json::Value {
    json!({ "ok": false, "error": message.into() })
}

fn cache_json(state: &AdminState) -> serde_json::Value {
    let (used, entries) = state.cache.stats();
    json!({
        "enabled": state.cache.is_enabled(),
        "max_bytes": state.cache.max_bytes(),
        "last_nonzero_max_bytes": state.cache.last_nonzero_max_bytes(),
        "used_bytes": used,
        "entries": entries,
    })
}

fn stats_json(state: &AdminState) -> serde_json::Value {
    let snap = state.snapshot.lock().clone();
    let uptime = state.started.elapsed();
    json!({
        "uptime_secs": uptime.as_secs(),
        "uptime": format_duration(uptime),
        "config_path": state.config_path.as_ref().map(|p| p.display().to_string()),
        "bind": snap.bind,
        "portals": snap.portals,
        "instance": snap.instance,
        "volumes": snap.volumes,
        "cache": cache_json(state),
        "iscsi": {
            "connections": state.server.active_connection_count(),
            "sessions": state.server.active_session_count(),
        },
    })
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h}h{m}m{s}s")
}

fn parse_optional_max_bytes(v: Option<&serde_json::Value>) -> Result<Option<u64>, String> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(val) => Ok(Some(parse_max_bytes_value(val)?)),
    }
}

fn parse_required_max_bytes(v: Option<&serde_json::Value>) -> Result<u64, String> {
    let Some(val) = v else {
        return Err("max_bytes is required".into());
    };
    parse_max_bytes_value(val)
}

fn parse_max_bytes_value(val: &serde_json::Value) -> Result<u64, String> {
    match val {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| "max_bytes must be a non-negative integer".into()),
        serde_json::Value::String(s) => parse_byte_size(s),
        _ => Err("max_bytes must be a number or size string".into()),
    }
}

fn reload(state: &AdminState) -> Result<serde_json::Value, String> {
    let path = state
        .config_path
        .as_ref()
        .ok_or_else(|| "no config file path (started without --config); cannot reload".to_string())?;
    let new_cfg = Config::load_for_reload(path).map_err(|e| e.to_string())?;
    let snap = state.snapshot.lock().clone();
    let result = apply_safe_reload(&snap, &new_cfg, &state.cache)?;
    {
        let mut snap = state.snapshot.lock();
        snap.cache_max_bytes = state.cache.max_bytes();
        // Portals/advertise labels for stats only (discovery still uses startup portals).
        snap.portals = if !new_cfg.portals.is_empty() {
            new_cfg.portals.clone()
        } else if let Some(ref a) = new_cfg.advertise {
            vec![a.clone()]
        } else {
            snap.portals.clone()
        };
        snap.instance = new_cfg.instance.clone();
        if result.bind_unchanged {
            // keep bind
        }
    }
    Ok(json!({
        "applied": result.applied,
        "rejected": result.rejected,
        "warnings": result.warnings,
        "cache": cache_json(state),
    }))
}

#[derive(Debug)]
pub struct ReloadResult {
    pub applied: Vec<String>,
    pub rejected: Vec<RejectedChange>,
    pub warnings: Vec<String>,
    pub bind_unchanged: bool,
}

/// Diff + apply safe subset. Pure enough for unit tests (cache side effects).
pub fn apply_safe_reload(
    current: &AdminSnapshot,
    new_cfg: &Config,
    cache: &ChunkCache,
) -> Result<ReloadResult, String> {
    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    let mut warnings = Vec::new();

    if new_cfg.bind != current.bind {
        rejected.push(RejectedChange {
            field: "bind".into(),
            reason: "listen address cannot change without restart".into(),
        });
    }

    if volumes_structurally_changed(current, new_cfg) {
        rejected.push(RejectedChange {
            field: "volumes".into(),
            reason: "volume add/remove/iqn/prefix/geometry changes require restart".into(),
        });
    }

    if new_cfg.s3.bucket != current.s3_bucket
        || new_cfg.s3.endpoint != current.s3_endpoint
        || new_cfg.s3.region != current.s3_region
        || new_cfg.s3.force_path_style != current.s3_force_path_style
    {
        rejected.push(RejectedChange {
            field: "s3".into(),
            reason: "S3 bucket/endpoint/region/path-style changes require restart".into(),
        });
    }

    if new_cfg.auth.is_some()
        || new_cfg.volumes.iter().any(|v| v.auth.is_some())
        || current.volumes.iter().any(|v| v.auth != "none")
    {
        rejected.push(RejectedChange {
            field: "auth".into(),
            reason: "CHAP / ACL changes require restart".into(),
        });
    }

    if new_cfg.cache.max_bytes != current.cache_max_bytes {
        if new_cfg.cache.max_bytes == 0 {
            cache.disable();
        } else {
            cache.enable(Some(new_cfg.cache.max_bytes));
        }
        applied.push(format!("cache.max_bytes={}", new_cfg.cache.max_bytes));
    }

    let new_portals: Vec<String> = if !new_cfg.portals.is_empty() {
        new_cfg.portals.clone()
    } else if let Some(ref a) = new_cfg.advertise {
        vec![a.clone()]
    } else {
        current.portals.clone()
    };
    if new_portals != current.portals {
        warnings.push(
            "portals/advertise updated in stats labels only; SendTargets still uses addresses from process start until restart"
                .into(),
        );
        applied.push("portals/advertise@stats".into());
    }

    if !rejected.is_empty() && applied.is_empty() {
        // Still ok — report rejections
    }

    Ok(ReloadResult {
        applied,
        rejected,
        warnings,
        bind_unchanged: new_cfg.bind == current.bind,
    })
}

fn volumes_structurally_changed(current: &AdminSnapshot, new_cfg: &Config) -> bool {
    if current.volumes.len() != new_cfg.volumes.len() {
        return true;
    }
    for (cur, vol) in current.volumes.iter().zip(new_cfg.volumes.iter()) {
        if cur.name != vol.name
            || cur.iqn != vol.iqn
            || cur.prefix != vol.prefix.trim_matches('/')
            || cur.capacity != vol.capacity
            || cur.chunk_size != vol.chunk_size
            || cur.compression != vol.compression.as_str()
        {
            return true;
        }
    }
    false
}

/// Send one JSON request and read one JSON response over a Unix socket.
pub fn call_admin(socket: &Path, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.write_all(b"\n").map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader
        .read_line(&mut resp)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(resp.trim()).map_err(|e| format!("bad response json: {e}"))
}

/// Two-phase `volume.write_image`: ready handshake, stream `size` bytes, final JSON.
pub fn call_admin_write_image(
    socket: &Path,
    volume: &str,
    size: u64,
    body: &mut impl Read,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let req = json!({
        "op": "volume.write_image",
        "volume": volume,
        "size": size,
    });
    let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.write_all(b"\n").map_err(|e| format!("write: {e}"))?;

    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("clone: {e}"))?);
    let mut ready_line = String::new();
    reader
        .read_line(&mut ready_line)
        .map_err(|e| format!("read ready: {e}"))?;
    let ready: serde_json::Value = serde_json::from_str(ready_line.trim())
        .map_err(|e| format!("bad ready json: {e}"))?;
    if ready.get("ok") != Some(&json!(true)) {
        return Ok(ready);
    }

    let mut remaining = size;
    let mut buf = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let n = (remaining as usize).min(buf.len());
        body.read_exact(&mut buf[..n])
            .map_err(|e| format!("read local file: {e}"))?;
        stream
            .write_all(&buf[..n])
            .map_err(|e| format!("send image bytes: {e}"))?;
        remaining -= n as u64;
    }

    let mut final_line = String::new();
    reader
        .read_line(&mut final_line)
        .map_err(|e| format!("read final: {e}"))?;
    serde_json::from_str(final_line.trim()).map_err(|e| format!("bad final json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CACHE_MAX;

    #[test]
    fn apply_reload_cache_only() {
        let cache = ChunkCache::new(1024);
        let snap = AdminSnapshot {
            bind: "0.0.0.0:3260".into(),
            portals: vec![],
            instance: None,
            volumes: vec![VolumeSummary {
                name: "disk0".into(),
                iqn: "iqn.test:disk0".into(),
                capacity: 1024,
                auth: "none".into(),
                prefix: "disks/disk0".into(),
                chunk_size: 4096,
                compression: "none".into(),
            }],
            cache_max_bytes: 1024,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: "us-east-1".into(),
            s3_force_path_style: false,
        };
        let mut cfg = Config {
            bind: "0.0.0.0:3260".into(),
            advertise: None,
            portals: vec![],
            instance: None,
            auth: None,
            s3: Default::default(),
            cache: crate::config::CacheConfig { max_bytes: 0 },
            metrics: Default::default(),
            admin: Default::default(),
            volumes: vec![crate::config::VolumeConfig {
                name: "disk0".into(),
                iqn: "iqn.test:disk0".into(),
                prefix: "disks/disk0".into(),
                capacity: 1024,
                block_size: 512,
                chunk_size: 4096,
                auth: None,
                compression: Default::default(),
            }],
        };
        let r = apply_safe_reload(&snap, &cfg, &cache).unwrap();
        assert!(r.applied.iter().any(|a| a.contains("cache.max_bytes=0")));
        assert_eq!(cache.max_bytes(), 0);

        cfg.bind = "0.0.0.0:9999".into();
        cfg.cache.max_bytes = DEFAULT_CACHE_MAX;
        let r = apply_safe_reload(
            &AdminSnapshot {
                cache_max_bytes: 0,
                ..snap
            },
            &cfg,
            &cache,
        )
        .unwrap();
        assert!(r.rejected.iter().any(|x| x.field == "bind"));
    }

    #[test]
    fn parse_max_bytes_values() {
        assert_eq!(parse_max_bytes_value(&json!(0)).unwrap(), 0);
        assert_eq!(
            parse_max_bytes_value(&json!("1MiB")).unwrap(),
            1024 * 1024
        );
    }

    #[test]
    fn call_admin_uds_roundtrip() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        let sock_server = sock.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            let _ = sock_server;
            assert!(std::str::from_utf8(&buf[..n]).unwrap().contains("stats"));
            stream
                .write_all(br#"{"ok":true,"data":{"ping":"pong"}}"#)
                .unwrap();
            stream.write_all(b"\n").unwrap();
            tx.send(()).unwrap();
        });
        let resp = call_admin(&sock, &json!({"op":"stats"})).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["ping"], "pong");
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn ensure_empty_allows_meta_only() {
        ensure_prefix_empty_for_seed(&PrefixObjectStats {
            object_count: 1,
            meta_count: 1,
            meta_bytes: 100,
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn ensure_empty_rejects_chunks() {
        let err = ensure_prefix_empty_for_seed(&PrefixObjectStats {
            object_count: 2,
            chunk_count: 1,
            meta_count: 1,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("not empty"));
    }

    #[test]
    fn seed_image_skips_zero_chunks() {
        use crate::store::MemoryStore;
        use std::io::Cursor;

        let store = MemoryStore::new(16 * 1024, 512, 4 * 1024).unwrap();
        // chunk0 = non-zero, chunk1 = zeros, chunk2 = partial non-zero
        let mut image = vec![0u8; 10 * 1024];
        image[0] = 0xAB;
        image[9 * 1024] = 0xCD;
        let mut cursor = Cursor::new(image);
        let result = seed_image_from_reader(&store, &mut cursor, 10 * 1024).unwrap();
        assert_eq!(result.bytes_read, 10 * 1024);
        assert_eq!(result.chunks_written, 2);
        assert_eq!(result.zero_chunks_skipped, 1);
        assert_eq!(result.bytes_stored, 4 * 1024 + 2 * 1024);

        let mut buf = [0u8; 1];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);
        store.read_at(9 * 1024, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
    }

    #[test]
    fn copy_volume_overwrites_and_deletes_extra() {
        use crate::store::MemoryStore;

        let src = MemoryStore::new(16 * 1024, 512, 4 * 1024).unwrap();
        let dst = MemoryStore::new(16 * 1024, 512, 4 * 1024).unwrap();

        src.write_at(0, &[1u8; 4096]).unwrap();
        src.write_at(8192, &[2u8; 4096]).unwrap();
        // dest has stale data in chunk1 and chunk2
        dst.write_at(4096, &[9u8; 4096]).unwrap();
        dst.write_at(8192, &[8u8; 4096]).unwrap();

        let result = copy_volume(&src, &dst).unwrap();
        assert_eq!(result.chunks_copied, 2);
        assert_eq!(result.chunks_deleted, 1); // dest-only chunk1

        let mut buf = [0u8; 1];
        dst.read_at(0, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        dst.read_at(4096, &mut buf).unwrap();
        assert_eq!(buf[0], 0); // deleted → sparse zeros
        dst.read_at(8192, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        assert_eq!(dst.present_chunks().unwrap(), vec![0, 2]);
    }

    #[test]
    fn copy_volume_rejects_capacity_mismatch() {
        use crate::store::MemoryStore;
        let src = MemoryStore::new(8192, 512, 4096).unwrap();
        let dst = MemoryStore::new(16384, 512, 4096).unwrap();
        let err = copy_volume(&src, &dst).unwrap_err();
        assert!(err.contains("capacity mismatch"));
    }

    #[test]
    fn wipe_volume_deletes_present_chunks() {
        use crate::store::MemoryStore;
        let store = MemoryStore::new(16 * 1024, 512, 4 * 1024).unwrap();
        store.write_at(0, &[1u8; 4096]).unwrap();
        store.write_at(8192, &[2u8; 4096]).unwrap();
        assert_eq!(store.present_chunks().unwrap().len(), 2);

        let result = wipe_volume(&store).unwrap();
        assert_eq!(result.chunks_deleted, 2);
        assert!(store.present_chunks().unwrap().is_empty());

        let mut buf = [0u8; 1];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0);
        store.read_at(8192, &mut buf).unwrap();
        assert_eq!(buf[0], 0);
    }
}
