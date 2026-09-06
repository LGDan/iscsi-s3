//! Volume open helpers (meta grow rules live in S3ChunkStore::open).

use crate::cache::{CachedStore, ChunkCache};
use crate::config::{Config, VolumeConfig};
use crate::metrics::{Metrics, VolumeLabels};
use crate::store::{BlockStore, S3ChunkStore, S3StoreConfig, StoreError};
use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client;
use std::sync::Arc;
use tokio::runtime::Handle;

pub struct OpenedVolume {
    pub name: String,
    pub iqn: String,
    pub labels: VolumeLabels,
    pub store: Arc<CachedStore<S3ChunkStore>>,
}

pub async fn build_s3_client(cfg: &Config) -> Result<Client, StoreError> {
    let region = Region::new(cfg.s3.region.clone());
    let region_provider = RegionProviderChain::first_try(region.clone()).or_default_provider();

    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(region_provider);

    if let (Some(ak), Some(sk)) = (&cfg.s3.access_key_id, &cfg.s3.secret_access_key) {
        loader = loader.credentials_provider(Credentials::new(
            ak,
            sk,
            None,
            None,
            "iscsi-s3-config",
        ));
    }

    let shared = loader.load().await;

    let mut s3_conf = aws_sdk_s3::config::Builder::from(&shared).region(Some(region));

    if let Some(endpoint) = &cfg.s3.endpoint {
        s3_conf = s3_conf.endpoint_url(endpoint);
    }
    if cfg.s3.force_path_style {
        s3_conf = s3_conf.force_path_style(true);
    }

    Ok(Client::from_conf(s3_conf.build()))
}

pub fn open_volume(
    client: &Client,
    runtime: Handle,
    cache: Arc<ChunkCache>,
    metrics: Arc<Metrics>,
    cfg: &Config,
    _index: usize,
    vol: &VolumeConfig,
) -> Result<OpenedVolume, StoreError> {
    let bucket = cfg
        .s3
        .bucket
        .clone()
        .ok_or_else(|| StoreError::Other("s3.bucket required".into()))?;

    let labels = VolumeLabels::new(vol.name.clone(), vol.iqn.clone());

    let store = S3ChunkStore::open(
        client.clone(),
        runtime,
        S3StoreConfig {
            bucket,
            prefix: vol.prefix.clone(),
            capacity: vol.capacity,
            block_size: vol.block_size,
            chunk_size: vol.chunk_size,
            labels: labels.clone(),
            metrics: Arc::clone(&metrics),
        },
    )?;

    let cached = Arc::new(CachedStore::new(
        store,
        cache,
        labels.clone(),
        Arc::clone(&metrics),
    ));

    metrics.set_volume_capacity(&labels, cached.capacity());

    Ok(OpenedVolume {
        name: vol.name.clone(),
        iqn: vol.iqn.clone(),
        labels,
        store: cached,
    })
}
