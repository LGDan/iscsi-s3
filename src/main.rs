//! iSCSI target presenting S3-backed virtual disks.
//!
//! Future: HTTP Range GET/PUT backend via the same `BlockStore` trait
//! (`store::http` / `HttpRangeStore`) — not implemented in v1.

use clap::Parser;
use iscsi_s3::admin::{spawn_admin_server, AdminSnapshot, AdminState, VolumeSummary};
use iscsi_s3::cache::ChunkCache;
use iscsi_s3::config::{resolve_auth, Cli, Config};
use iscsi_s3::device::S3BlockDevice;
use iscsi_s3::metrics::{spawn_metrics_server, Metrics, SessionMetricsSink, VolumeLabels};
use iscsi_s3::store::BlockStore;
use iscsi_s3::volume::{build_s3_client, open_volume};
use iscsi_target::IscsiServer;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, warn};
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

/// Resolve portal list for SendTargets (MPIO).
///
/// Preference: non-empty `portals` → each resolved; else single `advertise`;
/// else empty (vendor falls back to socket local_addr).
fn resolve_portals(cfg: &Config) -> Result<Vec<String>, String> {
    if !cfg.portals.is_empty() {
        cfg.portals
            .iter()
            .map(|p| resolve_advertise(p, &cfg.bind))
            .collect()
    } else if let Some(ref adv) = cfg.advertise {
        Ok(vec![resolve_advertise(adv, &cfg.bind)?])
    } else {
        Ok(Vec::new())
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = cli.config.clone();
    let cfg = Config::load(&cli)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("iscsi-s3-tokio")
        .build()?;
    let handle = runtime.handle().clone();

    let client = runtime.block_on(build_s3_client(&cfg))?;
    let cache = ChunkCache::new(cfg.cache.max_bytes);
    let metrics = Metrics::new().map_err(|e| e.to_string())?;

    let portals = resolve_portals(&cfg)?;
    if portals.len() > 1 && cfg.cache.max_bytes > 0 {
        warn!(
            cache_max_bytes = cfg.cache.max_bytes,
            portals = portals.len(),
            "multiple portals advertised with chunk cache enabled: safe for a single process (dual-NIC); if another iscsi-s3 instance serves the same volume prefix, set cache.max_bytes = 0 on every peer"
        );
    }

    let mut builder = IscsiServer::builder().bind_addr(&cfg.bind);
    if !portals.is_empty() {
        builder = builder.portal_addrs(portals.clone());
    }

    let mut volume_labels: Vec<VolumeLabels> = Vec::new();
    let mut volume_summaries: Vec<VolumeSummary> = Vec::new();

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
        let capacity = opened.store.capacity();
        volume_labels.push(opened.labels.clone());
        let device = S3BlockDevice::new(opened.store, opened.labels, Arc::clone(&metrics));
        let auth = resolve_auth(&cfg.auth, &vol.auth)?;
        volume_summaries.push(VolumeSummary {
            name: opened.name.clone(),
            iqn: opened.iqn.clone(),
            capacity,
            auth: auth.mode.as_str().to_string(),
            prefix: vol.prefix.trim_matches('/').to_string(),
            chunk_size: vol.chunk_size,
            compression: vol.compression.as_str().to_string(),
        });
        info!(
            name = %opened.name,
            iqn = %opened.iqn,
            bind = %cfg.bind,
            instance = cfg.instance.as_deref().unwrap_or("-"),
            portals = ?portals,
            capacity,
            auth = auth.mode.as_str(),
            chap_user = auth.username.as_deref().unwrap_or("-"),
            "volume ready"
        );
        builder = builder.add_target_with_auth(
            opened.iqn,
            Box::new(device),
            Some(opened.name),
            auth.config,
            auth.allowed_initiators,
        );
    }

    builder = builder.session_events(SessionMetricsSink::new(
        Arc::clone(&metrics),
        volume_labels,
    ));

    let server = Arc::new(builder.build().map_err(|e| e.to_string())?);
    info!(
        bind = %cfg.bind,
        instance = cfg.instance.as_deref().unwrap_or("-"),
        portals = ?portals,
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

    if cfg.admin.enabled {
        let admin_state = Arc::new(AdminState {
            config_path,
            started: Instant::now(),
            cache: Arc::clone(&cache),
            server: Arc::clone(&server),
            s3_client: client.clone(),
            runtime: handle.clone(),
            snapshot: Mutex::new(AdminSnapshot {
                bind: cfg.bind.clone(),
                portals: portals.clone(),
                instance: cfg.instance.clone(),
                volumes: volume_summaries,
                cache_max_bytes: cfg.cache.max_bytes,
                s3_bucket: cfg.s3.bucket.clone(),
                s3_endpoint: cfg.s3.endpoint.clone(),
                s3_region: cfg.s3.region.clone(),
                s3_force_path_style: cfg.s3.force_path_style,
            }),
        });
        spawn_admin_server(PathBuf::from(&cfg.admin.socket), admin_state)?;
    } else {
        info!("admin control socket disabled");
    }

    // Keep the tokio runtime alive for S3 I/O while the target runs.
    let _runtime_guard = runtime;
    server.run().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve_advertise, resolve_portals};
    use iscsi_s3::config::Config;

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

    #[test]
    fn portals_list_takes_precedence() {
        let cfg = Config {
            bind: "0.0.0.0:3260".into(),
            advertise: Some("10.0.0.1".into()),
            portals: vec!["10.0.0.1".into(), "10.0.0.2:3260".into()],
            instance: Some("a".into()),
            auth: None,
            s3: Default::default(),
            cache: Default::default(),
            metrics: Default::default(),
            admin: Default::default(),
            volumes: vec![],
        };
        assert_eq!(
            resolve_portals(&cfg).unwrap(),
            vec!["10.0.0.1:3260".to_string(), "10.0.0.2:3260".to_string()]
        );
    }
}
