//! Control client for a running iscsi-s3 daemon (Unix admin socket).

use clap::{Parser, Subcommand, ValueEnum};
use iscsi_s3::admin::{call_admin, call_admin_write_image};
use iscsi_s3::config::{parse_byte_size, DEFAULT_ADMIN_SOCKET};
use serde_json::json;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(name = "iscsi-s3-ctl", about = "Manage a running iscsi-s3 instance")]
struct CtlCli {
    /// Admin Unix socket path (default: ISCSI_S3_ADMIN_SOCKET or built-in default)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Process / cache / session snapshot
    Stats,
    /// Cache subcommands
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Per-volume S3 / config queries
    Volume {
        #[command(subcommand)]
        action: VolumeCmd,
    },
    /// Re-read config file; apply safe fields only (cache.max_bytes, stats labels)
    Reload,
}

#[derive(Debug, Subcommand)]
enum CacheCmd {
    Status,
    Disable,
    Enable {
        #[arg(long)]
        max_bytes: Option<String>,
    },
    Set {
        #[arg(long)]
        max_bytes: String,
    },
}

#[derive(Debug, Subcommand)]
enum VolumeCmd {
    /// List configured volumes (name, IQN, capacity, prefix, …)
    List,
    /// List objects under the volume S3 prefix (chunk count, bytes used, …)
    #[command(name = "s3-stats", alias = "usage")]
    S3Stats {
        /// Volume name or IQN
        volume: String,
    },
    /// Write a raw disk image into an empty volume (no existing chunk objects)
    #[command(name = "write-image", alias = "seed")]
    WriteImage {
        /// Volume name or IQN
        volume: String,
        /// Path to a raw image (dd / .img); streamed over the admin socket
        #[arg(long, short)]
        file: PathBuf,
    },
    /// Copy one volume onto another (1:1 sparse overwrite)
    Copy {
        /// Source volume name or IQN
        from: String,
        /// Destination volume name or IQN (overwritten)
        to: String,
        /// Skip interactive confirmation
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Delete all chunk objects on a volume (keeps meta.json)
    Wipe {
        /// Volume name or IQN
        volume: String,
        /// Skip interactive confirmation
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Discover + login via open-iscsi (`iscsiadm`) on this host
    Connect {
        /// Volume name or IQN
        volume: String,
        /// Portal host:port (default: daemon portals / advertise)
        #[arg(long, short)]
        portal: Option<String>,
    },
    /// Logout via open-iscsi (`iscsiadm`) on this host
    Disconnect {
        /// Volume name or IQN
        volume: String,
        /// Portal host:port (default: logout all sessions for the IQN)
        #[arg(long, short)]
        portal: Option<String>,
    },
}

fn socket_path(cli: &CtlCli) -> PathBuf {
    if let Some(ref p) = cli.socket {
        return p.clone();
    }
    if let Ok(p) = std::env::var("ISCSI_S3_ADMIN_SOCKET") {
        return PathBuf::from(p);
    }
    PathBuf::from(DEFAULT_ADMIN_SOCKET)
}

fn main() -> ExitCode {
    let cli = CtlCli::parse();
    let sock = socket_path(&cli);

    if let Commands::Volume {
        action: VolumeCmd::WriteImage { volume, file },
    } = &cli.command
    {
        return run_write_image(&cli, &sock, volume, file);
    }

    if let Commands::Volume {
        action: VolumeCmd::Connect { volume, portal },
    } = &cli.command
    {
        return match run_volume_connect(&sock, volume, portal.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        };
    }

    if let Commands::Volume {
        action: VolumeCmd::Disconnect { volume, portal },
    } = &cli.command
    {
        return match run_volume_disconnect(&sock, volume, portal.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        };
    }

    if let Commands::Volume {
        action: VolumeCmd::Copy { from, to, force },
    } = &cli.command
    {
        if let Err(e) = confirm_yes(
            &format!(
                "This will OVERWRITE volume '{to}' with a 1:1 copy of '{from}'."
            ),
            *force,
            "volume copy",
        ) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }

    if let Commands::Volume {
        action: VolumeCmd::Wipe { volume, force },
    } = &cli.command
    {
        if let Err(e) = confirm_yes(
            &format!(
                "This will WIPE all chunk data on volume '{volume}' (meta.json is kept)."
            ),
            *force,
            "volume wipe",
        ) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }

    let req = match build_request(&cli.command) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match call_admin(&sock, &req) {
        Ok(resp) => finish_response(&cli, &resp),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn confirm_yes(message: &str, force: bool, action: &str) -> Result<(), String> {
    if force {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(format!(
            "refusing destructive {action} without --force when stdin is not a TTY"
        ));
    }
    eprint!("{message}\nType 'yes' to continue: ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("read confirmation: {e}"))?;
    if line.trim() == "yes" {
        Ok(())
    } else {
        Err("aborted (confirmation was not 'yes')".into())
    }
}

fn run_write_image(cli: &CtlCli, sock: &PathBuf, volume: &str, file: &PathBuf) -> ExitCode {
    let meta = match std::fs::metadata(file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: open {}: {e}", file.display());
            return ExitCode::from(2);
        }
    };
    let size = meta.len();
    let mut f = match File::open(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: open {}: {e}", file.display());
            return ExitCode::from(2);
        }
    };
    eprintln!(
        "seeding volume {volume} from {} ({size} bytes)…",
        file.display()
    );
    match call_admin_write_image(sock, volume, size, &mut f) {
        Ok(resp) => finish_response(cli, &resp),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn finish_response(cli: &CtlCli, resp: &serde_json::Value) -> ExitCode {
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    match cli.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(resp).unwrap_or_else(|_| resp.to_string())
            );
        }
        OutputFormat::Text => {
            if let Err(e) = print_text(&cli.command, resp) {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn build_request(cmd: &Commands) -> Result<serde_json::Value, String> {
    Ok(match cmd {
        Commands::Stats => json!({ "op": "stats" }),
        Commands::Reload => json!({ "op": "reload" }),
        Commands::Cache { action } => match action {
            CacheCmd::Status => json!({ "op": "cache.status" }),
            CacheCmd::Disable => json!({ "op": "cache.disable" }),
            CacheCmd::Enable { max_bytes } => {
                let mut req = json!({ "op": "cache.enable" });
                if let Some(s) = max_bytes {
                    let n = parse_byte_size(s)?;
                    req["max_bytes"] = json!(n);
                }
                req
            }
            CacheCmd::Set { max_bytes } => {
                let n = parse_byte_size(max_bytes)?;
                json!({ "op": "cache.set", "max_bytes": n })
            }
        },
        Commands::Volume { action } => match action {
            VolumeCmd::List => json!({ "op": "volume.list" }),
            VolumeCmd::S3Stats { volume } => {
                json!({ "op": "volume.s3_stats", "volume": volume })
            }
            VolumeCmd::WriteImage { .. } => {
                unreachable!("write-image uses call_admin_write_image")
            }
            VolumeCmd::Copy { from, to, .. } => {
                json!({ "op": "volume.copy", "volume": from, "to": to })
            }
            VolumeCmd::Wipe { volume, .. } => {
                json!({ "op": "volume.wipe", "volume": volume })
            }
            VolumeCmd::Connect { .. } | VolumeCmd::Disconnect { .. } => {
                unreachable!("connect/disconnect are local iscsiadm helpers")
            }
        },
    })
}

fn print_text(cmd: &Commands, resp: &serde_json::Value) -> Result<(), String> {
    if resp.get("ok") != Some(&json!(true)) {
        let err = resp
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("request failed");
        return Err(err.to_string());
    }
    let data = resp.get("data").cloned().unwrap_or(json!({}));
    match cmd {
        Commands::Stats | Commands::Cache { action: CacheCmd::Status } => {
            if matches!(cmd, Commands::Cache { .. }) {
                print_cache(&data);
            } else {
                print_stats(&data);
            }
        }
        Commands::Cache { .. } => {
            println!("cache updated");
            print_cache(&data);
        }
        Commands::Volume {
            action: VolumeCmd::List,
        } => {
            print_volume_list(&data);
        }
        Commands::Volume {
            action: VolumeCmd::S3Stats { .. },
        } => {
            print_volume_s3_stats(&data);
        }
        Commands::Volume {
            action: VolumeCmd::WriteImage { .. },
        } => {
            print_write_image(&data);
        }
        Commands::Volume {
            action: VolumeCmd::Copy { .. },
        } => {
            print_volume_copy(&data);
        }
        Commands::Volume {
            action: VolumeCmd::Wipe { .. },
        } => {
            print_volume_wipe(&data);
        }
        Commands::Volume {
            action: VolumeCmd::Connect { .. } | VolumeCmd::Disconnect { .. },
        } => {}
        Commands::Reload => {
            println!("reload complete");
            if let Some(arr) = data.get("applied").and_then(|a| a.as_array()) {
                println!("applied:");
                for a in arr {
                    println!("  - {}", a.as_str().unwrap_or("?"));
                }
            }
            if let Some(arr) = data.get("rejected").and_then(|a| a.as_array()) {
                if !arr.is_empty() {
                    println!("rejected:");
                    for r in arr {
                        println!(
                            "  - {}: {}",
                            r.get("field").and_then(|f| f.as_str()).unwrap_or("?"),
                            r.get("reason").and_then(|f| f.as_str()).unwrap_or("?")
                        );
                    }
                }
            }
            if let Some(arr) = data.get("warnings").and_then(|a| a.as_array()) {
                for w in arr {
                    println!("warning: {}", w.as_str().unwrap_or("?"));
                }
            }
            if let Some(c) = data.get("cache") {
                print_cache(c);
            }
        }
    }
    Ok(())
}

fn print_stats(data: &serde_json::Value) {
    println!(
        "uptime: {}",
        data.get("uptime").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "bind: {}",
        data.get("bind").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "instance: {}",
        data.get("instance")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "config: {}",
        data.get("config_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(none)")
    );
    if let Some(p) = data.get("portals").and_then(|v| v.as_array()) {
        let list: Vec<_> = p.iter().filter_map(|x| x.as_str()).collect();
        println!("portals: {}", list.join(", "));
    }
    if let Some(iscsi) = data.get("iscsi") {
        println!(
            "iscsi connections: {}",
            iscsi
                .get("connections")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
        println!(
            "iscsi sessions: {}",
            iscsi.get("sessions").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    if let Some(vols) = data.get("volumes").and_then(|v| v.as_array()) {
        println!("volumes:");
        for v in vols {
            println!(
                "  - {} ({}) capacity={} auth={} prefix={} compression={}",
                v.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("iqn").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("capacity").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("auth").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("prefix").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("compression")
                    .and_then(|x| x.as_str())
                    .unwrap_or("none")
            );
        }
    }
    if let Some(c) = data.get("cache") {
        print_cache(c);
    }
}

fn print_cache(c: &serde_json::Value) {
    println!(
        "cache: enabled={} max_bytes={} used_bytes={} entries={}",
        c.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        c.get("max_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        c.get("used_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        c.get("entries").and_then(|v| v.as_u64()).unwrap_or(0)
    );
}

fn print_volume_list(data: &serde_json::Value) {
    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("volumes: {count}");
    if let Some(vols) = data.get("volumes").and_then(|v| v.as_array()) {
        for v in vols {
            println!(
                "  - {}  iqn={}  capacity={}  prefix={}  compression={}  auth={}",
                v.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("iqn").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("capacity").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("prefix").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("compression")
                    .and_then(|x| x.as_str())
                    .unwrap_or("none"),
                v.get("auth").and_then(|x| x.as_str()).unwrap_or("?")
            );
        }
    }
}

fn print_volume_s3_stats(data: &serde_json::Value) {
    println!(
        "volume: {} ({})",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("iqn").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "s3: s3://{}/{}",
        data.get("bucket").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("prefix").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "logical_capacity_bytes: {}  chunk_size: {}  compression: {}",
        data.get("logical_capacity_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("chunk_size").and_then(|v| v.as_u64()).unwrap_or(0),
        data.get("compression")
            .and_then(|v| v.as_str())
            .unwrap_or("none")
    );
    if let Some(o) = data.get("objects") {
        println!(
            "objects: total={} chunks={} meta={} other={}",
            o.get("total").and_then(|v| v.as_u64()).unwrap_or(0),
            o.get("chunks").and_then(|v| v.as_u64()).unwrap_or(0),
            o.get("meta").and_then(|v| v.as_u64()).unwrap_or(0),
            o.get("other").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    if let Some(b) = data.get("bytes") {
        println!(
            "bytes: total={} chunks={} meta={} other={}",
            b.get("total").and_then(|v| v.as_u64()).unwrap_or(0),
            b.get("chunks").and_then(|v| v.as_u64()).unwrap_or(0),
            b.get("meta").and_then(|v| v.as_u64()).unwrap_or(0),
            b.get("other").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    if let Some(n) = data.get("note").and_then(|v| v.as_str()) {
        println!("note: {n}");
    }
}

fn print_write_image(data: &serde_json::Value) {
    // Ready-only responses should not reach here on success path; final has bytes_read.
    if data.get("ready") == Some(&json!(true)) {
        println!(
            "ready to receive {} bytes for volume {}",
            data.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            data.get("volume").and_then(|v| v.as_str()).unwrap_or("?")
        );
        return;
    }
    println!(
        "seeded volume {} ({})",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("iqn").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "bytes_read={}  bytes_stored={}  chunks_written={}  zero_chunks_skipped={}",
        data.get("bytes_read").and_then(|v| v.as_u64()).unwrap_or(0),
        data.get("bytes_stored")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("chunks_written")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("zero_chunks_skipped")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );
    if let Some(n) = data.get("note").and_then(|v| v.as_str()) {
        println!("note: {n}");
    }
}

fn print_volume_copy(data: &serde_json::Value) {
    let from = data.get("from");
    let to = data.get("to");
    println!(
        "copied {} → {}",
        from.and_then(|v| v.get("volume"))
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        to.and_then(|v| v.get("volume"))
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "chunks_copied={}  chunks_deleted={}  bytes_copied={}",
        data.get("chunks_copied")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("chunks_deleted")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("bytes_copied")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );
    if let Some(n) = data.get("note").and_then(|v| v.as_str()) {
        println!("note: {n}");
    }
}

fn print_volume_wipe(data: &serde_json::Value) {
    println!(
        "wiped volume {} ({})",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("iqn").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "chunks_deleted={}",
        data.get("chunks_deleted")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );
    if let Some(n) = data.get("note").and_then(|v| v.as_str()) {
        println!("note: {n}");
    }
}

struct VolumeAttachInfo {
    name: String,
    iqn: String,
    auth: String,
    portals: Vec<String>,
}

fn lookup_volume_attach_from_stats(
    data: &serde_json::Value,
    volume: &str,
) -> Result<VolumeAttachInfo, String> {
    let vols = data
        .get("volumes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "stats response missing volumes".to_string())?;
    let vol = vols
        .iter()
        .find(|v| {
            v.get("name").and_then(|x| x.as_str()) == Some(volume)
                || v.get("iqn").and_then(|x| x.as_str()) == Some(volume)
        })
        .ok_or_else(|| format!("unknown volume {volume:?}"))?;

    let portals_from_stats: Vec<String> = data
        .get("portals")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Ok(VolumeAttachInfo {
        name: vol
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(volume)
            .to_string(),
        iqn: vol
            .get("iqn")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "volume missing iqn".to_string())?
            .to_string(),
        auth: vol
            .get("auth")
            .and_then(|x| x.as_str())
            .unwrap_or("none")
            .to_string(),
        portals: portals_from_stats,
    })
}

fn fetch_volume_attach(sock: &PathBuf, volume: &str) -> Result<(serde_json::Value, VolumeAttachInfo), String> {
    let resp = call_admin(sock, &json!({ "op": "stats" }))?;
    if resp.get("ok") != Some(&json!(true)) {
        return Err(resp
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("stats failed")
            .to_string());
    }
    let data = resp.get("data").cloned().unwrap_or(json!({}));
    let info = lookup_volume_attach_from_stats(&data, volume)?;
    Ok((data, info))
}

fn resolve_portals(
    info: &VolumeAttachInfo,
    portal_override: Option<&str>,
    bind: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(p) = portal_override {
        return Ok(vec![normalize_portal(p)]);
    }
    if !info.portals.is_empty() {
        return Ok(info.portals.iter().map(|p| normalize_portal(p)).collect());
    }
    if let Some(bind) = bind {
        if !is_wildcard_bind(bind) {
            return Ok(vec![normalize_portal(bind)]);
        }
    }
    Err(
        "no usable portal: set portals/advertise on the daemon, or pass --portal HOST:PORT".into(),
    )
}

fn normalize_portal(portal: &str) -> String {
    let p = portal.trim();
    if p.contains(':') {
        p.to_string()
    } else {
        format!("{p}:3260")
    }
}

fn is_wildcard_bind(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let host = host.trim_matches(|c| c == '[' || c == ']');
    host == "0.0.0.0" || host == "::" || host == "*"
}

fn run_iscsiadm(args: &[&str]) -> Result<(), String> {
    eprintln!("+ iscsiadm {}", args.join(" "));
    let status = Command::new("iscsiadm")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| {
            format!("failed to run iscsiadm ({e}); is open-iscsi installed and in PATH?")
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "iscsiadm {} failed with {}",
            args.join(" "),
            status
        ))
    }
}

fn run_volume_connect(
    sock: &PathBuf,
    volume: &str,
    portal_override: Option<&str>,
) -> Result<(), String> {
    let (data, info) = fetch_volume_attach(sock, volume)?;
    let bind = data.get("bind").and_then(|v| v.as_str());
    let portals = resolve_portals(&info, portal_override, bind)?;

    if info.auth != "none" {
        eprintln!(
            "note: volume {} uses auth={}; configure CHAP on the node if login fails \
             (see docs/users/configuration.md)",
            info.name, info.auth
        );
    }

    // Discover via the first portal (SendTargets returns the rest).
    run_iscsiadm(&[
        "-m",
        "discovery",
        "-t",
        "sendtargets",
        "-p",
        &portals[0],
    ])?;

    for portal in &portals {
        run_iscsiadm(&["-m", "node", "-T", &info.iqn, "-p", portal, "--login"])?;
    }

    println!(
        "connected {} ({}) via {}",
        info.name,
        info.iqn,
        portals.join(", ")
    );
    Ok(())
}

fn run_volume_disconnect(
    sock: &PathBuf,
    volume: &str,
    portal_override: Option<&str>,
) -> Result<(), String> {
    let (_data, info) = fetch_volume_attach(sock, volume)?;
    if let Some(portal) = portal_override {
        let portal = normalize_portal(portal);
        run_iscsiadm(&[
            "-m",
            "node",
            "-T",
            &info.iqn,
            "-p",
            &portal,
            "--logout",
        ])?;
        println!("disconnected {} ({}) from {portal}", info.name, info.iqn);
    } else {
        // Logout every session for this target IQN.
        run_iscsiadm(&["-m", "node", "-T", &info.iqn, "-u"])?;
        println!("disconnected {} ({}) (all portals)", info.name, info.iqn);
    }
    Ok(())
}
