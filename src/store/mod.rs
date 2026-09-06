//! Block storage backends.

mod memory;
mod s3;

pub use memory::MemoryStore;
pub use s3::{
    list_chunk_indices, list_prefix_stats, plan_capacity, PrefixObjectStats, S3ChunkStore,
    S3StoreConfig, VolumeMeta,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O out of range: offset={offset} len={len} capacity={capacity}")]
    OutOfRange {
        offset: u64,
        len: u64,
        capacity: u64,
    },
    #[error("cannot shrink capacity from {current} to {requested}")]
    ShrinkRefused { current: u64, requested: u64 },
    #[error("geometry mismatch: {0}")]
    GeometryMismatch(String),
    #[error("S3 error: {0}")]
    S3(String),
    #[error("metadata error: {0}")]
    Meta(String),
    #[error("{0}")]
    Other(String),
}

/// Byte-oriented block store used by the iSCSI device adapter.
pub trait BlockStore: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError>;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError>;
    fn capacity(&self) -> u64;
    /// Grow only; errors if `new_capacity < current`.
    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError>;
    fn flush(&self) -> Result<(), StoreError>;
    fn block_size(&self) -> u32;
    fn chunk_size(&self) -> u64;
    /// Indices of chunks that currently exist (non-sparse). Used for volume copy.
    fn present_chunks(&self) -> Result<Vec<u64>, StoreError>;
    /// Remove a chunk object (restore sparse zeros). Used when overwriting on copy.
    fn delete_chunk(&self, index: u64) -> Result<(), StoreError>;
}

pub(crate) fn check_range(
    capacity: u64,
    offset: u64,
    len: usize,
) -> Result<(), StoreError> {
    let len = len as u64;
    let end = offset
        .checked_add(len)
        .ok_or(StoreError::OutOfRange {
            offset,
            len,
            capacity,
        })?;
    if end > capacity {
        return Err(StoreError::OutOfRange {
            offset,
            len,
            capacity,
        });
    }
    Ok(())
}
