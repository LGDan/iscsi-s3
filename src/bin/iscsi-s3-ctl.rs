//! Control client for a running iscsi-s3 daemon (Unix admin socket).

use clap::{Parser, Subcommand, ValueEnum};
use iscsi_s3::admin::{call_admin, call_admin_export, call_admin_write_image};
use iscsi_s3::config::{parse_byte_size, DEFAULT_ADMIN_SOCKET};
use iscsi_s3::identity::{naa_from_iqn, serial_from_iqn};
use serde_json::json;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
    /// Liveness + S3 reachability
    Health,
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
        /// Resume from this chunk index (skip copying source chunks with a lower index)
        #[arg(long = "resume-from")]
        resume_from: Option<u64>,
    },
    /// Delete all chunk objects on a volume (keeps meta.json)
    Wipe {
        /// Volume name or IQN
        volume: String,
        /// Skip interactive confirmation
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Grow volume capacity (grow-only; updates meta.json)
    Grow {
        /// Volume name or IQN
        volume: String,
        /// New capacity (e.g. 20GiB or byte count)
        #[arg(long, short)]
        capacity: String,
    },
    /// List active FullFeature iSCSI sessions
    Sessions {
        /// Optional volume name or IQN filter
        volume: Option<String>,
    },
    /// Export a raw disk image from a volume
    Export {
        /// Volume name or IQN
        volume: String,
        /// Output path for the raw image
        #[arg(long, short)]
        file: PathBuf,
        /// Bytes to export (default: full capacity)
        #[arg(long)]
        size: Option<String>,
    },
    /// Discover + login via open-iscsi (`iscsiadm`) on this host
    Connect {
        /// Volume name or IQN
        volume: String,
        /// Portal host:port (default: daemon portals / advertise)
        #[arg(long, short)]
        portal: Option<String>,
        /// CHAP username (or ISCSI_S3_CHAP_USERNAME)
        #[arg(long)]
        username: Option<String>,
        /// CHAP password (or ISCSI_S3_CHAP_PASSWORD / ISCSI_S3_CHAP_SECRET)
        #[arg(long)]
        password: Option<String>,
        /// Mutual CHAP username (or ISCSI_S3_CHAP_MUTUAL_USERNAME)
        #[arg(long)]
        mutual_username: Option<String>,
        /// Mutual CHAP password (or ISCSI_S3_CHAP_MUTUAL_PASSWORD)
        #[arg(long)]
        mutual_password: Option<String>,
    },
    /// Logout via open-iscsi (`iscsiadm`) on this host
    Disconnect {
        /// Volume name or IQN
        volume: String,
        /// Portal host:port (default: logout all sessions for the IQN)
        #[arg(long, short)]
        portal: Option<String>,
    },
    /// Show local block device path(s) for a connected volume
    Device {
        /// Volume name or IQN
        volume: String,
        /// Seconds to wait for udev/by-path to appear (default 5)
        #[arg(long, default_value = "5")]
        wait: u64,
    },
    /// Snapshot create / list / delete / restore / clone (storage=cow volumes)
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
    /// Migrate a legacy volume to COW layout (then set storage=\"cow\" and restart)
    #[command(name = "migrate-cow")]
    MigrateCow {
        /// Volume name or IQN
        volume: String,
        /// Skip interactive confirmation
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SnapshotCmd {
    /// Create a snapshot from live pointers
    Create {
        /// Volume name or IQN
        volume: String,
        /// Snapshot id (default: snap-<unix>)
        #[arg(long, short)]
        name: Option<String>,
        /// Allow create with active sessions (crash-consistent best-effort)
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// List snapshot headers for a volume
    List {
        /// Volume name or IQN
        volume: String,
    },
    /// Delete a snapshot and GC unreferenced objects
    Delete {
        /// Volume name or IQN
        volume: String,
        /// Snapshot id
        id: String,
        /// Reserved for confirmation patterns (delete is metadata+GC)
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Restore live volume from a snapshot
    Restore {
        /// Volume name or IQN
        volume: String,
        /// Snapshot id
        id: String,
        /// Allow restore with active sessions
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Clone a snapshot into an empty destination volume
    Clone {
        /// Source volume name or IQN
        volume: String,
        /// Snapshot id
        id: String,
        /// Destination volume name or IQN (must be empty, storage=cow)
        #[arg(long)]
        to: String,
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
        action:
            VolumeCmd::Export {
                volume,
                file,
                size,
            },
    } = &cli.command
    {
        return run_export(&cli, &sock, volume, file, size.as_deref());
    }

    if let Commands::Volume {
        action:
            VolumeCmd::Connect {
                volume,
                portal,
                username,
                password,
                mutual_username,
                mutual_password,
            },
    } = &cli.command
    {
        let chap = ChapCredentials::resolve(
            username.as_deref(),
            password.as_deref(),
            mutual_username.as_deref(),
            mutual_password.as_deref(),
        );
        return match run_volume_connect(&sock, volume, portal.as_deref(), chap) {
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
        action: VolumeCmd::Device { volume, wait },
    } = &cli.command
    {
        return match run_volume_device(&sock, volume, *wait) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        };
    }

    if let Commands::Volume {
        action:
            VolumeCmd::Copy {
                from,
                to,
                force,
                resume_from,
            },
    } = &cli.command
    {
        let resume_note = resume_from
            .map(|i| format!(" Resuming from chunk index {i}."))
            .unwrap_or_default();
        if let Err(e) = confirm_yes(
            &format!(
                "This will OVERWRITE volume '{to}' with a 1:1 copy of '{from}'.{resume_note}"
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

    if let Commands::Volume {
        action:
            VolumeCmd::Snapshot {
                action:
                    SnapshotCmd::Delete {
                        volume,
                        id,
                        force,
                    },
            },
    } = &cli.command
    {
        if let Err(e) = confirm_yes(
            &format!("This will DELETE snapshot '{id}' on volume '{volume}' and GC unreferenced objects."),
            *force,
            "snapshot delete",
        ) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }

    if let Commands::Volume {
        action:
            VolumeCmd::Snapshot {
                action:
                    SnapshotCmd::Restore {
                        volume,
                        id,
                        force,
                    },
            },
    } = &cli.command
    {
        if let Err(e) = confirm_yes(
            &format!(
                "This will RESTORE volume '{volume}' from snapshot '{id}' (live data replaced)."
            ),
            *force,
            "snapshot restore",
        ) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }

    if let Commands::Volume {
        action: VolumeCmd::MigrateCow { volume, force },
    } = &cli.command
    {
        if let Err(e) = confirm_yes(
            &format!(
                "This will MIGRATE volume '{volume}' from legacy flat chunks to COW (objects + pointers)."
            ),
            *force,
            "migrate-cow",
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

fn run_export(
    cli: &CtlCli,
    sock: &PathBuf,
    volume: &str,
    file: &PathBuf,
    size: Option<&str>,
) -> ExitCode {
    let size = match size {
        Some(s) => match parse_byte_size(s) {
            Ok(n) => Some(n),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let mut f = match File::create(file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: create {}: {e}", file.display());
            return ExitCode::from(2);
        }
    };
    eprintln!(
        "exporting volume {volume} to {}…",
        file.display()
    );
    match call_admin_export(sock, volume, size, &mut f) {
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
    let health_degraded = matches!(cli.command, Commands::Health)
        && resp
            .pointer("/data/status")
            .and_then(|v| v.as_str())
            != Some("ok");
    if ok && !health_degraded {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn build_request(cmd: &Commands) -> Result<serde_json::Value, String> {
    Ok(match cmd {
        Commands::Stats => json!({ "op": "stats" }),
        Commands::Health => json!({ "op": "health" }),
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
            VolumeCmd::Export { .. } => {
                unreachable!("export uses call_admin_export")
            }
            VolumeCmd::Copy {
                from,
                to,
                resume_from,
                ..
            } => {
                let mut req = json!({ "op": "volume.copy", "volume": from, "to": to });
                if let Some(idx) = resume_from {
                    req["resume_from"] = json!(idx);
                }
                req
            }
            VolumeCmd::Wipe { volume, .. } => {
                json!({ "op": "volume.wipe", "volume": volume })
            }
            VolumeCmd::Grow { volume, capacity } => {
                let n = parse_byte_size(capacity)?;
                json!({ "op": "volume.grow", "volume": volume, "capacity": n })
            }
            VolumeCmd::Sessions { volume } => {
                let mut req = json!({ "op": "volume.sessions" });
                if let Some(v) = volume {
                    req["volume"] = json!(v);
                }
                req
            }
            VolumeCmd::Snapshot { action } => match action {
                SnapshotCmd::Create {
                    volume,
                    name,
                    force,
                } => {
                    let mut req = json!({
                        "op": "volume.snapshot.create",
                        "volume": volume,
                        "force": force,
                    });
                    if let Some(n) = name {
                        req["id"] = json!(n);
                    }
                    req
                }
                SnapshotCmd::List { volume } => {
                    json!({ "op": "volume.snapshot.list", "volume": volume })
                }
                SnapshotCmd::Delete { volume, id, .. } => {
                    json!({
                        "op": "volume.snapshot.delete",
                        "volume": volume,
                        "id": id,
                    })
                }
                SnapshotCmd::Restore {
                    volume,
                    id,
                    force,
                } => {
                    json!({
                        "op": "volume.snapshot.restore",
                        "volume": volume,
                        "id": id,
                        "force": force,
                    })
                }
                SnapshotCmd::Clone { volume, id, to } => {
                    json!({
                        "op": "volume.snapshot.clone",
                        "volume": volume,
                        "id": id,
                        "to": to,
                    })
                }
            },
            VolumeCmd::MigrateCow { volume, .. } => {
                json!({ "op": "volume.migrate_cow", "volume": volume })
            }
            VolumeCmd::Connect { .. }
            | VolumeCmd::Disconnect { .. }
            | VolumeCmd::Device { .. } => {
                unreachable!("connect/disconnect/device are local helpers")
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
        Commands::Health => {
            print_health(&data);
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
            action: VolumeCmd::Export { .. },
        } => {
            print_volume_export(&data);
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
            action: VolumeCmd::Grow { .. },
        } => {
            print_volume_grow(&data);
        }
        Commands::Volume {
            action: VolumeCmd::Sessions { .. },
        } => {
            print_volume_sessions(&data);
        }
        Commands::Volume {
            action: VolumeCmd::Snapshot { action },
        } => match action {
            SnapshotCmd::Create { .. } => print_snapshot_create(&data),
            SnapshotCmd::List { .. } => print_snapshot_list(&data),
            SnapshotCmd::Delete { .. } => print_snapshot_delete(&data),
            SnapshotCmd::Restore { .. } => print_snapshot_restore(&data),
            SnapshotCmd::Clone { .. } => print_snapshot_clone(&data),
        },
        Commands::Volume {
            action: VolumeCmd::MigrateCow { .. },
        } => {
            print_migrate_cow(&data);
        }
        Commands::Volume {
            action:
                VolumeCmd::Connect { .. }
                | VolumeCmd::Disconnect { .. }
                | VolumeCmd::Device { .. },
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
                "  - {}  iqn={}  capacity={}  prefix={}  storage={}  compression={}  auth={}",
                v.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("iqn").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("capacity").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("prefix").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("storage").and_then(|x| x.as_str()).unwrap_or("legacy"),
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
    if let Some(idx) = data.get("resume_from").and_then(|v| v.as_u64()) {
        println!(
            "resume_from={}  chunks_skipped={}",
            idx,
            data.get("chunks_skipped")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
    }
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

fn print_volume_grow(data: &serde_json::Value) {
    println!(
        "grew volume {} ({})",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("iqn").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "capacity: {} → {}  changed={}",
        data.get("old_capacity")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("new_capacity")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        data.get("changed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    );
    if let Some(n) = data.get("note").and_then(|v| v.as_str()) {
        println!("note: {n}");
    }
}

fn print_snapshot_create(data: &serde_json::Value) {
    let snap = data.get("snapshot");
    println!(
        "created snapshot {} on volume {}",
        snap.and_then(|s| s.get("id")).and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "chunk_count={}  state={}",
        snap.and_then(|s| s.get("chunk_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        snap.and_then(|s| s.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
}

fn print_snapshot_list(data: &serde_json::Value) {
    println!(
        "volume {} (storage={}): {} snapshot(s)",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("storage").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("count").and_then(|v| v.as_u64()).unwrap_or(0)
    );
    if let Some(arr) = data.get("snapshots").and_then(|v| v.as_array()) {
        for s in arr {
            println!(
                "  - {}  created_at={}  chunks={}  state={}",
                s.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("created_at_unix")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                s.get("chunk_count").and_then(|v| v.as_u64()).unwrap_or(0),
                s.get("state").and_then(|v| v.as_str()).unwrap_or("?")
            );
        }
    }
}

fn print_snapshot_delete(data: &serde_json::Value) {
    println!(
        "deleted snapshot {} on volume {}",
        data.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?")
    );
    if let Some(gc) = data.get("gc") {
        println!(
            "gc: objects_deleted={}  objects_referenced={}",
            gc.get("objects_deleted")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            gc.get("objects_referenced")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
    }
}

fn print_snapshot_restore(data: &serde_json::Value) {
    println!(
        "restored volume {} from snapshot {}",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    if let Some(r) = data.get("restore") {
        println!(
            "pointers_written={}  pointers_deleted={}  capacity={}",
            r.get("pointers_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            r.get("pointers_deleted")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            r.get("capacity").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
}

fn print_snapshot_clone(data: &serde_json::Value) {
    println!(
        "cloned snapshot {} from {} → {}",
        data.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("from").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("to").and_then(|v| v.as_str()).unwrap_or("?")
    );
    if let Some(c) = data.get("clone") {
        println!(
            "pointers_written={}  objects_copied={}",
            c.get("pointers_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            c.get("objects_copied")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
    }
}

fn print_migrate_cow(data: &serde_json::Value) {
    println!(
        "migrated volume {} to storage=cow",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?")
    );
    if let Some(m) = data.get("migrate") {
        println!(
            "chunks_migrated={}  legacy_chunks_deleted={}",
            m.get("chunks_migrated")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            m.get("chunks_deleted")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
        if let Some(n) = m.get("note").and_then(|v| v.as_str()) {
            println!("note: {n}");
        }
    }
}

fn print_volume_sessions(data: &serde_json::Value) {
    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Some(iscsi) = data.get("iscsi") {
        println!(
            "iscsi connections={} sessions={}",
            iscsi
                .get("connections")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            iscsi.get("sessions").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    println!("sessions: {count}");
    if let Some(arr) = data.get("sessions").and_then(|v| v.as_array()) {
        for s in arr {
            println!(
                "  - volume={} target={} initiator={} peer={} age={}s",
                s.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("target_iqn").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("initiator_iqn")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                s.get("peer").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("started_secs_ago")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            );
        }
    }
}

fn print_volume_export(data: &serde_json::Value) {
    if data.get("ready") == Some(&json!(true)) {
        println!(
            "ready to send {} bytes for volume {}",
            data.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            data.get("volume").and_then(|v| v.as_str()).unwrap_or("?")
        );
        return;
    }
    println!(
        "exported volume {} ({})",
        data.get("volume").and_then(|v| v.as_str()).unwrap_or("?"),
        data.get("iqn").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "bytes_sent={}  chunks_read={}",
        data.get("bytes_sent").and_then(|v| v.as_u64()).unwrap_or(0),
        data.get("chunks_read")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );
}

fn print_health(data: &serde_json::Value) {
    println!(
        "status: {}",
        data.get("status").and_then(|v| v.as_str()).unwrap_or("?")
    );
    println!(
        "uptime: {}",
        data.get("uptime").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "bind: {}",
        data.get("bind").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "volumes: {}",
        data.get("volumes").and_then(|v| v.as_u64()).unwrap_or(0)
    );
    if let Some(iscsi) = data.get("iscsi") {
        println!(
            "iscsi connections={} sessions={}",
            iscsi
                .get("connections")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            iscsi.get("sessions").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    if let Some(c) = data.get("cache") {
        print_cache(c);
    }
    if let Some(s3) = data.get("s3") {
        if s3.get("ok") == Some(&json!(true)) {
            println!(
                "s3: ok bucket={} latency_ms={}",
                s3.get("bucket").and_then(|v| v.as_str()).unwrap_or("?"),
                s3.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0)
            );
        } else {
            println!(
                "s3: FAIL bucket={} error={}",
                s3.get("bucket").and_then(|v| v.as_str()).unwrap_or("?"),
                s3.get("error").and_then(|v| v.as_str()).unwrap_or("?")
            );
        }
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

#[derive(Debug, Clone, Default)]
struct ChapCredentials {
    username: Option<String>,
    password: Option<String>,
    mutual_username: Option<String>,
    mutual_password: Option<String>,
}

impl ChapCredentials {
    fn resolve(
        username: Option<&str>,
        password: Option<&str>,
        mutual_username: Option<&str>,
        mutual_password: Option<&str>,
    ) -> Self {
        fn env_or(cli: Option<&str>, keys: &[&str]) -> Option<String> {
            if let Some(v) = cli.map(str::trim).filter(|s| !s.is_empty()) {
                return Some(v.to_string());
            }
            for k in keys {
                if let Ok(v) = std::env::var(k) {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
            None
        }
        Self {
            username: env_or(username, &["ISCSI_S3_CHAP_USERNAME"]),
            password: env_or(
                password,
                &["ISCSI_S3_CHAP_PASSWORD", "ISCSI_S3_CHAP_SECRET"],
            ),
            mutual_username: env_or(mutual_username, &["ISCSI_S3_CHAP_MUTUAL_USERNAME"]),
            mutual_password: env_or(
                mutual_password,
                &["ISCSI_S3_CHAP_MUTUAL_PASSWORD", "ISCSI_S3_CHAP_MUTUAL_SECRET"],
            ),
        }
    }

    fn has_one_way(&self) -> bool {
        self.username.is_some() && self.password.is_some()
    }

    fn has_mutual(&self) -> bool {
        self.mutual_username.is_some() && self.mutual_password.is_some()
    }

    fn validate_for_volume_auth(&self, auth: &str) -> Result<(), String> {
        match auth {
            "none" => Ok(()),
            "chap" => {
                if self.has_one_way() {
                    Ok(())
                } else {
                    Err(
                        "volume requires CHAP: pass --username/--password or set \
                         ISCSI_S3_CHAP_USERNAME and ISCSI_S3_CHAP_PASSWORD"
                            .into(),
                    )
                }
            }
            "mutual-chap" => {
                if self.has_one_way() && self.has_mutual() {
                    Ok(())
                } else {
                    Err(
                        "volume requires mutual CHAP: pass --username/--password and \
                         --mutual-username/--mutual-password (or matching ISCSI_S3_CHAP_* env vars)"
                            .into(),
                    )
                }
            }
            other => {
                // Unknown label from older daemons — require one-way if not none.
                if other != "none" && !self.has_one_way() {
                    Err(format!(
                        "volume auth={other}: pass --username/--password or ISCSI_S3_CHAP_* env"
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}

fn configure_node_chap(iqn: &str, portal: &str, chap: &ChapCredentials) -> Result<(), String> {
    if !chap.has_one_way() {
        return Ok(());
    }
    let user = chap.username.as_deref().unwrap();
    let pass = chap.password.as_deref().unwrap();
    run_iscsiadm(&[
        "-m",
        "node",
        "-T",
        iqn,
        "-p",
        portal,
        "--op",
        "update",
        "-n",
        "node.session.auth.authmethod",
        "-v",
        "CHAP",
    ])?;
    run_iscsiadm(&[
        "-m",
        "node",
        "-T",
        iqn,
        "-p",
        portal,
        "--op",
        "update",
        "-n",
        "node.session.auth.username",
        "-v",
        user,
    ])?;
    run_iscsiadm(&[
        "-m",
        "node",
        "-T",
        iqn,
        "-p",
        portal,
        "--op",
        "update",
        "-n",
        "node.session.auth.password",
        "-v",
        pass,
    ])?;
    if chap.has_mutual() {
        let mu = chap.mutual_username.as_deref().unwrap();
        let mp = chap.mutual_password.as_deref().unwrap();
        run_iscsiadm(&[
            "-m",
            "node",
            "-T",
            iqn,
            "-p",
            portal,
            "--op",
            "update",
            "-n",
            "node.session.auth.username_in",
            "-v",
            mu,
        ])?;
        run_iscsiadm(&[
            "-m",
            "node",
            "-T",
            iqn,
            "-p",
            portal,
            "--op",
            "update",
            "-n",
            "node.session.auth.password_in",
            "-v",
            mp,
        ])?;
    }
    Ok(())
}

fn run_volume_connect(
    sock: &PathBuf,
    volume: &str,
    portal_override: Option<&str>,
    chap: ChapCredentials,
) -> Result<(), String> {
    let (data, info) = fetch_volume_attach(sock, volume)?;
    let bind = data.get("bind").and_then(|v| v.as_str());
    let portals = resolve_portals(&info, portal_override, bind)?;
    chap.validate_for_volume_auth(&info.auth)?;

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
        configure_node_chap(&info.iqn, portal, &chap)?;
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

#[derive(Debug)]
struct LocalDevice {
    by_path: PathBuf,
    block: PathBuf,
    mapper: Option<PathBuf>,
}

fn run_volume_device(sock: &PathBuf, volume: &str, wait_secs: u64) -> Result<(), String> {
    let (_data, info) = fetch_volume_attach(sock, volume)?;
    let serial = serial_from_iqn(&info.iqn);
    let naa = naa_from_iqn(&info.iqn);
    let naa_hex: String = naa.iter().map(|b| format!("{b:02x}")).collect();

    let deadline = Instant::now() + Duration::from_secs(wait_secs.max(1));
    let devices = loop {
        let found = find_local_devices(&info.iqn)?;
        if !found.is_empty() || Instant::now() >= deadline {
            break found;
        }
        thread::sleep(Duration::from_millis(200));
    };

    if devices.is_empty() {
        return Err(format!(
            "no local device for {} ({}); is the volume connected? \
             looked under /dev/disk/by-path/*-iscsi-*-lun-*",
            info.name, info.iqn
        ));
    }

    println!("volume: {} ({})", info.name, info.iqn);
    println!("serial: {serial}");
    println!("naa: 0x{naa_hex}");
    for (i, d) in devices.iter().enumerate() {
        println!("path[{i}]: {}", d.by_path.display());
        println!("  block: {}", d.block.display());
        if let Some(ref m) = d.mapper {
            println!("  multipath: {}", m.display());
        }
    }
    // Prefer multipath map when present, else first block device.
    let preferred = devices
        .iter()
        .find_map(|d| d.mapper.as_ref())
        .unwrap_or(&devices[0].block);
    println!("device: {}", preferred.display());
    Ok(())
}

fn find_local_devices(iqn: &str) -> Result<Vec<LocalDevice>, String> {
    let dir = Path::new("/dev/disk/by-path");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // ip-…-iscsi-{iqn}-lun-N
        if !name.contains("-iscsi-") || !name.contains(iqn) || !name.contains("-lun-") {
            continue;
        }
        let by_path = entry.path();
        let block = std::fs::canonicalize(&by_path)
            .map_err(|e| format!("resolve {}: {e}", by_path.display()))?;
        let mapper = multipath_mapper_for_block(&block);
        out.push(LocalDevice {
            by_path,
            block,
            mapper,
        });
    }
    out.sort_by(|a, b| a.by_path.cmp(&b.by_path));
    out.dedup_by(|a, b| a.block == b.block);
    Ok(out)
}

fn multipath_mapper_for_block(block: &Path) -> Option<PathBuf> {
    // /sys/block/sdX/holders/dm-Y → /dev/mapper/* with dm-uuid-mpath-*
    let name = block.file_name()?.to_str()?;
    let holders = Path::new("/sys/block").join(name).join("holders");
    let entries = std::fs::read_dir(holders).ok()?;
    for entry in entries.flatten() {
        let dm = entry.file_name();
        let dm = dm.to_string_lossy();
        if !dm.starts_with("dm-") {
            continue;
        }
        let uuid_path = Path::new("/sys/block").join(dm.as_ref()).join("dm/uuid");
        let uuid = std::fs::read_to_string(uuid_path).ok()?;
        if !uuid.starts_with("mpath-") {
            continue;
        }
        // Prefer /dev/mapper name from /sys/block/dm-Y/dm/name
        let name_path = Path::new("/sys/block").join(dm.as_ref()).join("dm/name");
        if let Ok(map_name) = std::fs::read_to_string(name_path) {
            let map_name = map_name.trim();
            if !map_name.is_empty() {
                return Some(PathBuf::from(format!("/dev/mapper/{map_name}")));
            }
        }
        return Some(PathBuf::from(format!("/dev/{dm}")));
    }
    None
}
