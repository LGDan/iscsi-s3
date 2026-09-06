//! Layered configuration: defaults < TOML file < env < CLI.

use clap::Parser;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

pub const DEFAULT_BIND: &str = "0.0.0.0:3260";
pub const DEFAULT_BLOCK_SIZE: u32 = 512;
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB
pub const DEFAULT_CACHE_MAX: u64 = 256 * 1024 * 1024; // 256 MiB

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config error: {0}")]
    Figment(#[from] figment::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Host (or host:port) advertised in SendTargets. When unset, the target
    /// uses the socket's local address (often a container/NAT IP).
    #[serde(default)]
    pub advertise: Option<String>,
    #[serde(default)]
    pub s3: S3Config,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    pub bucket: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub force_path_style: bool,
    /// Optional static credentials (otherwise AWS default chain).
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

fn default_region() -> String {
    "us-east-1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_max", with = "bytesize_serde")]
    pub max_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_CACHE_MAX,
        }
    }
}

fn default_cache_max() -> u64 {
    DEFAULT_CACHE_MAX
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Serve Prometheus text on `bind` when true.
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
    /// HTTP listen address for `/metrics` (and `/healthz`).
    #[serde(default = "default_metrics_bind")]
    pub bind: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            bind: default_metrics_bind(),
        }
    }
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_bind() -> String {
    "0.0.0.0:9090".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub name: String,
    pub iqn: String,
    pub prefix: String,
    #[serde(with = "bytesize_serde")]
    pub capacity: u64,
    #[serde(default = "default_block_size")]
    pub block_size: u32,
    #[serde(default = "default_chunk_size", with = "bytesize_serde")]
    pub chunk_size: u64,
}

fn default_block_size() -> u32 {
    DEFAULT_BLOCK_SIZE
}

fn default_chunk_size() -> u64 {
    DEFAULT_CHUNK_SIZE
}

/// CLI overrides (highest precedence). `--config` selects the file layer only.
#[derive(Debug, Clone, Parser, Serialize)]
#[command(name = "iscsi-s3", about = "iSCSI target backed by S3 chunk objects")]
pub struct Cli {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Listen address (host:port) for the shared multi-IQN portal.
    #[arg(long)]
    pub bind: Option<String>,

    /// Address clients should use (host or host:port) in SendTargets.
    #[arg(long)]
    pub advertise: Option<String>,

    /// S3 bucket override
    #[arg(long)]
    pub bucket: Option<String>,

    /// S3 endpoint override (e.g. http://127.0.0.1:9000)
    #[arg(long)]
    pub endpoint: Option<String>,

    /// S3 region override
    #[arg(long)]
    pub region: Option<String>,

    /// Force path-style S3 addressing
    #[arg(long)]
    pub force_path_style: Option<bool>,

    /// Log filter (e.g. info, iscsi_s3=debug)
    #[arg(long, default_value = "info")]
    #[serde(skip)]
    pub log: String,

    /// Prometheus metrics listen address (overrides config when set)
    #[arg(long)]
    pub metrics_bind: Option<String>,

    /// Disable the Prometheus metrics HTTP endpoint
    #[arg(long)]
    pub no_metrics: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    advertise: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s3: Option<CliS3Overrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<CliMetricsOverrides>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CliS3Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    force_path_style: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CliMetricsOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bind: Option<String>,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self, ConfigError> {
        let defaults = Config {
            bind: DEFAULT_BIND.to_string(),
            advertise: None,
            s3: S3Config {
                bucket: None,
                region: default_region(),
                endpoint: None,
                force_path_style: false,
                access_key_id: None,
                secret_access_key: None,
            },
            cache: CacheConfig::default(),
            metrics: MetricsConfig::default(),
            volumes: Vec::new(),
        };

        let mut figment = Figment::new().merge(Serialized::defaults(defaults));

        if let Some(path) = &cli.config {
            figment = figment.merge(Toml::file(path));
        }

        // ISCSI_S3_BIND, ISCSI_S3_S3__ENDPOINT, ISCSI_S3_CACHE__MAX_BYTES, ...
        figment = figment.merge(Env::prefixed("ISCSI_S3_").split("__"));

        let overrides = CliOverrides {
            bind: cli.bind.clone(),
            advertise: cli.advertise.clone(),
            s3: Some(CliS3Overrides {
                bucket: cli.bucket.clone(),
                endpoint: cli.endpoint.clone(),
                region: cli.region.clone(),
                force_path_style: cli.force_path_style,
            }),
            metrics: Some(CliMetricsOverrides {
                enabled: if cli.no_metrics { Some(false) } else { None },
                bind: cli.metrics_bind.clone(),
            }),
        };
        figment = figment.merge(Serialized::defaults(overrides));

        let config: Config = figment.extract()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.volumes.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[volumes]] entry is required (define them in the config file)"
                    .into(),
            ));
        }
        if self.s3.bucket.as_ref().map(|b| b.is_empty()).unwrap_or(true) {
            return Err(ConfigError::Invalid(
                "s3.bucket is required (config file, ISCSI_S3_S3__BUCKET, or --bucket)".into(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        let mut iqns = std::collections::HashSet::new();
        let mut prefixes = std::collections::HashSet::new();
        for vol in &self.volumes {
            if vol.name.is_empty() {
                return Err(ConfigError::Invalid("volume name must not be empty".into()));
            }
            if !names.insert(vol.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate volume name {:?}",
                    vol.name
                )));
            }
            if !iqns.insert(vol.iqn.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate volume iqn {:?}",
                    vol.iqn
                )));
            }
            if !prefixes.insert(vol.prefix.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate volume prefix {:?}",
                    vol.prefix
                )));
            }
            if vol.block_size == 0 || !vol.block_size.is_power_of_two() {
                return Err(ConfigError::Invalid(format!(
                    "volume {}: block_size must be a non-zero power of two",
                    vol.name
                )));
            }
            if vol.chunk_size == 0 || vol.chunk_size % u64::from(vol.block_size) != 0 {
                return Err(ConfigError::Invalid(format!(
                    "volume {}: chunk_size must be a non-zero multiple of block_size",
                    vol.name
                )));
            }
            if vol.capacity == 0 || vol.capacity % u64::from(vol.block_size) != 0 {
                return Err(ConfigError::Invalid(format!(
                    "volume {}: capacity must be a non-zero multiple of block_size",
                    vol.name
                )));
            }
        }
        Ok(())
    }
}

/// Serde helper so TOML can use `"10GiB"` or integer bytes.
mod bytesize_serde {
    use bytesize::ByteSize;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&ByteSize(*bytes).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Size {
            Int(u64),
            Str(String),
        }
        match Size::deserialize(deserializer)? {
            Size::Int(n) => Ok(n),
            Size::Str(s) => s
                .parse::<ByteSize>()
                .map(|b| b.as_u64())
                .map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn file_env_cli_precedence() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
bind = "0.0.0.0:3260"
[s3]
bucket = "from-file"
region = "eu-west-1"
[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local:disk0"
prefix = "disks/disk0"
capacity = "1GiB"
"#
        )
        .unwrap();

        // Env beats file
        std::env::set_var("ISCSI_S3_S3__BUCKET", "from-env");
        let cli = Cli {
            config: Some(file.path().to_path_buf()),
            bind: None,
            advertise: None,
            bucket: None,
            endpoint: None,
            region: None,
            force_path_style: None,
            log: "info".into(),
            metrics_bind: None,
            no_metrics: false,
        };
        let cfg = Config::load(&cli).unwrap();
        assert_eq!(cfg.s3.bucket.as_deref(), Some("from-env"));

        // CLI beats env
        let cli = Cli {
            bucket: Some("from-cli".into()),
            ..cli
        };
        let cfg = Config::load(&cli).unwrap();
        assert_eq!(cfg.s3.bucket.as_deref(), Some("from-cli"));

        std::env::remove_var("ISCSI_S3_S3__BUCKET");
    }

}
