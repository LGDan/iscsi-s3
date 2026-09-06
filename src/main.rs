//! iSCSI target presenting S3-backed virtual disks.
//!
//! Future: HTTP Range GET/PUT backend via the same `BlockStore` trait
//! (`store::http` / `HttpRangeStore`) — not implemented in v1.

use clap::Parser;
use iscsi_s3::cache::ChunkCache;
use iscsi_s3::config::{Cli, Config};
use iscsi_s3::device::S3BlockDevice;
use iscsi_s3::metrics::{spawn_metrics_server, Metrics, SessionMetricsSink, VolumeLabels};
use iscsi_s3::store::BlockStore;
use iscsi_s3::volume::{build_s3_client, open_volume};
use iscsi_target::IscsiServer;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() {
    let cli = Cli::parse();

    // Prefer RUST_LOG when set (docker-compose), otherwise --log / default.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cli.log))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let _ = tracing_log::LogTracer::init();

    if let Err(e) = run(cli) {
        error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

/// Resolve the portal advertised in SendTargets.
///
/// `advertise` may be a host (`127.0.0.1`) or full `host:port`. Host-only
/// reuses the port from `bind`.
fn resolve_advertise(advertise: &str, bind: &str) -> Result<String, String> {
    if looks_like_host_port(advertise) {
        Ok(advertise.to_string())
    } else {
        let (_, port) = split_host_port(bind)?;
        Ok(format_host_port(advertise, port))
    }
}

fn looks_like_host_port(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix('[') {
        return rest.contains("]:");
    }
    s.rsplit_once(':')
        .map(|(_, port)| port.parse::<u16>().is_ok())
        .unwrap_or(false)
}

fn split_host_port(addr: &str) -> Result<(String, u16), String> {
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| format!("invalid address: {addr}"))?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| format!("invalid port in address: {addr}"))?;
        Ok((format!("[{host}]"), port))
    } else {
        let (host, port_s) = addr
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid address: {addr}"))?;
        let port: u16 = port_s
            .parse()
            .map_err(|_| format!("invalid port in address: {addr}"))?;
        Ok((host.to_string(), port))
    }
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
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
    let metrics = Metrics::new().map_err(|e| e.to_string())?;

    let advertise = cfg
        .advertise
        .as_deref()
        .map(|a| resolve_advertise(a, &cfg.bind))
        .transpose()?;

    let mut builder = IscsiServer::builder().bind_addr(&cfg.bind);
    if let Some(ref addr) = advertise {
        builder = builder.advertise_addr(addr);
    }

    let mut volume_labels: Vec<VolumeLabels> = Vec::new();

    for (index, vol) in cfg.volumes.iter().enumerate() {
        let opened = open_volume(
            &client,
            handle.clone(),
            Arc::clone(&cache),
            Arc::clone(&metrics),
            &cfg,
            index,
            vol,
        )?;
        info!(
            name = %opened.name,
            iqn = %opened.iqn,
            bind = %cfg.bind,
            advertise = advertise.as_deref().unwrap_or("(socket local_addr)"),
            capacity = opened.store.capacity(),
            "volume ready"
        );

        volume_labels.push(opened.labels.clone());
        let device = S3BlockDevice::new(opened.store, opened.labels, Arc::clone(&metrics));
        builder = builder.add_target(opened.iqn, Box::new(device), Some(opened.name));
    }

    builder = builder.session_events(SessionMetricsSink::new(
        Arc::clone(&metrics),
        volume_labels,
    ));

    let server = Arc::new(builder.build().map_err(|e| e.to_string())?);
    info!(
        bind = %cfg.bind,
        advertise = advertise.as_deref().unwrap_or("(socket local_addr)"),
        volumes = cfg.volumes.len(),
        "starting shared-portal IscsiServer (one TCP port, many IQNs)"
    );

    if cfg.metrics.enabled {
        let server_gauges = Arc::clone(&server);
        let cache_gauges = Arc::clone(&cache);
        spawn_metrics_server(
            cfg.metrics.bind.clone(),
            Arc::clone(&metrics),
            move || {
                (
                    server_gauges.active_connection_count(),
                    server_gauges.active_session_count(),
                )
            },
            move || cache_gauges.stats(),
        )?;
    } else {
        info!("prometheus metrics disabled");
    }

    // Keep the tokio runtime alive for S3 I/O while the target runs.
    let _runtime_guard = runtime;
    server.run().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_advertise;

    #[test]
    fn advertise_host_only_uses_bind_port() {
        assert_eq!(
            resolve_advertise("127.0.0.1", "0.0.0.0:3260").unwrap(),
            "127.0.0.1:3260"
        );
    }

    #[test]
    fn advertise_full_host_port_unchanged() {
        assert_eq!(
            resolve_advertise("10.0.0.5:4000", "0.0.0.0:3260").unwrap(),
            "10.0.0.5:4000"
        );
    }
}
