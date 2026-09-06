//! Chunk payload compression for S3 objects.

use crate::store::StoreError;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"ISC3";
const HEADER_LEN: usize = 10; // magic(4) + algo(1) + flags(1) + uncompressed_len(4 BE)

/// Per-volume compression algorithm (stored uncompressed logical chunks in RAM/cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    #[default]
    None,
    Lz4,
    Zstd,
    Deflate,
}

impl Compression {
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Lz4 => "lz4",
            Compression::Zstd => "zstd",
            Compression::Deflate => "deflate",
        }
    }

    fn wire_id(self) -> Option<u8> {
        match self {
            Compression::None => None,
            Compression::Lz4 => Some(1),
            Compression::Zstd => Some(2),
            Compression::Deflate => Some(3),
        }
    }

    fn from_wire_id(id: u8) -> Result<Self, StoreError> {
        match id {
            1 => Ok(Compression::Lz4),
            2 => Ok(Compression::Zstd),
            3 => Ok(Compression::Deflate),
            _ => Err(StoreError::Other(format!(
                "unknown compression wire id {id}"
            ))),
        }
    }
}

/// Encode a full logical chunk for S3 storage.
pub fn encode_chunk(algo: Compression, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    if matches!(algo, Compression::None) {
        return Ok(plaintext.to_vec());
    }
    let compressed = compress(algo, plaintext)?;
    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(MAGIC);
    out.push(algo.wire_id().unwrap());
    out.push(0); // flags
    out.extend_from_slice(&(plaintext.len() as u32).to_be_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Decode an S3 object body into a full logical chunk of `expected_len` bytes.
pub fn decode_chunk(
    algo: Compression,
    stored: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, StoreError> {
    match algo {
        Compression::None => {
            if stored.len() != expected_len {
                return Err(StoreError::S3(format!(
                    "uncompressed chunk length {}, expected {expected_len}",
                    stored.len()
                )));
            }
            Ok(stored.to_vec())
        }
        _ => {
            if stored.len() < HEADER_LEN || &stored[..4] != MAGIC {
                return Err(StoreError::S3(
                    "compressed chunk missing ISC3 header (geometry/compression mismatch?)"
                        .into(),
                ));
            }
            let wire_algo = Compression::from_wire_id(stored[4])?;
            if wire_algo != algo {
                return Err(StoreError::S3(format!(
                    "chunk compression {:?} != volume {:?}",
                    wire_algo.as_str(),
                    algo.as_str()
                )));
            }
            let uncomp_len =
                u32::from_be_bytes(stored[6..10].try_into().unwrap()) as usize;
            if uncomp_len != expected_len {
                return Err(StoreError::S3(format!(
                    "compressed chunk declares length {uncomp_len}, expected {expected_len}"
                )));
            }
            let plain = decompress(algo, &stored[HEADER_LEN..], expected_len)?;
            if plain.len() != expected_len {
                return Err(StoreError::S3(format!(
                    "decompressed length {}, expected {expected_len}",
                    plain.len()
                )));
            }
            Ok(plain)
        }
    }
}

fn compress(algo: Compression, data: &[u8]) -> Result<Vec<u8>, StoreError> {
    match algo {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => Ok(lz4_flex::block::compress_prepend_size(data)),
        Compression::Zstd => zstd::bulk::compress(data, 3)
            .map_err(|e| StoreError::Other(format!("zstd compress: {e}"))),
        Compression::Deflate => {
            use flate2::write::DeflateEncoder;
            use flate2::Compression as FlateLevel;
            use std::io::Write;
            let mut enc = DeflateEncoder::new(Vec::new(), FlateLevel::fast());
            enc.write_all(data)
                .map_err(|e| StoreError::Other(format!("deflate compress: {e}")))?;
            enc.finish()
                .map_err(|e| StoreError::Other(format!("deflate finish: {e}")))
        }
    }
}

fn decompress(
    algo: Compression,
    data: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, StoreError> {
    match algo {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => lz4_flex::block::decompress_size_prepended(data)
            .map_err(|e| StoreError::Other(format!("lz4 decompress: {e}"))),
        Compression::Zstd => zstd::bulk::decompress(data, expected_len)
            .map_err(|e| StoreError::Other(format!("zstd decompress: {e}"))),
        Compression::Deflate => {
            use flate2::read::DeflateDecoder;
            use std::io::Read;
            let mut dec = DeflateDecoder::new(data);
            let mut out = Vec::with_capacity(expected_len);
            dec.read_to_end(&mut out)
                .map_err(|e| StoreError::Other(format!("deflate decompress: {e}")))?;
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_algos() {
        let plain = vec![7u8; 4096];
        for algo in [
            Compression::None,
            Compression::Lz4,
            Compression::Zstd,
            Compression::Deflate,
        ] {
            let stored = encode_chunk(algo, &plain).unwrap();
            if algo == Compression::None {
                assert_eq!(stored.len(), plain.len());
            } else {
                assert!(stored.starts_with(MAGIC));
                assert!(stored.len() < plain.len() + HEADER_LEN); // compressible zeros-ish
            }
            let out = decode_chunk(algo, &stored, plain.len()).unwrap();
            assert_eq!(out, plain, "algo={}", algo.as_str());
        }
    }

    #[test]
    fn refuses_algo_mismatch() {
        let plain = vec![1u8; 1024];
        let stored = encode_chunk(Compression::Lz4, &plain).unwrap();
        let err = decode_chunk(Compression::Zstd, &stored, plain.len()).unwrap_err();
        assert!(err.to_string().contains("compression"));
    }
}
