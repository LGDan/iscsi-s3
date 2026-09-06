//! Deterministic SCSI identity helpers for multipath / multi-instance.

/// FNV-1a 64-bit — stable across Rust versions (unlike DefaultHasher).
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in data {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 16-character ASCII serial derived from IQN (same on every instance).
pub fn serial_from_iqn(iqn: &str) -> String {
    format!("{:016X}", fnv1a64(iqn.as_bytes()))
}

/// NAA-6 style 8-byte identifier derived from IQN.
///
/// Layout: nibble `6` (NAA-6) + 60 bits of FNV hash. Unique per IQN and
/// identical across iscsi-s3 instances sharing that volume config.
pub fn naa_from_iqn(iqn: &str) -> [u8; 8] {
    let h = fnv1a64(iqn.as_bytes());
    let mut out = [0u8; 8];
    out[0] = 0x60 | (((h >> 56) as u8) & 0x0F);
    out[1] = (h >> 48) as u8;
    out[2] = (h >> 40) as u8;
    out[3] = (h >> 32) as u8;
    out[4] = (h >> 24) as u8;
    out[5] = (h >> 16) as u8;
    out[6] = (h >> 8) as u8;
    out[7] = h as u8;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_stable_and_distinct() {
        let a = serial_from_iqn("iqn.2026-09.local.iscsi-s3:disk0");
        let b = serial_from_iqn("iqn.2026-09.local.iscsi-s3:disk0");
        let c = serial_from_iqn("iqn.2026-09.local.iscsi-s3:disk1");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert_eq!(naa_from_iqn("iqn.2026-09.local.iscsi-s3:disk0")[0] & 0xF0, 0x60);
        assert_ne!(
            naa_from_iqn("iqn.2026-09.local.iscsi-s3:disk0"),
            naa_from_iqn("iqn.2026-09.local.iscsi-s3:disk1")
        );
    }
}
