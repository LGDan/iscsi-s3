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
    /// Ignored when `portals` is non-empty (except as documentation of this
    /// instance's preferred address).
    #[serde(default)]
    pub advertise: Option<String>,
    /// All client-reachable portals for SendTargets (MPIO / multi-instance).
    /// Each entry is `host` or `host:port` (host-only reuses `bind`'s port).
    /// Discovery lists every portal for each IQN so initiators can open
    /// multiple paths. Each iscsi-s3 process still binds only `bind`.
    #[serde(default)]
    pub portals: Vec<String>,
    /// Optional instance label (metrics / logs) for multi-instance deployments.
    #[serde(default)]
    pub instance: Option<String>,
    /// Optional CHAP (and mutual CHAP) defaults for all volumes.
    #[serde(default)]
    pub auth: Option<AuthSettings>,
    #[serde(default)]
    pub s3: S3Config,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub admin: AdminConfig,
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

pub const DEFAULT_ADMIN_SOCKET: &str = "/tmp/iscsi-s3/admin.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// Listen for `iscsi-s3-ctl` on a Unix domain socket.
    #[serde(default = "default_admin_enabled")]
    pub enabled: bool,
    /// Filesystem path for the admin UDS.
    #[serde(default = "default_admin_socket")]
    pub socket: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: default_admin_enabled(),
            socket: default_admin_socket(),
        }
    }
}

fn default_admin_enabled() -> bool {
    true
}

fn default_admin_socket() -> String {
    DEFAULT_ADMIN_SOCKET.to_string()
}

/// Optional CHAP credentials (global or per-volume).
///
/// One-way CHAP when `username` + `secret` are set. Mutual CHAP when
/// `mutual_username` + `mutual_secret` are also both set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthSettings {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    /// Username the target uses when proving identity (mutual CHAP).
    #[serde(default)]
    pub mutual_username: Option<String>,
    #[serde(default)]
    pub mutual_secret: Option<String>,
    /// If set, only these initiator IQNs may login after auth.
    #[serde(default)]
    pub allowed_initiators: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    None,
    Chap,
    MutualChap,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::None => "none",
            AuthMode::Chap => "chap",
            AuthMode::MutualChap => "mutual-chap",
        }
    }
}

/// Resolved auth for one volume (ready for `add_target_with_auth`).
#[derive(Debug, Clone)]
pub struct ResolvedAuth {
    pub config: iscsi_target::AuthConfig,
    pub allowed_initiators: Option<Vec<String>>,
    pub mode: AuthMode,
    /// CHAP username when mode is chap/mutual-chap (safe to log).
    pub username: Option<String>,
}

fn non_empty(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Merge global and per-volume auth (volume fields win when set).
pub fn merge_auth_settings(
    global: &Option<AuthSettings>,
    volume: &Option<AuthSettings>,
) -> Option<AuthSettings> {
    match (global, volume) {
        (None, None) => None,
        (Some(g), None) => Some(g.clone()),
        (None, Some(v)) => Some(v.clone()),
        (Some(g), Some(v)) => Some(AuthSettings {
            username: v.username.clone().or_else(|| g.username.clone()),
            secret: v.secret.clone().or_else(|| g.secret.clone()),
            mutual_username: v
                .mutual_username
                .clone()
                .or_else(|| g.mutual_username.clone()),
            mutual_secret: v
                .mutual_secret
                .clone()
                .or_else(|| g.mutual_secret.clone()),
            allowed_initiators: v
                .allowed_initiators
                .clone()
                .or_else(|| g.allowed_initiators.clone()),
        }),
    }
}

/// Build vendor `AuthConfig` from merged settings.
pub fn resolve_auth(
    global: &Option<AuthSettings>,
    volume: &Option<AuthSettings>,
) -> Result<ResolvedAuth, ConfigError> {
    let merged = merge_auth_settings(global, volume);
    let Some(settings) = merged else {
        return Ok(ResolvedAuth {
            config: iscsi_target::AuthConfig::None,
            allowed_initiators: None,
            mode: AuthMode::None,
            username: None,
        });
    };

    let user = non_empty(&settings.username);
    let secret = non_empty(&settings.secret);
    let mutual_user = non_empty(&settings.mutual_username);
    let mutual_secret = non_empty(&settings.mutual_secret);

    match (user, secret) {
        (None, None) => {
            if mutual_user.is_some() || mutual_secret.is_some() {
                return Err(ConfigError::Invalid(
                    "auth: mutual_username/mutual_secret require username and secret".into(),
                ));
            }
            Ok(ResolvedAuth {
                config: iscsi_target::AuthConfig::None,
                allowed_initiators: settings.allowed_initiators.clone(),
                mode: AuthMode::None,
                username: None,
            })
        }
        (Some(_), None) | (None, Some(_)) => Err(ConfigError::Invalid(
            "auth: username and secret must both be set for CHAP".into(),
        )),
        (Some(username), Some(secret)) => {
            let target_creds =
                iscsi_target::ChapCredentials::new(username.to_string(), secret.to_string());
            let acl = settings.allowed_initiators.clone();
            match (mutual_user, mutual_secret) {
                (None, None) => Ok(ResolvedAuth {
                    config: iscsi_target::AuthConfig::Chap {
                        credentials: target_creds,
                    },
                    allowed_initiators: acl,
                    mode: AuthMode::Chap,
                    username: Some(username.to_string()),
                }),
                (Some(mu), Some(ms)) => Ok(ResolvedAuth {
                    config: iscsi_target::AuthConfig::MutualChap {
                        target_credentials: target_creds,
                        initiator_credentials: iscsi_target::ChapCredentials::new(
                            mu.to_string(),
                            ms.to_string(),
                        ),
                    },
                    allowed_initiators: acl,
                    mode: AuthMode::MutualChap,
                    username: Some(username.to_string()),
                }),
                _ => Err(ConfigError::Invalid(
                    "auth: mutual_username and mutual_secret must both be set for mutual CHAP"
                        .into(),
                )),
            }
        }
    }
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
    /// Optional per-volume CHAP override (inherits unset fields from `[auth]`).
    #[serde(default)]
    pub auth: Option<AuthSettings>,
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
            portals: Vec::new(),
            instance: None,
            auth: None,
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
            admin: AdminConfig::default(),
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

    /// Reload from a config file + current process env (no CLI overrides).
    pub fn load_for_reload(path: &std::path::Path) -> Result<Self, ConfigError> {
        let defaults = Config {
            bind: DEFAULT_BIND.to_string(),
            advertise: None,
            portals: Vec::new(),
            instance: None,
            auth: None,
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
            admin: AdminConfig::default(),
            volumes: Vec::new(),
        };
        let figment = Figment::new()
            .merge(Serialized::defaults(defaults))
            .merge(Toml::file(path))
            .merge(Env::prefixed("ISCSI_S3_").split("__"));
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
            resolve_auth(&self.auth, &vol.auth).map_err(|e| match e {
                ConfigError::Invalid(msg) => {
                    ConfigError::Invalid(format!("volume {}: {}", vol.name, msg))
                }
                other => other,
            })?;
        }
        Ok(())
    }
}

/// Parse a human size (`256MiB`) or decimal integer string into bytes.
pub fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    s.parse::<bytesize::ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| format!("invalid size {s:?}: {e}"))
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

    #[test]
    fn resolve_auth_none() {
        let r = resolve_auth(&None, &None).unwrap();
        assert_eq!(r.mode, AuthMode::None);
        assert!(matches!(r.config, iscsi_target::AuthConfig::None));
    }

    #[test]
    fn resolve_auth_chap() {
        let global = Some(AuthSettings {
            username: Some("u".into()),
            secret: Some("s".into()),
            ..Default::default()
        });
        let r = resolve_auth(&global, &None).unwrap();
        assert_eq!(r.mode, AuthMode::Chap);
        assert_eq!(r.username.as_deref(), Some("u"));
        assert!(matches!(r.config, iscsi_target::AuthConfig::Chap { .. }));
    }

    #[test]
    fn resolve_auth_mutual() {
        let global = Some(AuthSettings {
            username: Some("u".into()),
            secret: Some("s".into()),
            mutual_username: Some("tu".into()),
            mutual_secret: Some("ts".into()),
            allowed_initiators: Some(vec!["iqn.test:1".into()]),
            ..Default::default()
        });
        let r = resolve_auth(&global, &None).unwrap();
        assert_eq!(r.mode, AuthMode::MutualChap);
        assert_eq!(r.allowed_initiators.as_ref().unwrap().len(), 1);
        assert!(matches!(
            r.config,
            iscsi_target::AuthConfig::MutualChap { .. }
        ));
    }

    #[test]
    fn resolve_auth_partial_mutual_errors() {
        let global = Some(AuthSettings {
            username: Some("u".into()),
            secret: Some("s".into()),
            mutual_username: Some("tu".into()),
            mutual_secret: None,
            ..Default::default()
        });
        assert!(resolve_auth(&global, &None).is_err());
    }

    #[test]
    fn resolve_auth_username_without_secret_errors() {
        let global = Some(AuthSettings {
            username: Some("u".into()),
            secret: None,
            ..Default::default()
        });
        assert!(resolve_auth(&global, &None).is_err());
    }

    #[test]
    fn resolve_auth_volume_overrides_global() {
        let global = Some(AuthSettings {
            username: Some("global-u".into()),
            secret: Some("global-s".into()),
            ..Default::default()
        });
        let volume = Some(AuthSettings {
            username: Some("vol-u".into()),
            secret: None, // inherit secret
            ..Default::default()
        });
        let r = resolve_auth(&global, &volume).unwrap();
        assert_eq!(r.mode, AuthMode::Chap);
        assert_eq!(r.username.as_deref(), Some("vol-u"));
    }

    #[test]
    fn config_loads_auth_from_toml() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
bind = "0.0.0.0:3260"
[s3]
bucket = "iscsi"
[auth]
username = "iscsiuser"
secret = "sekrit"
[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local:disk0"
prefix = "disks/disk0"
capacity = "1GiB"
"#
        )
        .unwrap();
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
        let r = resolve_auth(&cfg.auth, &cfg.volumes[0].auth).unwrap();
        assert_eq!(r.mode, AuthMode::Chap);
        assert_eq!(r.username.as_deref(), Some("iscsiuser"));
    }

    #[test]
    fn config_rejects_partial_auth() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
bind = "0.0.0.0:3260"
[s3]
bucket = "iscsi"
[auth]
username = "iscsiuser"
[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local:disk0"
prefix = "disks/disk0"
capacity = "1GiB"
"#
        )
        .unwrap();
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
        assert!(Config::load(&cli).is_err());
    }
}
