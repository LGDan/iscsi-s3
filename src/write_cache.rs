//! Optional per-volume write-back cache in front of a BlockStore.
//!
//! `none` — write-through (SCSI write returns after the inner store accepts it).
//! `memory` — dirty chunks in RAM; lost on crash; flushed on SYNCHRONIZE CACHE / shutdown.
//! `disk` — same as memory plus atomic files under `path` so a restart can recover.

use crate::cache::{CachedStore, ChunkCache};
use crate::config::{WriteCacheConfig, WriteCacheMode};
use crate::store::{check_range, BlockStore, S3ChunkStore, StoreError};
use std::sync::Arc;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const STAMP_NAME: &str = ".write-cache.json";

pub struct WriteCachedStore<S: BlockStore> {
    inner: S,
    backend: WriteCacheBackend,
    max_bytes: u64,
}

struct DirtyChunk {
    data: Vec<u8>,
    tick: u64,
}

struct DirtyState {
    chunks: HashMap<u64, DirtyChunk>,
    tick: u64,
    used_bytes: u64,
}

impl DirtyState {
    fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            tick: 0,
            used_bytes: 0,
        }
    }

    fn from_recovered(map: HashMap<u64, Vec<u8>>) -> Self {
        let mut state = Self::new();
        for (idx, data) in map {
            state.insert(idx, data);
        }
        state
    }

    fn insert(&mut self, idx: u64, data: Vec<u8>) {
        self.tick = self.tick.wrapping_add(1);
        let size = data.len() as u64;
        if let Some(old) = self.chunks.insert(
            idx,
            DirtyChunk {
                data,
                tick: self.tick,
            },
        ) {
            self.used_bytes = self.used_bytes.saturating_sub(old.data.len() as u64);
        }
        self.used_bytes = self.used_bytes.saturating_add(size);
    }

    fn remove(&mut self, idx: u64) -> Option<Vec<u8>> {
        let e = self.chunks.remove(&idx)?;
        self.used_bytes = self.used_bytes.saturating_sub(e.data.len() as u64);
        Some(e.data)
    }

    fn get(&self, idx: u64) -> Option<Vec<u8>> {
        self.chunks.get(&idx).map(|e| e.data.clone())
    }

    fn oldest(&self) -> Option<u64> {
        self.chunks
            .iter()
            .min_by_key(|(_, e)| e.tick)
            .map(|(k, _)| *k)
    }
}

enum WriteCacheBackend {
    Off,
    Memory {
        dirty: Mutex<DirtyState>,
    },
    Disk {
        dir: PathBuf,
        dirty: Mutex<DirtyState>,
    },
}

impl<S: BlockStore> WriteCachedStore<S> {
    pub fn wrap(inner: S, cfg: &WriteCacheConfig, volume: &str) -> Result<Self, StoreError> {
        let max_bytes = cfg.max_bytes;
        let backend = match cfg.mode {
            WriteCacheMode::None => WriteCacheBackend::Off,
            WriteCacheMode::Memory => {
                info!(
                    volume,
                    max_bytes,
                    "write cache: memory (volatile; flush on SYNCHRONIZE CACHE / shutdown / budget)"
                );
                WriteCacheBackend::Memory {
                    dirty: Mutex::new(DirtyState::new()),
                }
            }
            WriteCacheMode::Disk => {
                let dir = cfg.path.as_ref().ok_or_else(|| {
                    StoreError::Other("write_cache.mode=disk requires path".into())
                })?;
                let recovered = load_disk_cache(dir, volume, inner.chunk_size())?;
                info!(
                    volume,
                    path = %dir.display(),
                    recovered = recovered.len(),
                    max_bytes,
                    "write cache: disk (dirty chunks persist until flush)"
                );
                WriteCacheBackend::Disk {
                    dir: dir.clone(),
                    dirty: Mutex::new(DirtyState::from_recovered(recovered)),
                }
            }
        };
        let store = Self {
            inner,
            backend,
            max_bytes,
        };
        store.evict_over_budget()?;
        Ok(store)
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn mode(&self) -> WriteCacheMode {
        match self.backend {
            WriteCacheBackend::Off => WriteCacheMode::None,
            WriteCacheBackend::Memory { .. } => WriteCacheMode::Memory,
            WriteCacheBackend::Disk { .. } => WriteCacheMode::Disk,
        }
    }

    /// Drop dirty data without flushing to the inner store (e.g. snapshot restore).
    pub fn discard_dirty(&self) -> Result<(), StoreError> {
        let Some(state) = self.dirty_state() else {
            return Ok(());
        };
        let idxs: Vec<u64> = state.lock().chunks.keys().copied().collect();
        for idx in idxs {
            self.remove_persisted(idx)?;
        }
        *state.lock() = DirtyState::new();
        Ok(())
    }

    pub fn dirty_count(&self) -> usize {
        match &self.backend {
            WriteCacheBackend::Off => 0,
            WriteCacheBackend::Memory { dirty } | WriteCacheBackend::Disk { dirty, .. } => {
                dirty.lock().chunks.len()
            }
        }
    }

    pub fn dirty_bytes(&self) -> u64 {
        match &self.backend {
            WriteCacheBackend::Off => 0,
            WriteCacheBackend::Memory { dirty } | WriteCacheBackend::Disk { dirty, .. } => {
                dirty.lock().used_bytes
            }
        }
    }

    fn dirty_state(&self) -> Option<&Mutex<DirtyState>> {
        match &self.backend {
            WriteCacheBackend::Off => None,
            WriteCacheBackend::Memory { dirty } | WriteCacheBackend::Disk { dirty, .. } => {
                Some(dirty)
            }
        }
    }

    fn persist_chunk(&self, idx: u64, data: &[u8]) -> Result<(), StoreError> {
        let WriteCacheBackend::Disk { dir, .. } = &self.backend else {
            return Ok(());
        };
        write_chunk_file(dir, idx, data)
    }

    fn remove_persisted(&self, idx: u64) -> Result<(), StoreError> {
        let WriteCacheBackend::Disk { dir, .. } = &self.backend else {
            return Ok(());
        };
        let path = chunk_path(dir, idx);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Other(format!(
                "remove write-cache {}: {e}",
                path.display()
            ))),
        }
    }

    fn load_chunk_for_write(&self, idx: u64) -> Result<Vec<u8>, StoreError> {
        if let Some(state) = self.dirty_state() {
            if let Some(existing) = state.lock().get(idx) {
                return Ok(existing);
            }
        }
        read_full_chunk(&self.inner, idx)
    }

    fn flush_chunk(&self, idx: u64, data: &[u8]) -> Result<(), StoreError> {
        let chunk_size = self.inner.chunk_size();
        let capacity = self.inner.capacity();
        let offset = idx.saturating_mul(chunk_size);
        if offset < capacity {
            let valid = ((capacity - offset) as usize).min(data.len());
            self.inner.write_at(offset, &data[..valid])?;
        }
        self.remove_persisted(idx)?;
        if let Some(state) = self.dirty_state() {
            state.lock().remove(idx);
        }
        Ok(())
    }

    fn evict_over_budget(&self) -> Result<(), StoreError> {
        if self.max_bytes == 0 || matches!(self.backend, WriteCacheBackend::Off) {
            return Ok(());
        }
        loop {
            let victim = {
                let Some(state) = self.dirty_state() else {
                    return Ok(());
                };
                let guard = state.lock();
                if guard.used_bytes <= self.max_bytes {
                    return Ok(());
                }
                guard.oldest()
            };
            let Some(idx) = victim else {
                return Ok(());
            };
            let Some(data) = self.dirty_state().and_then(|s| s.lock().get(idx)) else {
                continue;
            };
            self.flush_chunk(idx, &data)?;
        }
    }
}

impl WriteCachedStore<CachedStore<S3ChunkStore>> {
    pub fn s3(&self) -> &S3ChunkStore {
        self.inner.inner()
    }

    pub fn cache(&self) -> &Arc<ChunkCache> {
        self.inner.cache()
    }

    pub fn lock_io(&self) {
        if let Err(e) = self.flush() {
            warn!(error = %e, "flush write cache before I/O lock failed");
        }
        self.inner.lock_io();
    }

    pub fn unlock_io(&self) {
        self.inner.unlock_io();
    }

    pub fn is_io_locked(&self) -> bool {
        self.inner.is_io_locked()
    }
}

impl<S: BlockStore> BlockStore for WriteCachedStore<S> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        if matches!(self.backend, WriteCacheBackend::Off) {
            return self.inner.read_at(offset, buf);
        }
        check_range(self.inner.capacity(), offset, buf.len())?;
        let chunk_size = self.inner.chunk_size();
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(buf.len() - done);
            if let Some(chunk) = self
                .dirty_state()
                .and_then(|m| m.lock().get(chunk_idx))
            {
                buf[done..done + take].copy_from_slice(&chunk[within..within + take]);
            } else {
                self.inner
                    .read_at(abs, &mut buf[done..done + take])?;
            }
            done += take;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
        if matches!(self.backend, WriteCacheBackend::Off) {
            return self.inner.write_at(offset, data);
        }
        check_range(self.inner.capacity(), offset, data.len())?;
        let chunk_size = self.inner.chunk_size();
        let mut done = 0usize;
        while done < data.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(data.len() - done);
            let mut chunk = self.load_chunk_for_write(chunk_idx)?;
            chunk[within..within + take].copy_from_slice(&data[done..done + take]);
            self.persist_chunk(chunk_idx, &chunk)?;
            if let Some(state) = self.dirty_state() {
                state.lock().insert(chunk_idx, chunk);
            }
            done += take;
        }
        self.evict_over_budget()?;
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
        self.flush()?;
        self.inner.set_capacity(new_capacity)
    }

    fn flush(&self) -> Result<(), StoreError> {
        let Some(state) = self.dirty_state() else {
            return self.inner.flush();
        };
        let dirty: Vec<(u64, Vec<u8>)> = {
            let guard = state.lock();
            guard
                .chunks
                .iter()
                .map(|(k, v)| (*k, v.data.clone()))
                .collect()
        };
        for (idx, chunk) in dirty {
            self.flush_chunk(idx, &chunk)?;
        }
        self.inner.flush()
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn chunk_size(&self) -> u64 {
        self.inner.chunk_size()
    }

    fn present_chunks(&self) -> Result<Vec<u64>, StoreError> {
        let mut idxs = self.inner.present_chunks()?;
        if let Some(state) = self.dirty_state() {
            for idx in state.lock().chunks.keys() {
                if !idxs.contains(idx) {
                    idxs.push(*idx);
                }
            }
            idxs.sort_unstable();
        }
        Ok(idxs)
    }

    fn delete_chunk(&self, index: u64) -> Result<(), StoreError> {
        if let Some(state) = self.dirty_state() {
            state.lock().remove(index);
        }
        self.remove_persisted(index)?;
        self.inner.delete_chunk(index)
    }
}

impl<S: BlockStore> Drop for WriteCachedStore<S> {
    fn drop(&mut self) {
        if matches!(self.backend, WriteCacheBackend::Off) {
            return;
        }
        if let Err(e) = self.flush() {
            warn!(error = %e, "write cache flush on drop failed; dirty data may be lost");
        }
    }
}

fn read_full_chunk(store: &impl BlockStore, idx: u64) -> Result<Vec<u8>, StoreError> {
    let chunk_size = store.chunk_size();
    let mut buf = vec![0u8; chunk_size as usize];
    let offset = idx.saturating_mul(chunk_size);
    let capacity = store.capacity();
    if offset >= capacity {
        return Err(StoreError::OutOfRange {
            offset,
            len: chunk_size,
            capacity,
        });
    }
    let valid = ((capacity - offset) as usize).min(buf.len());
    store.read_at(offset, &mut buf[..valid])?;
    Ok(buf)
}

fn chunk_path(dir: &Path, idx: u64) -> PathBuf {
    dir.join(format!("{idx:016x}"))
}

fn write_chunk_file(dir: &Path, idx: u64, data: &[u8]) -> Result<(), StoreError> {
    fs::create_dir_all(dir).map_err(|e| {
        StoreError::Other(format!("create write-cache dir {}: {e}", dir.display()))
    })?;
    let dest = chunk_path(dir, idx);
    let tmp = dest.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| {
            StoreError::Other(format!("create {}: {e}", tmp.display()))
        })?;
        f.write_all(data)
            .map_err(|e| StoreError::Other(format!("write {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| StoreError::Other(format!("sync {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, &dest)
        .map_err(|e| StoreError::Other(format!("rename {} -> {}: {e}", tmp.display(), dest.display())))?;
    Ok(())
}

fn load_disk_cache(
    dir: &Path,
    volume: &str,
    chunk_size: u64,
) -> Result<HashMap<u64, Vec<u8>>, StoreError> {
    fs::create_dir_all(dir).map_err(|e| {
        StoreError::Other(format!("create write-cache dir {}: {e}", dir.display()))
    })?;
    let stamp_path = dir.join(STAMP_NAME);
    if stamp_path.exists() {
        let text = fs::read_to_string(&stamp_path).map_err(|e| {
            StoreError::Other(format!("read {}: {e}", stamp_path.display()))
        })?;
        let stamp: WriteCacheStamp = serde_json::from_str(&text).map_err(|e| {
            StoreError::Other(format!("parse {}: {e}", stamp_path.display()))
        })?;
        if stamp.volume != volume {
            return Err(StoreError::Other(format!(
                "write-cache {} belongs to volume {:?}, not {volume:?}",
                dir.display(),
                stamp.volume
            )));
        }
        if stamp.chunk_size != chunk_size {
            return Err(StoreError::Other(format!(
                "write-cache {} chunk_size {} != volume {chunk_size}",
                dir.display(),
                stamp.chunk_size
            )));
        }
    } else {
        let stamp = WriteCacheStamp {
            volume: volume.to_string(),
            chunk_size,
        };
        let text = serde_json::to_string_pretty(&stamp)
            .map_err(|e| StoreError::Other(format!("stamp json: {e}")))?;
        fs::write(&stamp_path, text).map_err(|e| {
            StoreError::Other(format!("write {}: {e}", stamp_path.display()))
        })?;
    }

    let mut dirty = HashMap::new();
    for ent in fs::read_dir(dir)
        .map_err(|e| StoreError::Other(format!("read write-cache dir {}: {e}", dir.display())))?
    {
        let ent = ent.map_err(|e| StoreError::Other(format!("read_dir: {e}")))?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.ends_with(".tmp") {
            continue;
        }
        if name.len() != 16 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let idx = u64::from_str_radix(&name, 16)
            .map_err(|e| StoreError::Other(format!("bad write-cache name {name}: {e}")))?;
        let bytes = fs::read(ent.path())
            .map_err(|e| StoreError::Other(format!("read {}: {e}", ent.path().display())))?;
        if bytes.len() as u64 != chunk_size {
            return Err(StoreError::Other(format!(
                "write-cache {} length {} != chunk_size {chunk_size}",
                ent.path().display(),
                bytes.len()
            )));
        }
        dirty.insert(idx, bytes);
    }
    Ok(dirty)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WriteCacheStamp {
    volume: String,
    chunk_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use tempfile::tempdir;

    fn mem() -> MemoryStore {
        MemoryStore::new(8192, 512, 4096).unwrap()
    }

    fn wc(mode: WriteCacheMode, max_bytes: u64) -> WriteCacheConfig {
        WriteCacheConfig {
            mode,
            path: None,
            max_bytes,
        }
    }

    #[test]
    fn none_writes_through() {
        let store = WriteCachedStore::wrap(
            mem(),
            &wc(WriteCacheMode::None, 0),
            "disk0",
        )
        .unwrap();
        store.write_at(0, &[7u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 0);
        let mut buf = [0u8; 512];
        store.inner().read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [7u8; 512]);
    }

    #[test]
    fn memory_defers_until_flush() {
        let store = WriteCachedStore::wrap(
            mem(),
            &wc(WriteCacheMode::Memory, 0),
            "disk0",
        )
        .unwrap();
        store.write_at(100, &[2u8; 50]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        let mut inner = [0u8; 50];
        store.inner().read_at(100, &mut inner).unwrap();
        assert_eq!(inner, [0u8; 50]);
        let mut seen = [0u8; 50];
        store.read_at(100, &mut seen).unwrap();
        assert_eq!(seen, [2u8; 50]);
        store.flush().unwrap();
        assert_eq!(store.dirty_count(), 0);
        store.inner().read_at(100, &mut inner).unwrap();
        assert_eq!(inner, [2u8; 50]);
    }

    #[test]
    fn memory_rmw_preserves_neighbors() {
        let store = WriteCachedStore::wrap(
            mem(),
            &wc(WriteCacheMode::Memory, 0),
            "disk0",
        )
        .unwrap();
        store.write_at(0, &[1u8; 4096]).unwrap();
        store.write_at(100, &[2u8; 50]).unwrap();
        store.flush().unwrap();
        let mut buf = [0u8; 1];
        store.read_at(99, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        store.read_at(100, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        store.read_at(150, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn disk_recovers_dirty_after_reopen() {
        let dir = tempdir().unwrap();
        let cfg = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 0,
        };
        {
            let store = WriteCachedStore::wrap(mem(), &cfg, "disk0").unwrap();
            store.write_at(0, &[9u8; 512]).unwrap();
            assert_eq!(store.dirty_count(), 1);
            // Leak without Drop flush: forget the wrapper after persisting.
            std::mem::forget(store);
        }
        let store = WriteCachedStore::wrap(mem(), &cfg, "disk0").unwrap();
        assert_eq!(store.dirty_count(), 1);
        let mut buf = [0u8; 512];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [9u8; 512]);
        store.flush().unwrap();
        assert_eq!(store.dirty_count(), 0);
    }

    #[test]
    fn disk_reopen_evicts_over_budget() {
        let dir = tempdir().unwrap();
        let unlimited = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 0,
        };
        {
            let store = WriteCachedStore::wrap(mem(), &unlimited, "disk0").unwrap();
            store.write_at(0, &[1u8; 512]).unwrap();
            store.write_at(4096, &[2u8; 512]).unwrap();
            assert_eq!(store.dirty_count(), 2);
            std::mem::forget(store);
        }
        let capped = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 4096,
        };
        let store = WriteCachedStore::wrap(mem(), &capped, "disk0").unwrap();
        assert_eq!(store.dirty_count(), 1);
        assert_eq!(store.dirty_bytes(), 4096);
    }

    #[test]
    fn present_chunks_includes_dirty_only() {
        let store = WriteCachedStore::wrap(
            mem(),
            &wc(WriteCacheMode::Memory, 0),
            "disk0",
        )
        .unwrap();
        store.write_at(4096, &[3u8; 512]).unwrap();
        assert_eq!(store.present_chunks().unwrap(), vec![1]);
        assert!(store.inner().present_chunks().unwrap().is_empty());
    }

    #[test]
    fn memory_evicts_oldest_when_over_budget() {
        let store = WriteCachedStore::wrap(
            mem(),
            &wc(WriteCacheMode::Memory, 4096),
            "disk0",
        )
        .unwrap();
        store.write_at(0, &[1u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        store.write_at(4096, &[2u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        assert_eq!(store.dirty_bytes(), 4096);
        let mut inner = [0u8; 512];
        store.inner().read_at(0, &mut inner).unwrap();
        assert_eq!(inner, [1u8; 512]);
        store.inner().read_at(4096, &mut inner).unwrap();
        assert_eq!(inner, [0u8; 512]);
        store.read_at(4096, &mut inner).unwrap();
        assert_eq!(inner, [2u8; 512]);
    }
}
