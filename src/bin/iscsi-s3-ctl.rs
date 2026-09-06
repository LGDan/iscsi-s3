//! Control client for a running iscsi-s3 daemon (Unix admin socket).

use clap::{Parser, Subcommand, ValueEnum};
use iscsi_s3::admin::call_admin;
use iscsi_s3::config::{parse_byte_size, DEFAULT_ADMIN_SOCKET};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

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
    let req = match build_request(&cli.command) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match call_admin(&sock, &req) {
        Ok(resp) => {
            let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            match cli.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp)
                            .unwrap_or_else(|_| resp.to_string())
                    );
                }
                OutputFormat::Text => {
                    if let Err(e) = print_text(&cli.command, &resp) {
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
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
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
