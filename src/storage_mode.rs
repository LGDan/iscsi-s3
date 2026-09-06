//! Per-volume on-disk layout: legacy flat chunks vs content-addressed COW.

use serde::{Deserialize, Serialize};

/// Volume storage layout (locked in `meta.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    /// Flat `{prefix}/chunks/{index}` full payloads — max performance, no snapshots.
    #[default]
    Legacy,
    /// Content-addressed objects + live pointers — snapshots enabled.
    Cow,
}

impl StorageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            StorageMode::Legacy => "legacy",
            StorageMode::Cow => "cow",
        }
    }

    pub fn supports_snapshots(self) -> bool {
        matches!(self, StorageMode::Cow)
    }
}
