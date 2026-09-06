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

fn volume_bind(base: &str, index: usize) -> Result<String, String> {
    let (host, port) = split_host_port(base)?;
    let port = port
        .checked_add(u16::try_from(index).map_err(|_| "too many volumes".to_string())?)
        .ok_or_else(|| "port overflow".to_string())?;
    Ok(format_host_port(&host, port))
}

/// Resolve the per-volume address returned in SendTargets.
///
/// `advertise` may be a host (`127.0.0.1`, `host.docker.internal`) or a full
/// `host:port` base (same port-offset rules as `bind`).
fn volume_advertise(advertise: &str, bind: &str, index: usize) -> Result<String, String> {
    if looks_like_host_port(advertise) {
        volume_bind(advertise, index)
    } else {
        let bound = volume_bind(bind, index)?;
        let (_, port) = split_host_port(&bound)?;
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
    // Bracket bare IPv6 literals (contain ':' but are not already [bracketed]).
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

    let mut joins = Vec::new();

    for (index, vol) in cfg.volumes.iter().enumerate() {
        let opened = open_volume(&client, handle.clone(), Arc::clone(&cache), &cfg, index, vol)?;
        let bind = volume_bind(&cfg.bind, index)?;
        let advertise = cfg
            .advertise
            .as_deref()
            .map(|a| volume_advertise(a, &cfg.bind, index))
            .transpose()?;
        info!(
            name = %opened.name,
            iqn = %opened.iqn,
            bind = %bind,
            advertise = advertise.as_deref().unwrap_or("(socket local_addr)"),
            capacity = opened.store.capacity(),
            "volume ready"
        );

        let device = S3BlockDevice::new(opened.store, &opened.name);
        let iqn = opened.iqn.clone();
        let name = opened.name.clone();

        let join = thread::Builder::new()
            .name(format!("iscsi-{name}"))
            .spawn(move || {
                let mut builder = IscsiTarget::builder()
                    .bind_addr(&bind)
                    .target_name(&iqn)
                    .target_alias(&name);
                if let Some(ref addr) = advertise {
                    builder = builder.advertise_addr(addr);
                }
                let target = match builder.build(device) {
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

#[cfg(test)]
mod tests {
    use super::{volume_advertise, volume_bind};

    #[test]
    fn volume_bind_offsets_port() {
        assert_eq!(volume_bind("0.0.0.0:3260", 0).unwrap(), "0.0.0.0:3260");
        assert_eq!(volume_bind("0.0.0.0:3260", 1).unwrap(), "0.0.0.0:3261");
    }

    #[test]
    fn advertise_host_only_uses_bind_ports() {
        assert_eq!(
            volume_advertise("127.0.0.1", "0.0.0.0:3260", 0).unwrap(),
            "127.0.0.1:3260"
        );
        assert_eq!(
            volume_advertise("127.0.0.1", "0.0.0.0:3260", 1).unwrap(),
            "127.0.0.1:3261"
        );
    }

    #[test]
    fn advertise_host_port_offsets_like_bind() {
        assert_eq!(
            volume_advertise("10.0.0.5:4000", "0.0.0.0:3260", 1).unwrap(),
            "10.0.0.5:4001"
        );
    }
}
