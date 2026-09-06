//! In-memory block store for unit tests.

use super::{check_range, BlockStore, StoreError};
use parking_lot::RwLock;
use std::collections::HashMap;

pub struct MemoryStore {
    inner: RwLock<Inner>,
    block_size: u32,
    chunk_size: u64,
}

struct Inner {
    capacity: u64,
    /// Sparse chunks: missing = zeros.
    chunks: HashMap<u64, Vec<u8>>,
}

impl MemoryStore {
    pub fn new(capacity: u64, block_size: u32, chunk_size: u64) -> Result<Self, StoreError> {
        if block_size == 0 || chunk_size == 0 || chunk_size % u64::from(block_size) != 0 {
            return Err(StoreError::GeometryMismatch(
                "invalid block_size/chunk_size".into(),
            ));
        }
        if capacity == 0 || capacity % u64::from(block_size) != 0 {
            return Err(StoreError::GeometryMismatch(
                "capacity must be multiple of block_size".into(),
            ));
        }
        Ok(Self {
            inner: RwLock::new(Inner {
                capacity,
                chunks: HashMap::new(),
            }),
            block_size,
            chunk_size,
        })
    }
}

impl BlockStore for MemoryStore {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        let inner = self.inner.read();
        check_range(inner.capacity, offset, buf.len())?;
        buf.fill(0);
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / self.chunk_size;
            let within = (abs % self.chunk_size) as usize;
            let take = ((self.chunk_size as usize) - within).min(buf.len() - done);
            if let Some(chunk) = inner.chunks.get(&chunk_idx) {
                buf[done..done + take].copy_from_slice(&chunk[within..within + take]);
            }
            done += take;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
        let mut inner = self.inner.write();
        check_range(inner.capacity, offset, data.len())?;
        let chunk_size = self.chunk_size as usize;
        let mut done = 0usize;
        while done < data.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / self.chunk_size;
            let within = (abs % self.chunk_size) as usize;
            let take = (chunk_size - within).min(data.len() - done);
            let chunk = inner
                .chunks
                .entry(chunk_idx)
                .or_insert_with(|| vec![0u8; chunk_size]);
            chunk[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.inner.read().capacity
    }

    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
        if new_capacity % u64::from(self.block_size) != 0 {
            return Err(StoreError::GeometryMismatch(
                "capacity must be multiple of block_size".into(),
            ));
        }
        let mut inner = self.inner.write();
        if new_capacity < inner.capacity {
            return Err(StoreError::ShrinkRefused {
                current: inner.capacity,
                requested: new_capacity,
            });
        }
        inner.capacity = new_capacity;
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
        let mut keys: Vec<u64> = self.inner.read().chunks.keys().copied().collect();
        keys.sort_unstable();
        Ok(keys)
    }

    fn delete_chunk(&self, index: u64) -> Result<(), StoreError> {
        self.inner.write().chunks.remove(&index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_read_is_zeros() {
        let s = MemoryStore::new(8192, 512, 4096).unwrap();
        let mut buf = vec![0xff; 1024];
        s.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn write_read_across_chunk_boundary() {
        let s = MemoryStore::new(16 * 1024, 512, 4096).unwrap();
        let data: Vec<u8> = (0u8..200).collect();
        // Cross chunk boundary at 4096
        s.write_at(4000, &data).unwrap();
        let mut buf = vec![0u8; data.len()];
        s.read_at(4000, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn partial_rmw_preserves_neighbors() {
        let s = MemoryStore::new(8192, 512, 4096).unwrap();
        s.write_at(0, &[1u8; 4096]).unwrap();
        s.write_at(100, &[2u8; 50]).unwrap();
        let mut buf = [0u8; 1];
        s.read_at(99, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        s.read_at(100, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        s.read_at(150, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn grow_only() {
        let s = MemoryStore::new(4096, 512, 4096).unwrap();
        s.write_at(0, &[9u8; 512]).unwrap();
        s.set_capacity(8192).unwrap();
        assert_eq!(s.capacity(), 8192);
        let mut buf = [0u8; 512];
        s.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [9u8; 512]);
        let err = s.set_capacity(4096).unwrap_err();
        assert!(matches!(err, StoreError::ShrinkRefused { .. }));
    }

    #[test]
    fn out_of_range() {
        let s = MemoryStore::new(4096, 512, 4096).unwrap();
        let mut buf = [0u8; 100];
        assert!(s.read_at(4000, &mut buf).is_err());
    }
}
