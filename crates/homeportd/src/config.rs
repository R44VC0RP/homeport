//! `server.toml`: shares, port, bind address and allowed logins.
//!
//! ```toml
//! # name = "basebook"            # shown to clients (default: host name)
//! # port = 47820
//! # bind = "100.84.220.87"       # default: this node's tailnet IPv4
//! allow_logins = ["you@example.com"]
//!
//! [[shares]]
//! name = "dev"
//! path = "~/dev"
//! ```
//!
//! Command-line flags override the file; `--share` replaces its shares and
//! `--allow-login` adds to its logins. Without `shares` in the file (and no
//! `--share`), the platform's default shares are used.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("config {path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("config: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_logins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shares: Option<Vec<ShareConfig>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareConfig {
    pub name: String,
    pub path: PathBuf,
}

/// A share name: non-empty, no `/` or `:` (it is part of `root:<name>` ids).
pub fn check_share_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name.contains(':') || name.contains('\\') {
        return Err(format!("share name {name:?} must be non-empty without '/', '\\' or ':'"));
    }
    Ok(())
}

/// Expands a leading `~` to `home`.
pub fn expand(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_owned(),
    }
}

impl Config {
    /// Reads the file; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ConfigError::Io { path: path.display().to_string(), source }),
        };
        let cfg: Self =
            toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })?;
        for s in cfg.shares.iter().flatten() {
            check_share_name(&s.name).map_err(ConfigError::Invalid)?;
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let io_err = |source| ConfigError::Io { path: path.display().to_string(), source };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err)?;
        }
        let text = format!("# homeportd server configuration\n\n{}", toml::to_string_pretty(self)?);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io_err)?;
        std::fs::rename(&tmp, path).map_err(io_err)
    }
}
