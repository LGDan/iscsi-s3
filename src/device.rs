//! `ScsiBlockDevice` adapter over `BlockStore`.

use crate::store::{BlockStore, StoreError};
use iscsi_target::{ScsiBlockDevice, ScsiResult};
use std::sync::Arc;
use tracing::error;

pub struct S3BlockDevice<S: BlockStore> {
    store: Arc<S>,
    product: String,
}

impl<S: BlockStore> S3BlockDevice<S> {
    pub fn new(store: Arc<S>, name: &str) -> Self {
        // Product ID max 16 chars for INQUIRY
        let mut product = format!("S3-{name}");
        product.truncate(16);
        while product.len() < 16 {
            product.push(' ');
        }
        Self { store, product }
    }
}

fn map_err(e: StoreError) -> iscsi_target::error::IscsiError {
    error!(error = %e, "block store I/O error");
    iscsi_target::error::IscsiError::Scsi(e.to_string())
}

impl<S: BlockStore + 'static> ScsiBlockDevice for S3BlockDevice<S> {
    fn read(&self, lba: u64, blocks: u32, block_size: u32) -> ScsiResult<Vec<u8>> {
        let bs = self.store.block_size();
        if block_size != bs {
            return Err(map_err(StoreError::GeometryMismatch(format!(
                "unexpected block_size {block_size}, expected {bs}"
            ))));
        }
        let offset = lba
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| map_err(StoreError::Other("lba overflow".into())))?;
        let len = (blocks as usize)
            .checked_mul(block_size as usize)
            .ok_or_else(|| map_err(StoreError::Other("length overflow".into())))?;
        let mut buf = vec![0u8; len];
        self.store.read_at(offset, &mut buf).map_err(map_err)?;
        Ok(buf)
    }

    fn write(&mut self, lba: u64, data: &[u8], block_size: u32) -> ScsiResult<()> {
        let bs = self.store.block_size();
        if block_size != bs {
            return Err(map_err(StoreError::GeometryMismatch(format!(
                "unexpected block_size {block_size}, expected {bs}"
            ))));
        }
        if data.len() % block_size as usize != 0 {
            return Err(map_err(StoreError::Other(
                "write length not multiple of block_size".into(),
            )));
        }
        let offset = lba
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| map_err(StoreError::Other("lba overflow".into())))?;
        self.store.write_at(offset, data).map_err(map_err)?;
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.store.capacity() / u64::from(self.store.block_size())
    }

    fn block_size(&self) -> u32 {
        self.store.block_size()
    }

    fn flush(&mut self) -> ScsiResult<()> {
        self.store.flush().map_err(map_err)
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
}
