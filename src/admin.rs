//! Local Unix-domain admin control plane for `iscsi-s3-ctl`.

use crate::cache::ChunkCache;
use crate::config::{parse_byte_size, Config};
use iscsi_target::IscsiServer;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize)]
pub struct VolumeSummary {
    pub name: String,
    pub iqn: String,
    pub capacity: u64,
    pub auth: String,
}

pub struct AdminState {
    pub config_path: Option<PathBuf>,
    pub started: Instant,
    pub cache: Arc<ChunkCache>,
    pub server: Arc<IscsiServer>,
    /// Labels / config snapshot updated on safe reload.
    pub snapshot: Mutex<AdminSnapshot>,
}

#[derive(Debug, Clone)]
pub struct AdminSnapshot {
    pub bind: String,
    pub portals: Vec<String>,
    pub instance: Option<String>,
    pub volumes: Vec<VolumeSummary>,
    pub cache_max_bytes: u64,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    pub s3_force_path_style: bool,
}

#[derive(Debug, Deserialize)]
struct AdminRequest {
    op: String,
    #[serde(default)]
    max_bytes: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct RejectedChange {
    pub field: String,
    pub reason: String,
}

pub fn spawn_admin_server(socket: PathBuf, state: Arc<AdminState>) -> Result<(), String> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create admin socket dir {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("admin bind {}: {e}", socket.display()))?;
    // Restrict to owner when possible.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660));
    }
    info!(socket = %socket.display(), "admin control socket listening");

    thread::Builder::new()
        .name("iscsi-s3-admin".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let state = Arc::clone(&state);
                        if let Err(e) = handle_client(stream, &state) {
                            warn!(error = %e, "admin client error");
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "admin accept failed");
                    }
                }
            }
        })
        .map_err(|e| format!("spawn admin thread: {e}"))?;
    Ok(())
}

fn handle_client(stream: UnixStream, state: &AdminState) -> Result<(), String> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let req: AdminRequest =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad request json: {e}"))?;
    let response = dispatch(state, &req);
    let mut writer = stream;
    let body = serde_json::to_string(&response).map_err(|e| e.to_string())?;
    writer
        .write_all(body.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    writer.write_all(b"\n").map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn dispatch(state: &AdminState, req: &AdminRequest) -> serde_json::Value {
    match req.op.as_str() {
        "stats" | "cache.status" => ok(stats_json(state)),
        "cache.disable" => {
            state.cache.disable();
            let mut snap = state.snapshot.lock();
            snap.cache_max_bytes = 0;
            ok(cache_json(state))
        }
        "cache.enable" => match parse_optional_max_bytes(req.max_bytes.as_ref()) {
            Ok(mb) => {
                state.cache.enable(mb);
                let mut snap = state.snapshot.lock();
                snap.cache_max_bytes = state.cache.max_bytes();
                ok(cache_json(state))
            }
            Err(e) => err(e),
        },
        "cache.set" => match parse_required_max_bytes(req.max_bytes.as_ref()) {
            Ok(mb) => {
                if mb == 0 {
                    state.cache.disable();
                } else {
                    state.cache.enable(Some(mb));
                }
                let mut snap = state.snapshot.lock();
                snap.cache_max_bytes = state.cache.max_bytes();
                ok(cache_json(state))
            }
            Err(e) => err(e),
        },
        "reload" => match reload(state) {
            Ok(v) => ok(v),
            Err(e) => err(e),
        },
        other => err(format!("unknown op: {other}")),
    }
}

fn ok(data: serde_json::Value) -> serde_json::Value {
    json!({ "ok": true, "data": data })
}

fn err(message: impl Into<String>) -> serde_json::Value {
    json!({ "ok": false, "error": message.into() })
}

fn cache_json(state: &AdminState) -> serde_json::Value {
    let (used, entries) = state.cache.stats();
    json!({
        "enabled": state.cache.is_enabled(),
        "max_bytes": state.cache.max_bytes(),
        "last_nonzero_max_bytes": state.cache.last_nonzero_max_bytes(),
        "used_bytes": used,
        "entries": entries,
    })
}

fn stats_json(state: &AdminState) -> serde_json::Value {
    let snap = state.snapshot.lock().clone();
    let uptime = state.started.elapsed();
    json!({
        "uptime_secs": uptime.as_secs(),
        "uptime": format_duration(uptime),
        "config_path": state.config_path.as_ref().map(|p| p.display().to_string()),
        "bind": snap.bind,
        "portals": snap.portals,
        "instance": snap.instance,
        "volumes": snap.volumes,
        "cache": cache_json(state),
        "iscsi": {
            "connections": state.server.active_connection_count(),
            "sessions": state.server.active_session_count(),
        },
    })
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h}h{m}m{s}s")
}

fn parse_optional_max_bytes(v: Option<&serde_json::Value>) -> Result<Option<u64>, String> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(val) => Ok(Some(parse_max_bytes_value(val)?)),
    }
}

fn parse_required_max_bytes(v: Option<&serde_json::Value>) -> Result<u64, String> {
    let Some(val) = v else {
        return Err("max_bytes is required".into());
    };
    parse_max_bytes_value(val)
}

fn parse_max_bytes_value(val: &serde_json::Value) -> Result<u64, String> {
    match val {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| "max_bytes must be a non-negative integer".into()),
        serde_json::Value::String(s) => parse_byte_size(s),
        _ => Err("max_bytes must be a number or size string".into()),
    }
}

fn reload(state: &AdminState) -> Result<serde_json::Value, String> {
    let path = state
        .config_path
        .as_ref()
        .ok_or_else(|| "no config file path (started without --config); cannot reload".to_string())?;
    let new_cfg = Config::load_for_reload(path).map_err(|e| e.to_string())?;
    let snap = state.snapshot.lock().clone();
    let result = apply_safe_reload(&snap, &new_cfg, &state.cache)?;
    {
        let mut snap = state.snapshot.lock();
        snap.cache_max_bytes = state.cache.max_bytes();
        // Portals/advertise labels for stats only (discovery still uses startup portals).
        snap.portals = if !new_cfg.portals.is_empty() {
            new_cfg.portals.clone()
        } else if let Some(ref a) = new_cfg.advertise {
            vec![a.clone()]
        } else {
            snap.portals.clone()
        };
        snap.instance = new_cfg.instance.clone();
        if result.bind_unchanged {
            // keep bind
        }
    }
    Ok(json!({
        "applied": result.applied,
        "rejected": result.rejected,
        "warnings": result.warnings,
        "cache": cache_json(state),
    }))
}

#[derive(Debug)]
pub struct ReloadResult {
    pub applied: Vec<String>,
    pub rejected: Vec<RejectedChange>,
    pub warnings: Vec<String>,
    pub bind_unchanged: bool,
}

/// Diff + apply safe subset. Pure enough for unit tests (cache side effects).
pub fn apply_safe_reload(
    current: &AdminSnapshot,
    new_cfg: &Config,
    cache: &ChunkCache,
) -> Result<ReloadResult, String> {
    let mut applied = Vec::new();
    let mut rejected = Vec::new();
    let mut warnings = Vec::new();

    if new_cfg.bind != current.bind {
        rejected.push(RejectedChange {
            field: "bind".into(),
            reason: "listen address cannot change without restart".into(),
        });
    }

    if volumes_structurally_changed(current, new_cfg) {
        rejected.push(RejectedChange {
            field: "volumes".into(),
            reason: "volume add/remove/iqn/prefix/geometry changes require restart".into(),
        });
    }

    if new_cfg.s3.bucket != current.s3_bucket
        || new_cfg.s3.endpoint != current.s3_endpoint
        || new_cfg.s3.region != current.s3_region
        || new_cfg.s3.force_path_style != current.s3_force_path_style
    {
        rejected.push(RejectedChange {
            field: "s3".into(),
            reason: "S3 bucket/endpoint/region/path-style changes require restart".into(),
        });
    }

    if new_cfg.auth.is_some()
        || new_cfg.volumes.iter().any(|v| v.auth.is_some())
        || current.volumes.iter().any(|v| v.auth != "none")
    {
        rejected.push(RejectedChange {
            field: "auth".into(),
            reason: "CHAP / ACL changes require restart".into(),
        });
    }

    if new_cfg.cache.max_bytes != current.cache_max_bytes {
        if new_cfg.cache.max_bytes == 0 {
            cache.disable();
        } else {
            cache.enable(Some(new_cfg.cache.max_bytes));
        }
        applied.push(format!("cache.max_bytes={}", new_cfg.cache.max_bytes));
    }

    let new_portals: Vec<String> = if !new_cfg.portals.is_empty() {
        new_cfg.portals.clone()
    } else if let Some(ref a) = new_cfg.advertise {
        vec![a.clone()]
    } else {
        current.portals.clone()
    };
    if new_portals != current.portals {
        warnings.push(
            "portals/advertise updated in stats labels only; SendTargets still uses addresses from process start until restart"
                .into(),
        );
        applied.push("portals/advertise@stats".into());
    }

    if !rejected.is_empty() && applied.is_empty() {
        // Still ok — report rejections
    }

    Ok(ReloadResult {
        applied,
        rejected,
        warnings,
        bind_unchanged: new_cfg.bind == current.bind,
    })
}

fn volumes_structurally_changed(current: &AdminSnapshot, new_cfg: &Config) -> bool {
    if current.volumes.len() != new_cfg.volumes.len() {
        return true;
    }
    for (cur, vol) in current.volumes.iter().zip(new_cfg.volumes.iter()) {
        if cur.name != vol.name || cur.iqn != vol.iqn {
            return true;
        }
        // capacity grow is still restart for ctl v1 (store already open).
        if cur.capacity != vol.capacity {
            return true;
        }
    }
    // prefix/geometry not in VolumeSummary — if names/iqns match assume same set;
    // reject if any volume block_size/chunk_size/prefix would need compare.
    // Include prefix in summary next — for now compare lengths only + iqn/name/capacity.
    false
}

/// Send one JSON request and read one JSON response over a Unix socket.
pub fn call_admin(socket: &Path, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.write_all(b"\n").map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader
        .read_line(&mut resp)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(resp.trim()).map_err(|e| format!("bad response json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CACHE_MAX;

    #[test]
    fn apply_reload_cache_only() {
        let cache = ChunkCache::new(1024);
        let snap = AdminSnapshot {
            bind: "0.0.0.0:3260".into(),
            portals: vec![],
            instance: None,
            volumes: vec![VolumeSummary {
                name: "disk0".into(),
                iqn: "iqn.test:disk0".into(),
                capacity: 1024,
                auth: "none".into(),
            }],
            cache_max_bytes: 1024,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: "us-east-1".into(),
            s3_force_path_style: false,
        };
        let mut cfg = Config {
            bind: "0.0.0.0:3260".into(),
            advertise: None,
            portals: vec![],
            instance: None,
            auth: None,
            s3: Default::default(),
            cache: crate::config::CacheConfig { max_bytes: 0 },
            metrics: Default::default(),
            admin: Default::default(),
            volumes: vec![crate::config::VolumeConfig {
                name: "disk0".into(),
                iqn: "iqn.test:disk0".into(),
                prefix: "disks/disk0".into(),
                capacity: 1024,
                block_size: 512,
                chunk_size: 4096,
                auth: None,
                compression: Default::default(),
            }],
        };
        let r = apply_safe_reload(&snap, &cfg, &cache).unwrap();
        assert!(r.applied.iter().any(|a| a.contains("cache.max_bytes=0")));
        assert_eq!(cache.max_bytes(), 0);

        cfg.bind = "0.0.0.0:9999".into();
        cfg.cache.max_bytes = DEFAULT_CACHE_MAX;
        let r = apply_safe_reload(
            &AdminSnapshot {
                cache_max_bytes: 0,
                ..snap
            },
            &cfg,
            &cache,
        )
        .unwrap();
        assert!(r.rejected.iter().any(|x| x.field == "bind"));
    }

    #[test]
    fn parse_max_bytes_values() {
        assert_eq!(parse_max_bytes_value(&json!(0)).unwrap(), 0);
        assert_eq!(
            parse_max_bytes_value(&json!("1MiB")).unwrap(),
            1024 * 1024
        );
    }

    #[test]
    fn call_admin_uds_roundtrip() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        let sock_server = sock.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            let _ = sock_server;
            assert!(std::str::from_utf8(&buf[..n]).unwrap().contains("stats"));
            stream
                .write_all(br#"{"ok":true,"data":{"ping":"pong"}}"#)
                .unwrap();
            stream.write_all(b"\n").unwrap();
            tx.send(()).unwrap();
        });
        let resp = call_admin(&sock, &json!({"op":"stats"})).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["ping"], "pong");
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    }
}
