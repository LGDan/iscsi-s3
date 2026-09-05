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
use iscsi_target::IscsiTarget;
use std::sync::Arc;
use std::thread;
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

fn volume_bind(base: &str, index: usize) -> Result<String, String> {
    let (host, port) = if let Some(rest) = base.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| format!("invalid bind address: {base}"))?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| format!("invalid port in bind: {base}"))?;
        (format!("[{host}]"), port)
    } else {
        let (host, port_s) = base
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid bind address: {base}"))?;
        let port: u16 = port_s
            .parse()
            .map_err(|_| format!("invalid port in bind: {base}"))?;
        (host.to_string(), port)
    };
    let port = port
        .checked_add(u16::try_from(index).map_err(|_| "too many volumes".to_string())?)
        .ok_or_else(|| "port overflow".to_string())?;
    Ok(format!("{host}:{port}"))
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

    let mut joins = Vec::new();

    for (index, vol) in cfg.volumes.iter().enumerate() {
        let opened = open_volume(&client, handle.clone(), Arc::clone(&cache), &cfg, index, vol)?;
        let bind = volume_bind(&cfg.bind, index)?;
        info!(
            name = %opened.name,
            iqn = %opened.iqn,
            bind = %bind,
            capacity = opened.store.capacity(),
            "volume ready"
        );

        let device = S3BlockDevice::new(opened.store, &opened.name);
        let iqn = opened.iqn.clone();
        let name = opened.name.clone();

        let join = thread::Builder::new()
            .name(format!("iscsi-{name}"))
            .spawn(move || {
                let target = match IscsiTarget::builder()
                    .bind_addr(&bind)
                    .target_name(&iqn)
                    .target_alias(&name)
                    .build(device)
                {
                    Ok(t) => t,
                    Err(e) => {
                        error!(volume = %name, error = %e, "failed to build iSCSI target");
                        return;
                    }
                };
                info!(volume = %name, bind = %bind, iqn = %iqn, "iSCSI target listening");
                if let Err(e) = target.run() {
                    error!(volume = %name, error = %e, "iSCSI target exited with error");
                }
            })?;
        joins.push(join);
    }

    info!(
        volumes = joins.len(),
        "started per-volume targets (port = base + index); digest-safe IscsiTarget path"
    );

    // Keep the tokio runtime alive for S3 I/O while target threads run.
    let _runtime_guard = runtime;
    for j in joins {
        let _ = j.join();
    }
    Ok(())
}
