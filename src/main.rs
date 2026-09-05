//! iSCSI target presenting S3-backed virtual disks.
//!
//! Future: HTTP Range GET/PUT backend via the same `BlockStore` trait
//! (`store::http` / `HttpRangeStore`) — not implemented in v1.

use clap::Parser;
use iscsi_s3::cache::ChunkCache;
use iscsi_s3::config::{Cli, Config};
use iscsi_s3::device::S3BlockDevice;
use iscsi_s3::store::BlockStore;
use iscsi_s3::volume::{build_s3_client, open_volume};
use iscsi_target::IscsiServer;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() {
    let cli = Cli::parse();

    let filter = EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let _ = tracing_log::LogTracer::init();

    if let Err(e) = run(cli) {
        error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::load(&cli)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("iscsi-s3-tokio")
        .build()?;
    let handle = runtime.handle().clone();

    let client = runtime.block_on(build_s3_client(&cfg))?;
    let cache = ChunkCache::new(cfg.cache.max_bytes);

    let mut builder = IscsiServer::builder().bind_addr(&cfg.bind);

    for (index, vol) in cfg.volumes.iter().enumerate() {
        let opened = open_volume(&client, handle.clone(), Arc::clone(&cache), &cfg, index, vol)?;
        info!(
            name = %opened.name,
            iqn = %opened.iqn,
            capacity = opened.store.capacity(),
            "volume ready"
        );

        let device = S3BlockDevice::new(opened.store, &opened.name);
        builder = builder.add_target(opened.iqn, Box::new(device), Some(opened.name));
    }

    let server = builder.build()?;
    info!(bind = %cfg.bind, volumes = cfg.volumes.len(), "iSCSI multi-target server starting");

    // Keep the tokio runtime alive while the blocking iSCSI server runs.
    let _runtime_guard = runtime;
    server.run()?;
    Ok(())
}
