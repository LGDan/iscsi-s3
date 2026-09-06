//! `ScsiBlockDevice` adapter over `BlockStore`.

use crate::identity::{naa_from_iqn, serial_from_iqn};
use crate::metrics::{Metrics, VolumeLabels};
use crate::store::{BlockStore, StoreError};
use iscsi_target::{ScsiBlockDevice, ScsiResult};
use std::sync::Arc;
use std::time::Instant;
use tracing::error;

pub struct S3BlockDevice<S: BlockStore> {
    store: Arc<S>,
    product: String,
    serial: String,
    naa: [u8; 8],
    labels: VolumeLabels,
    metrics: Arc<Metrics>,
}

impl<S: BlockStore> S3BlockDevice<S> {
    pub fn new(store: Arc<S>, labels: VolumeLabels, metrics: Arc<Metrics>) -> Self {
        // Product ID max 16 chars for INQUIRY
        let mut product = format!("S3-{}", labels.volume);
        product.truncate(16);
        while product.len() < 16 {
            product.push(' ');
        }
        let serial = serial_from_iqn(&labels.iqn);
        let naa = naa_from_iqn(&labels.iqn);
        Self {
            store,
            product,
            serial,
            naa,
            labels,
            metrics,
        }
    }
}

fn map_err(e: StoreError) -> iscsi_target::error::IscsiError {
    error!(error = %e, "block store I/O error");
    iscsi_target::error::IscsiError::Scsi(e.to_string())
}

impl<S: BlockStore + 'static> ScsiBlockDevice for S3BlockDevice<S> {
    fn read(&self, lba: u64, blocks: u32, block_size: u32) -> ScsiResult<Vec<u8>> {
        let started = Instant::now();
        let bs = self.store.block_size();
        if block_size != bs {
            self.metrics
                .observe_scsi(&self.labels, "read", 0, started, false);
            return Err(map_err(StoreError::GeometryMismatch(format!(
                "unexpected block_size {block_size}, expected {bs}"
            ))));
        }
        let offset = match lba.checked_mul(u64::from(block_size)) {
            Some(o) => o,
            None => {
                self.metrics
                    .observe_scsi(&self.labels, "read", 0, started, false);
                return Err(map_err(StoreError::Other("lba overflow".into())));
            }
        };
        let len = match (blocks as usize).checked_mul(block_size as usize) {
            Some(l) => l,
            None => {
                self.metrics
                    .observe_scsi(&self.labels, "read", 0, started, false);
                return Err(map_err(StoreError::Other("length overflow".into())));
            }
        };
        let mut buf = vec![0u8; len];
        match self.store.read_at(offset, &mut buf) {
            Ok(()) => {
                self.metrics
                    .observe_scsi(&self.labels, "read", len as u64, started, true);
                Ok(buf)
            }
            Err(e) => {
                self.metrics
                    .observe_scsi(&self.labels, "read", 0, started, false);
                Err(map_err(e))
            }
        }
    }

    fn write(&mut self, lba: u64, data: &[u8], block_size: u32) -> ScsiResult<()> {
        let started = Instant::now();
        let bs = self.store.block_size();
        if block_size != bs {
            self.metrics
                .observe_scsi(&self.labels, "write", 0, started, false);
            return Err(map_err(StoreError::GeometryMismatch(format!(
                "unexpected block_size {block_size}, expected {bs}"
            ))));
        }
        if data.len() % block_size as usize != 0 {
            self.metrics
                .observe_scsi(&self.labels, "write", 0, started, false);
            return Err(map_err(StoreError::Other(
                "write length not multiple of block_size".into(),
            )));
        }
        let offset = match lba.checked_mul(u64::from(block_size)) {
            Some(o) => o,
            None => {
                self.metrics
                    .observe_scsi(&self.labels, "write", 0, started, false);
                return Err(map_err(StoreError::Other("lba overflow".into())));
            }
        };
        match self.store.write_at(offset, data) {
            Ok(()) => {
                self.metrics.observe_scsi(
                    &self.labels,
                    "write",
                    data.len() as u64,
                    started,
                    true,
                );
                Ok(())
            }
            Err(e) => {
                self.metrics
                    .observe_scsi(&self.labels, "write", 0, started, false);
                Err(map_err(e))
            }
        }
    }

    fn capacity(&self) -> u64 {
        self.store.capacity() / u64::from(self.store.block_size())
    }

    fn block_size(&self) -> u32 {
        self.store.block_size()
    }

    fn flush(&mut self) -> ScsiResult<()> {
        let started = Instant::now();
        match self.store.flush() {
            Ok(()) => {
                self.metrics
                    .observe_scsi(&self.labels, "flush", 0, started, true);
                Ok(())
            }
            Err(e) => {
                self.metrics
                    .observe_scsi(&self.labels, "flush", 0, started, false);
                Err(map_err(e))
            }
        }
    }

    fn vendor_id(&self) -> &str {
        "ISCSI-S3"
    }

    fn product_id(&self) -> &str {
        &self.product
    }

    fn product_rev(&self) -> &str {
        "0.1 "
    }

    fn serial_number(&self) -> &str {
        &self.serial
    }

    fn naa_identifier(&self) -> [u8; 8] {
        self.naa
    }
}
