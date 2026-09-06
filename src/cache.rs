//! Whole-chunk LRU cache in front of a BlockStore.

use crate::metrics::{Metrics, VolumeLabels};
use crate::store::{check_range, BlockStore, StoreError};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

struct CacheEntry {
    data: Vec<u8>,
    /// Approximate LRU: bumped on access.
    tick: u64,
}

struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
    max_bytes: u64,
    used_bytes: u64,
    tick: u64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    volume: String,
    chunk_idx: u64,
}

/// Shared LRU pool keyed by (volume_id, chunk_index).
pub struct ChunkCache {
    inner: Mutex<CacheInner>,
}

impl ChunkCache {
    pub fn new(max_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::new(),
                max_bytes,
                used_bytes: 0,
                tick: 0,
            }),
        })
    }

    pub fn stats(&self) -> (u64, usize) {
        let inner = self.inner.lock();
        (inner.used_bytes, inner.entries.len())
    }

    fn get(&self, volume: &str, chunk_idx: u64) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock();
        inner.tick += 1;
        let tick = inner.tick;
        inner
            .entries
            .get_mut(&CacheKey {
                volume: volume.to_string(),
                chunk_idx,
            })
            .map(|e| {
                e.tick = tick;
                e.data.clone()
            })
    }

    fn put(&self, volume: &str, chunk_idx: u64, data: Vec<u8>) {
        let mut inner = self.inner.lock();
        inner.tick += 1;
        let key = CacheKey {
            volume: volume.to_string(),
            chunk_idx,
        };
        let size = data.len() as u64;
        if let Some(old) = inner.entries.remove(&key) {
            inner.used_bytes = inner.used_bytes.saturating_sub(old.data.len() as u64);
        }
        while inner.used_bytes + size > inner.max_bytes && !inner.entries.is_empty() {
            // Evict lowest tick
            let victim = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone());
            if let Some(v) = victim {
                if let Some(e) = inner.entries.remove(&v) {
                    inner.used_bytes = inner.used_bytes.saturating_sub(e.data.len() as u64);
                }
            } else {
                break;
            }
        }
        if size <= inner.max_bytes {
            let tick = inner.tick;
            inner.used_bytes += size;
            inner.entries.insert(
                key,
                CacheEntry {
                    data,
                    tick,
                },
            );
        }
    }

    fn invalidate(&self, volume: &str, chunk_idx: u64) {
        let mut inner = self.inner.lock();
        let key = CacheKey {
            volume: volume.to_string(),
            chunk_idx,
        };
        if let Some(old) = inner.entries.remove(&key) {
            inner.used_bytes = inner.used_bytes.saturating_sub(old.data.len() as u64);
        }
    }
}

/// Write-through cached view of an inner store.
pub struct CachedStore<S: BlockStore> {
    inner: S,
    cache: Arc<ChunkCache>,
    labels: VolumeLabels,
    metrics: Arc<Metrics>,
}

impl<S: BlockStore> CachedStore<S> {
    pub fn new(
        inner: S,
        cache: Arc<ChunkCache>,
        labels: VolumeLabels,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            inner,
            cache,
            labels,
            metrics,
        }
    }

    fn load_chunk(&self, chunk_idx: u64) -> Result<Vec<u8>, StoreError> {
        if let Some(data) = self.cache.get(&self.labels.volume, chunk_idx) {
            self.metrics.observe_cache(&self.labels, true);
            return Ok(data);
        }
        self.metrics.observe_cache(&self.labels, false);
        let chunk_size = self.inner.chunk_size();
        let offset = chunk_idx * chunk_size;
        let mut buf = vec![0u8; chunk_size as usize];
        // May read past capacity for the last partial logical region — clamp.
        let capacity = self.inner.capacity();
        if offset >= capacity {
            return Err(StoreError::OutOfRange {
                offset,
                len: chunk_size,
                capacity,
            });
        }
        let valid = ((capacity - offset) as usize).min(buf.len());
        self.inner.read_at(offset, &mut buf[..valid])?;
        self.cache
            .put(&self.labels.volume, chunk_idx, buf.clone());
        Ok(buf)
    }
}

impl<S: BlockStore> BlockStore for CachedStore<S> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        check_range(self.inner.capacity(), offset, buf.len())?;
        let chunk_size = self.inner.chunk_size();
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(buf.len() - done);
            let chunk = self.load_chunk(chunk_idx)?;
            buf[done..done + take].copy_from_slice(&chunk[within..within + take]);
            done += take;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
        self.inner.write_at(offset, data)?;
        let chunk_size = self.inner.chunk_size();
        let mut done = 0usize;
        while done < data.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(data.len() - done);
            // Update or invalidate cache entry
            if let Some(mut cached) = self.cache.get(&self.labels.volume, chunk_idx) {
                cached[within..within + take].copy_from_slice(&data[done..done + take]);
                self.cache.put(&self.labels.volume, chunk_idx, cached);
            } else {
                self.cache.invalidate(&self.labels.volume, chunk_idx);
            }
            done += take;
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
        self.inner.set_capacity(new_capacity)
    }

    fn flush(&self) -> Result<(), StoreError> {
        self.inner.flush()
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn chunk_size(&self) -> u64 {
        self.inner.chunk_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    #[test]
    fn cache_serves_second_read() {
        let mem = MemoryStore::new(8192, 512, 4096).unwrap();
        mem.write_at(0, &[7u8; 100]).unwrap();
        let cache = ChunkCache::new(1024 * 1024);
        let metrics = Metrics::new().unwrap();
        let labels = VolumeLabels::new("v0", "iqn.test:v0");
        let cached = CachedStore::new(mem, cache, labels, metrics);
        let mut a = [0u8; 100];
        cached.read_at(0, &mut a).unwrap();
        let mut b = [0u8; 100];
        cached.read_at(0, &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, [7u8; 100]);
    }
}
