//! Snapshot manifest + compact `chunks.bin` index (BLAKE3 content hashes).

use crate::compression::Compression;
use crate::storage_mode::StorageMode;
use crate::store::StoreError;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SNAP_MAGIC: &[u8; 8] = b"ISC3SNAP";
pub const SNAP_INDEX_VERSION: u32 = 1;
pub const HASH_LEN: usize = 32;
pub const PTR_MAGIC: &[u8; 4] = b"ISCP";
pub const PTR_ALGO_BLAKE3: u8 = 1;
pub const PTR_LEN: usize = 4 + 1 + HASH_LEN; // magic + algo + hash

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotManifest {
    pub id: String,
    pub created_at_unix: u64,
    #[serde(default)]
    pub parent: Option<String>,
    pub source_volume: String,
    pub capacity_bytes: u64,
    pub chunk_size: u64,
    pub block_size: u32,
    pub compression: Compression,
    pub chunk_count: u64,
    pub chunks_object: String,
    pub hash_algo: String,
    pub state: SnapshotState,
    pub format_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotState {
    Creating,
    Ready,
    Deleting,
}

impl SnapshotManifest {
    pub fn new_creating(
        id: String,
        source_volume: String,
        capacity_bytes: u64,
        chunk_size: u64,
        block_size: u32,
        compression: Compression,
    ) -> Self {
        let created_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id: id.clone(),
            created_at_unix,
            parent: None,
            source_volume,
            capacity_bytes,
            chunk_size,
            block_size,
            compression,
            chunk_count: 0,
            chunks_object: "chunks.bin".into(),
            hash_algo: "blake3".into(),
            state: SnapshotState::Creating,
            format_version: 1,
        }
    }
}

/// One present-chunk record in `chunks.bin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    pub index: u64,
    pub hash: [u8; HASH_LEN],
}

pub fn encode_pointer(hash: &[u8; HASH_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PTR_LEN);
    out.extend_from_slice(PTR_MAGIC);
    out.push(PTR_ALGO_BLAKE3);
    out.extend_from_slice(hash);
    out
}

pub fn decode_pointer(bytes: &[u8]) -> Result<[u8; HASH_LEN], StoreError> {
    if bytes.len() != PTR_LEN {
        return Err(StoreError::Other(format!(
            "invalid cow pointer length {}",
            bytes.len()
        )));
    }
    if &bytes[0..4] != PTR_MAGIC {
        return Err(StoreError::Other("invalid cow pointer magic".into()));
    }
    if bytes[4] != PTR_ALGO_BLAKE3 {
        return Err(StoreError::Other(format!(
            "unsupported pointer hash algo {}",
            bytes[4]
        )));
    }
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&bytes[5..]);
    Ok(hash)
}

pub fn hash_object_bytes(body: &[u8]) -> [u8; HASH_LEN] {
    *blake3::hash(body).as_bytes()
}

pub fn hash_hex(hash: &[u8; HASH_LEN]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hash_hex(s: &str) -> Result<[u8; HASH_LEN], StoreError> {
    if s.len() != HASH_LEN * 2 {
        return Err(StoreError::Other(format!("invalid blake3 hex length {}", s.len())));
    }
    let mut out = [0u8; HASH_LEN];
    for i in 0..HASH_LEN {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| StoreError::Other(format!("invalid blake3 hex: {e}")))?;
    }
    Ok(out)
}

pub fn write_chunks_bin(w: &mut impl Write, refs: &[ChunkRef]) -> Result<(), StoreError> {
    w.write_all(SNAP_MAGIC)
        .map_err(|e| StoreError::Other(e.to_string()))?;
    w.write_all(&SNAP_INDEX_VERSION.to_be_bytes())
        .map_err(|e| StoreError::Other(e.to_string()))?;
    w.write_all(&(refs.len() as u64).to_be_bytes())
        .map_err(|e| StoreError::Other(e.to_string()))?;
    for r in refs {
        w.write_all(&r.index.to_be_bytes())
            .map_err(|e| StoreError::Other(e.to_string()))?;
        w.write_all(&r.hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
    }
    Ok(())
}

pub fn read_chunks_bin(r: &mut impl Read) -> Result<Vec<ChunkRef>, StoreError> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)
        .map_err(|e| StoreError::Other(format!("read chunks.bin magic: {e}")))?;
    if &magic != SNAP_MAGIC {
        return Err(StoreError::Other("bad chunks.bin magic".into()));
    }
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver)
        .map_err(|e| StoreError::Other(format!("read chunks.bin version: {e}")))?;
    let version = u32::from_be_bytes(ver);
    if version != SNAP_INDEX_VERSION {
        return Err(StoreError::Other(format!(
            "unsupported chunks.bin version {version}"
        )));
    }
    let mut count_buf = [0u8; 8];
    r.read_exact(&mut count_buf)
        .map_err(|e| StoreError::Other(format!("read chunks.bin count: {e}")))?;
    let count = u64::from_be_bytes(count_buf) as usize;
    let mut refs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut idx_buf = [0u8; 8];
        r.read_exact(&mut idx_buf)
            .map_err(|e| StoreError::Other(format!("read chunk index: {e}")))?;
        let mut hash = [0u8; HASH_LEN];
        r.read_exact(&mut hash)
            .map_err(|e| StoreError::Other(format!("read chunk hash: {e}")))?;
        refs.push(ChunkRef {
            index: u64::from_be_bytes(idx_buf),
            hash,
        });
    }
    Ok(refs)
}

pub fn require_cow(mode: StorageMode, volume: &str) -> Result<(), String> {
    if mode.supports_snapshots() {
        Ok(())
    } else {
        Err(format!(
            "volume {volume} uses storage={}; set storage=\"cow\" (and migrate) to use snapshots",
            mode.as_str()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn pointer_roundtrip() {
        let hash = hash_object_bytes(b"hello");
        let ptr = encode_pointer(&hash);
        assert_eq!(decode_pointer(&ptr).unwrap(), hash);
    }

    #[test]
    fn chunks_bin_roundtrip() {
        let refs = vec![
            ChunkRef {
                index: 0,
                hash: hash_object_bytes(b"a"),
            },
            ChunkRef {
                index: 2,
                hash: hash_object_bytes(b"b"),
            },
        ];
        let mut buf = Vec::new();
        write_chunks_bin(&mut buf, &refs).unwrap();
        let got = read_chunks_bin(&mut Cursor::new(buf)).unwrap();
        assert_eq!(got, refs);
    }
}
